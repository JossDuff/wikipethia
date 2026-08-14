//! Any GitHub-hosted repository of markdown files (EIPs, ERCs,
//! consensus-specs), fetched as a snapshot tarball from codeload — no git
//! binary, no history clone. Per-file dates for repos without frontmatter
//! come from GitHub's per-path commit atom feeds, cached in `dates.json`.
//!
//! Raw layout under the data dir:
//!   files/<repo-relative path>.md   the snapshot (paths-filtered)
//!   dates.json                      relpath → ISO date, for dateless repos

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use corpus_core::{CoreError, Document};
use flate2::read::GzDecoder;
use serde_json::{Map, Value};

use crate::error::FetchError;
use crate::sync::{Fetcher, SyncStats, write_atomic_bytes};
use crate::xml;

pub struct RepoAdapter {
    pub source_id: String,
    /// "https://github.com/ethereum/EIPs" — no trailing slash.
    pub repo_url: String,
    pub branch: String,
    /// Repo-relative directories whose matching files are indexed.
    pub paths: Vec<String>,
    /// Extensions to ingest, without dots — `["md"]` for prose repos,
    /// `["py"]` for the executable execution-layer spec.
    pub file_types: Vec<String>,
    /// Canonical-URL template containing `{stem}` (flat repos — doc ids
    /// become `<source>/<stem>`) or `{path}` (nested — `<source>/<relpath>`).
    pub doc_url: String,
    /// data/<source_id>.
    pub data_dir: PathBuf,
    /// Lazy dates.json cache — parse_file runs once per file in the index
    /// loop and must not re-read the JSON each time. Fresh per process, so
    /// a sync in the same run is still observed by a later first read.
    pub dates: std::sync::OnceLock<HashMap<String, String>>,
}

/// The `branch` value that means "whatever this repo's default branch is",
/// rather than a fixed name. Written in sources.toml as `branch = "default"`.
///
/// It exists for `ethereum/execution-specs`, which has no stable branch at
/// all: it names its development branch after the fork in progress
/// (`forks/amsterdam`), has no `master`/`main`, and its `mainnet` branch
/// lags by months. A fixed pin there goes stale at every hard fork, and the
/// failure is silent — the old branch keeps existing, so sync keeps
/// succeeding while the corpus quietly stops learning anything new.
pub const TRACK_DEFAULT: &str = "default";

/// The git ref to fetch, as GitHub URLs want it. Tracking sources use
/// `HEAD`, which GitHub resolves to the default branch for codeload
/// tarballs, commit feeds, and blob URLs alike — so nothing has to be
/// resolved ahead of time or persisted for the offline index step.
fn git_ref(branch: &str) -> &str {
    if branch == TRACK_DEFAULT { "HEAD" } else { branch }
}

/// "https://codeload.github.com/{owner}/{repo}/tar.gz/refs/heads/{branch}",
/// or `tar.gz/HEAD` when tracking the default branch — `refs/heads/HEAD`
/// is not a ref and 404s.
fn tarball_url(repo_url: &str, branch: &str) -> String {
    let ownerrepo = repo_url
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/');
    match git_ref(branch) {
        "HEAD" => format!("https://codeload.github.com/{ownerrepo}/tar.gz/HEAD"),
        r => format!("https://codeload.github.com/{ownerrepo}/tar.gz/refs/heads/{r}"),
    }
}

/// "https://github.com/{owner}/{repo}/commits/{branch}/{relpath}.atom"
fn commits_atom_url(repo_url: &str, branch: &str, relpath: &str) -> String {
    format!(
        "{repo_url}/commits/{}/{}.atom",
        git_ref(branch),
        percent_encode_path(relpath)
    )
}

/// Percent-encode a repo-relative path for use in a URL, leaving the `/`
/// separators intact. ethereum/pm has real filenames like
/// `Meeting 1&2.md` and directories like `(e)PBS` — unencoded, those are
/// rejected outright as invalid URI characters.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// "https://api.github.com/repos/{owner}/{repo}" — for the branch-drift check.
fn repo_api_url(repo_url: &str) -> String {
    let ownerrepo = repo_url
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/');
    format!("https://api.github.com/repos/{ownerrepo}")
}

/// What a file's extension says about how to read it. The pipeline used to
/// infer this from content shape — a `#` line was a markdown heading, a
/// leading `---` was frontmatter — which breaks the moment a repo holds
/// Python (`# ` is a comment) or YAML (`---` is a document marker).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileKind {
    Markdown,
    Python,
}

impl FileKind {
    fn of(relpath: &str) -> Option<Self> {
        match relpath.rsplit_once('.').map(|(_, ext)| ext) {
            Some("md") => Some(Self::Markdown),
            Some("py") => Some(Self::Python),
            _ => None,
        }
    }
}

impl RepoAdapter {
    fn files_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }

    fn dates_path(&self) -> PathBuf {
        self.data_dir.join("dates.json")
    }

    fn archive_err(&self, detail: impl Into<String>) -> FetchError {
        FetchError::Archive {
            source_id: self.source_id.clone(),
            detail: detail.into(),
        }
    }

    fn load_dates(&self) -> HashMap<String, String> {
        fs::read_to_string(self.dates_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_dates(&self, dates: &HashMap<String, String>) -> Result<(), FetchError> {
        let value = Value::Object(
            dates
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<Map<_, _>>(),
        );
        write_atomic_bytes(&self.dates_path(), &serde_json::to_vec_pretty(&value)?)
    }

    /// Whether a repo-relative path is ours to ingest. MUST agree with
    /// `raw_files` — they gate independently, and a mismatch either strands
    /// files on disk that are never indexed, or makes the deletion pass
    /// treat synced files as removed upstream and wipe them.
    fn wanted(&self, relpath: &str) -> bool {
        self.has_wanted_extension(relpath)
            && self
                .paths
                .iter()
                .any(|p| relpath.starts_with(&format!("{p}/")))
    }

    fn has_wanted_extension(&self, relpath: &str) -> bool {
        relpath
            .rsplit_once('.')
            .is_some_and(|(_, ext)| self.file_types.iter().any(|want| want == ext))
    }

    /// Warn when the pinned branch is no longer the repo's default.
    ///
    /// execution-specs names its development branches after the fork in
    /// progress (`forks/amsterdam`), so a pin goes stale every hard fork —
    /// and the dangerous failure is silent: a frozen-but-existing branch
    /// keeps syncing successfully while never gaining another commit. This
    /// converts that into one visible line. Best-effort by design: a failed
    /// or rate-limited check is skipped, never fatal.
    fn warn_if_branch_drifted(&self, fetcher: &mut dyn Fetcher) {
        let Ok(body) = fetcher.get_text(&repo_api_url(&self.repo_url)) else {
            return;
        };
        let Some(default) = json_string_field(&body, "default_branch") else {
            return;
        };
        if self.branch == TRACK_DEFAULT {
            // No drift is possible — but say which branch that resolved to,
            // so the sync log records what was actually ingested.
            eprintln!("sync {}: tracking default branch {default:?}", self.source_id);
        } else if default != self.branch {
            eprintln!(
                "warn: {} pins branch {:?} but {}'s default is now {:?} — the pin may be \
                 frozen; set `branch = \"default\"` to track it, or update the pin",
                self.source_id, self.branch, self.repo_url, default
            );
        }
    }

    /// The interesting tail of a path: what remains after the configured
    /// `paths` prefix (`src/ethereum/forks/cancun/fork` → `cancun/fork`).
    fn strip_configured_prefix<'a>(&self, id_path: &'a str) -> Option<&'a str> {
        self.paths
            .iter()
            .find_map(|p| id_path.strip_prefix(&format!("{p}/")))
            .filter(|s| !s.is_empty())
    }

    fn relpath_of(&self, path: &Path) -> String {
        path.strip_prefix(self.files_dir())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

impl crate::Adapter for RepoAdapter {
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError> {
        fn walk(
            dir: &Path,
            wanted: &dyn Fn(&Path) -> bool,
            out: &mut Vec<PathBuf>,
        ) -> std::io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    walk(&path, wanted, out)?;
                } else if wanted(&path) {
                    out.push(path);
                }
            }
            Ok(())
        }
        let files_dir = self.files_dir();
        let mut paths = Vec::new();
        // Must agree with `wanted` on BOTH axes — extension and the paths
        // prefix. Extension-only here would re-index a tree that was
        // dropped from `paths` until the next full sync pruned it.
        let keep = |path: &Path| self.wanted(&self.relpath_of(path));
        walk(&files_dir, &keep, &mut paths).map_err(|source| FetchError::Io {
            path: files_dir,
            source,
        })?;
        paths.sort();
        Ok(paths)
    }

    /// One tarball request; commit-feed requests only for files that need a
    /// date (no frontmatter `created:`, no fresh dates.json entry). The
    /// dates pass is driven from disk state, so an interrupted run resumes
    /// where it stopped — dates.json persists after every fetch.
    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        limit: Option<usize>,
    ) -> Result<SyncStats, FetchError> {
        // The tarball is one silent request that can take minutes for
        // asset-heavy repos — without this line, that whole window is
        // indistinguishable from a hang (measured: eips took 6m21s on a
        // rate-limited server with no output at all).
        self.warn_if_branch_drifted(fetcher);
        eprintln!(
            "sync {}: downloading {} tarball ({})…",
            self.source_id,
            if self.branch == TRACK_DEFAULT { "default-branch" } else { &self.branch },
            self.repo_url
        );
        let bytes = fetcher.get_bytes(&tarball_url(&self.repo_url, &self.branch))?;
        eprintln!(
            "sync {}: tarball {:.1}MB, extracting…",
            self.source_id,
            bytes.len() as f64 / 1_048_576.0
        );
        let mut archive = tar::Archive::new(GzDecoder::new(bytes.as_slice()));

        let mut stats = SyncStats::default();
        let mut kept: HashSet<String> = HashSet::new();
        let mut dirty: Vec<String> = Vec::new();
        for entry in archive
            .entries()
            .map_err(|e| self.archive_err(format!("reading archive: {e}")))?
        {
            let mut entry = entry.map_err(|e| self.archive_err(format!("bad entry: {e}")))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let entry_path = entry
                .path()
                .map_err(|e| self.archive_err(format!("bad entry path: {e}")))?
                .into_owned();
            // Tarball entries live under a "<repo>-<ref>/" prefix.
            let mut components = entry_path.components();
            components.next();
            let relpath = components.as_path().to_string_lossy().replace('\\', "/");
            if !self.wanted(&relpath) {
                continue;
            }
            if limit.is_some_and(|l| kept.len() >= l) {
                break;
            }
            kept.insert(relpath.clone());

            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .map_err(|e| self.archive_err(format!("reading {relpath}: {e}")))?;
            let dest = self.files_dir().join(&relpath);
            if fs::read(&dest).is_ok_and(|existing| existing == contents) {
                stats.skipped += 1;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|source| FetchError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            write_atomic_bytes(&dest, &contents)?;
            stats.fetched += 1;
            eprintln!("fetch {relpath}");
            dirty.push(relpath);
        }

        let mut dates = self.load_dates();

        // Deletion pass: upstream removals and renames. Skipped under
        // --limit, which sees only a partial view of the repo.
        if limit.is_none() {
            for path in self.raw_files().unwrap_or_default() {
                let relpath = self.relpath_of(&path);
                if !kept.contains(&relpath) {
                    let _ = fs::remove_file(&path);
                    dates.remove(&relpath);
                    eprintln!("prune {relpath} (removed upstream)");
                }
            }
            self.save_dates(&dates)?;
        }

        // Dates pass: every file on disk that has neither a frontmatter
        // `created:` nor a dates.json entry (EIPs/ERCs never need one;
        // consensus-specs need one each). Driven from disk state, not this
        // run's dirty set, so an interrupted first sync resumes exactly
        // where it stopped instead of permanently losing the tail. Files
        // whose content changed this run get their entry invalidated first —
        // their last-commit date moved.
        for relpath in &dirty {
            dates.remove(relpath);
        }
        let pending: Vec<String> = self
            .raw_files()
            .unwrap_or_default()
            .iter()
            .map(|path| self.relpath_of(path))
            .filter(|relpath| {
                if dates.contains_key(relpath) {
                    return false;
                }
                let content =
                    fs::read_to_string(self.files_dir().join(relpath)).unwrap_or_default();
                // Frontmatter is a markdown convention; probing a .py file
                // for it is meaningless. A body-stated date also settles
                // the question without a request.
                if FileKind::of(relpath) == Some(FileKind::Markdown)
                    && (frontmatter(&content).is_some_and(|fm| fm.contains_key("created"))
                        || body_date(&content).is_some())
                {
                    return false;
                }
                true
            })
            .collect();
        if !pending.is_empty() {
            eprintln!(
                "sync {}: filling commit dates for {} files (~{}s at 1 request/s)",
                self.source_id,
                pending.len(),
                pending.len()
            );
        }
        for relpath in pending {
            let atom = fetcher.get_text(&commits_atom_url(&self.repo_url, &self.branch, &relpath))?;
            let Some(entry) = xml::blocks(&atom, "entry").into_iter().next() else {
                eprintln!("warn: no commit entries for {relpath} — published stays empty");
                continue;
            };
            if let Some(updated) = xml::tag_text(entry, "updated") {
                dates.insert(relpath, updated);
                // Persist after every fetch so an interrupted first sync
                // (~150 files at 1 rps) resumes without refetching.
                self.save_dates(&dates)?;
            }
        }
        Ok(stats)
    }

    fn parse_file(&self, path: &Path) -> Result<Vec<Document>, CoreError> {
        let content = fs::read_to_string(path)
            .map_err(|e| CoreError::Parse(format!("reading {}: {e}", path.display())))?;
        let relpath = self.relpath_of(path);
        let stem = Path::new(&relpath)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| relpath.clone());

        let doc = if let Some(fm) = frontmatter(&content) {
            // "Moved" tombstones (EIPs relocated to the ERCs repo) are
            // one-line redirects with no research content; their targets
            // are indexed by the other source. Skip.
            if fm.get("status").is_some_and(|s| s == "Moved") {
                return Ok(Vec::new());
            }
            // EIP/ERC style: everything worth knowing is in the frontmatter.
            // Both repos use the `eip:` frontmatter key (the ERCs repo kept
            // it after the 2023 split); the designator lives in the FILE
            // NAME: erc-1046.md vs eip-4844.md. Titles must carry the exact
            // token users search ("ERC-4337").
            let number = fm.get("eip").or_else(|| fm.get("erc"));
            let designator = if stem.starts_with("erc") { "ERC" } else { "EIP" };
            let title = match (number, fm.get("title")) {
                (Some(n), Some(t)) => format!("{designator}-{n}: {t}"),
                (_, Some(t)) => t.clone(),
                _ => stem.clone(),
            };
            let body = body_after_frontmatter(&content);
            let content_text = match fm.get("description") {
                Some(desc) => format!("{desc}\n\n{body}"),
                None => body.to_string(),
            };
            let mut meta = Map::new();
            let tags: Vec<Value> = ["status", "type", "category"]
                .iter()
                .filter_map(|k| fm.get(*k))
                .map(|v| Value::String(v.clone()))
                .collect();
            if !tags.is_empty() {
                meta.insert("tags".into(), Value::Array(tags));
            }
            for key in ["status", "discussions-to"] {
                if let Some(v) = fm.get(key) {
                    meta.insert(key.replace('-', "_"), Value::String(v.clone()));
                }
            }
            Document {
                id: format!("{}/{stem}", self.source_id),
                source: self.source_id.clone(),
                url: render_url(&self.doc_url, &stem, &relpath),
                title,
                author: fm.get("author").cloned(),
                published: fm
                    .get("created")
                    .map(|d| format!("{d}T00:00:00Z"))
                    .unwrap_or_default(),
                content: content_text,
                meta,
            }
        } else {
            // Markdown ids drop the extension (`.../beacon-chain`, long
            // established); other types keep it, which is both honest and
            // the discriminator lookup_spec uses to read a .py document as
            // Python rather than as prose quoting Python.
            let id_path = relpath.strip_suffix(".md").unwrap_or(&relpath);
            let kind = FileKind::of(&relpath).unwrap_or(FileKind::Markdown);
            // For Python, the path tail carries the fork and module name —
            // `amsterdam/bloom.py`, not the full `src/ethereum/forks/…`.
            let short = self.strip_configured_prefix(id_path).unwrap_or(id_path);
            let title = match (title_for(kind, &content), kind) {
                // EELS docstrings never name their fork, so all 24
                // `fork.py` files claim the title "Ethereum
                // Specification" — and the per-(source, title) search cap
                // would then hide 22 of them. Qualifying by path makes each
                // unique and puts the fork name in the title field, which
                // BM25 weights heavily.
                (Some(title), FileKind::Python) => format!("{title} — {short}"),
                (Some(title), _) => title,
                // Many EELS modules open with a wrapped paragraph rather
                // than a title line; the path beats a mid-sentence
                // fragment.
                (None, FileKind::Python) => short.to_string(),
                (None, _) => id_path.to_string(),
            };
            // A date in the body (meeting notes) beats the commit date,
            // which moves whenever the file is touched — a 2020 decision
            // re-stamped 2026 would silently corrupt supersession
            // reasoning, the one thing every citation is trusted for.
            let published = body_date(&content)
                .or_else(|| {
                    self.dates
                        .get_or_init(|| self.load_dates())
                        .get(&relpath)
                        .cloned()
                })
                .unwrap_or_default();
            if published.is_empty() {
                eprintln!("warn: {relpath} has no recorded date — run sync to fill dates.json");
            }
            Document {
                id: format!("{}/{id_path}", self.source_id),
                source: self.source_id.clone(),
                url: render_url(&self.doc_url, &stem, &relpath),
                title,
                author: None,
                published,
                content,
                meta: Map::new(),
            }
        };
        Ok(vec![doc])
    }
}

/// `{stem}` = filename sans .md; `{path}` = the FULL repo-relative path,
/// extension included — a GitHub blob URL without .md 404s, and both parse
/// branches must agree or one class of repo gets dead citations.
fn render_url(template: &str, stem: &str, relpath: &str) -> String {
    // Encoded for the same reason the atom URL is: ethereum/pm has real
    // filenames like `Meeting 1&2.md` under `(e)PBS/`. An unencoded space
    // truncates the link in any markdown citation, which breaks the
    // retrieval invariant that every result carries a CITABLE url.
    template
        .replace("{stem}", &percent_encode_path(stem))
        .replace("{path}", &percent_encode_path(relpath))
}

/// One string field out of a flat JSON object, without pulling the whole
/// document into a Value — the branch check needs exactly one key and must
/// never fail the sync it is advising.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = body.split_once(&needle)?.1;
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// A document title read the way its file type means it. Markdown's `# `
/// heading and Python's module docstring look nothing alike, and reading a
/// Python file the markdown way titles it after its first comment —
/// "ruff: noqa" or a copyright line.
fn title_for(kind: FileKind, content: &str) -> Option<String> {
    match kind {
        FileKind::Markdown => content
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        FileKind::Python => module_docstring_title(content),
    }
}

/// The first prose line of a leading module docstring. EELS opens every
/// file with one ("Ethereum Virtual Machine (EVM) Interpreter."), followed
/// by reST boilerplate (`.. contents::`, underlines) that is not a title.
fn module_docstring_title(content: &str) -> Option<String> {
    let mut lines = content.lines().skip_while(|l| l.trim().is_empty());
    let first = lines.next()?.trim();
    let quote = ["\"\"\"", "'''"].into_iter().find(|q| first.starts_with(q))?;
    // The title may sit on the opening line or the one after it.
    let inline = first.trim_start_matches(quote).trim();
    let (candidate, closes_line) = if inline.is_empty() {
        (lines.next()?.trim().to_string(), false)
    } else {
        (inline.to_string(), inline.ends_with(quote))
    };
    // Many EELS docstrings open with a WRAPPED PARAGRAPH, not a title:
    // "The Amsterdam fork ([EIP-7773]) includes block-level access lists
    // and the\ndeterministic…". Taking line one then yields a
    // mid-sentence fragment as the document's title — and titles are both
    // BM25-weighted and the visible label on every citation. A real title
    // stands alone: the next line is blank, an underline, or the
    // docstring's end.
    if !closes_line {
        let next = lines.next().map(str::trim).unwrap_or("");
        let standalone = next.is_empty()
            || next.starts_with(quote)
            || next.starts_with("..")
            || next.chars().all(|c| "-=~^\"'`#*+".contains(c));
        if !standalone {
            return None;
        }
    }
    let candidate = candidate.trim_end_matches(quote).trim();
    // reST directives and underline rules are structure, not titles.
    if candidate.is_empty()
        || candidate.starts_with("..")
        || candidate.chars().all(|c| "-=~^\"'`#*+".contains(c))
    {
        return None;
    }
    Some(candidate.trim_end_matches('.').to_string())
}

/// A date stated in a document's own prose, e.g. ethereum/pm's
/// `### Meeting Date/Time: Friday 4 Sept 2020, 14:00 UTC`. Returns an
/// ISO-8601 timestamp. Only the first 30 lines are scanned, and only
/// labelled lines count, so this stays quiet on documents that state no
/// date (it must no-op for every source that predates ethereum/pm).
fn body_date(content: &str) -> Option<String> {
    // Labels observed across ethereum/pm: "Meeting Date/Time:",
    // "Date & Time:", "**Date**:", plain "Date:". Match any line that
    // mentions a date and yields a parseable one — requiring the date to
    // parse is what keeps prose mentions from being mistaken for the
    // document's own date.
    content.lines().take(30).find_map(|line| {
        // The word must be a LABEL — "date" followed closely by a colon —
        // not merely present. Matching the substring alone reads "Speccing
        // Updates" in a table row as a date field and then harvests the
        // row number as a day, which put a fabricated 2025-12-13 on a
        // document whose own text says TBD.
        let lower = line.to_ascii_lowercase();
        let at = lower.find("date")?;
        let after = &line[at + "date".len()..];
        let colon = after.find(':').filter(|i| *i <= 20)?;
        parse_loose_date(&after[colon + 1..])
    })
}

/// `2023/3/9`, `2023-03-09`, or `4 Sept 2020` → `YYYY-MM-DDT00:00:00Z`.
/// Deliberately narrow: a wrong date is worse than no date, so anything
/// unrecognized falls through to the commit-date cache.
fn parse_loose_date(text: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let cleaned: String = text
        .chars()
        .map(|c| if c == ',' { ' ' } else { c })
        .collect();
    // Strip surrounding punctuation: real lines wrap the date in markdown
    // and links ("**Date & Time**: [Aug 16, 2024, …](url)"), so a bare
    // whitespace split yields "[Aug" and never matches a month.
    let tokens: Vec<&str> = cleaned
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|t| !t.is_empty())
        .collect();

    for token in &tokens {
        let parts: Vec<&str> = token.split(['/', '-']).collect();
        if parts.len() != 3 || !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        // `continue`, never `?`: an unparseable token (an overlong digit
        // run) must skip that token, not abandon a good date elsewhere on
        // the same line.
        let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse::<u32>().ok()).collect();
        let Some(nums) = nums else { continue };
        // ISO-ish: 2023/3/9 or 2023-03-09 — year first, unambiguous.
        if parts[0].len() == 4 {
            let (y, m, d) = (nums[0], nums[1], nums[2]);
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                return Some(format!("{y}-{m:02}-{d:02}T00:00:00Z"));
            }
        }
        // US M/D/YY, the early AllCoreDevs convention ("Friday 7/14/17").
        // Two-digit years are 20xx: these notes start in 2015.
        if parts[2].len() == 2 {
            let (m, d, y) = (nums[0], nums[1], nums[2]);
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                return Some(format!("20{y:02}-{m:02}-{d:02}T00:00:00Z"));
            }
        }
    }

    // Textual: "4 Sept 2020" / "Sept 4 2020", in either order.
    let month = tokens.iter().enumerate().find_map(|(i, t)| {
        let lower = t.to_ascii_lowercase();
        MONTHS
            .iter()
            .position(|m| lower.starts_with(m))
            .map(|m| (i, m as u32 + 1))
    })?;
    let year = tokens.iter().find_map(|t| {
        t.parse::<u32>().ok().filter(|y| (1970..=2999).contains(y))
    })?;
    let day = tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != month.0)
        .find_map(|(_, t)| {
            t.trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .parse::<u32>()
                .ok()
                .filter(|d| (1..=31).contains(d))
        })?;
    Some(format!("{year}-{:02}-{day:02}T00:00:00Z", month.1))
}

/// Flat `key: value` frontmatter between `---` lines; None when the file
/// doesn't open with one, or the block never closes.
fn frontmatter(content: &str) -> Option<HashMap<String, String>> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let mut map = HashMap::new();
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                map.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    Some(map)
}

fn body_after_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---") else {
        return content;
    };
    match rest.find("\n---") {
        Some(end) => {
            let after = &rest[end + 4..];
            after.trim_start_matches(['\r', '\n'])
        }
        None => content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_kind_comes_from_the_extension() {
        assert_eq!(FileKind::of("a/b/c.md"), Some(FileKind::Markdown));
        assert_eq!(FileKind::of("src/ethereum/forks/cancun/fork.py"), Some(FileKind::Python));
        assert_eq!(FileKind::of("apis/beacon/genesis.yaml"), None);
        assert_eq!(FileKind::of("LICENSE"), None);
    }

    #[test]
    fn python_titles_come_from_the_module_docstring() {
        // The EELS shape: docstring, title line, then reST scaffolding.
        let eels = "\"\"\"\nEthereum Virtual Machine (EVM) Interpreter.\n\n\
                    .. contents:: Table of Contents\n    :backlinks: none\n\n\
                    Introduction\n------------\n\nA straightforward interpreter.\n\"\"\"\n\n\
                    def process_message(x):\n    pass\n";
        assert_eq!(
            title_for(FileKind::Python, eels).as_deref(),
            Some("Ethereum Virtual Machine (EVM) Interpreter")
        );
        // Title on the opening line is equally valid.
        assert_eq!(
            title_for(FileKind::Python, "\"\"\"Fork transition logic.\"\"\"\n").as_deref(),
            Some("Fork transition logic")
        );
        // Reading a .py the markdown way titles it after its first comment;
        // the whole point of dispatching on file type is to not do that.
        let commented = "# ruff: noqa\n\"\"\"\nReal Title.\n\"\"\"\n";
        assert_eq!(title_for(FileKind::Markdown, commented).as_deref(), Some("ruff: noqa"));
        assert_eq!(title_for(FileKind::Python, commented), None);
        // No docstring at all: caller falls back to the path.
        assert_eq!(title_for(FileKind::Python, "import sys\n"), None);
        // reST directives and underlines are structure, not titles.
        assert_eq!(title_for(FileKind::Python, "\"\"\"\n.. module:: x\n\"\"\"\n"), None);
        assert_eq!(title_for(FileKind::Python, "\"\"\"\n------\n\"\"\"\n"), None);
    }

    #[test]
    fn body_dates_parse_the_formats_ethereum_pm_actually_uses() {
        // Both formats observed across AllCoreDevs EL and CL notes.
        let el = "# All Core Devs Meeting 95 Notes\n\
                  ### Meeting Date/Time: Friday 4 Sept 2020, 14:00 UTC\n";
        assert_eq!(body_date(el).as_deref(), Some("2020-09-04T00:00:00Z"));
        let cl = "# Consensus Layer Call 104\n\n\
                  ### Meeting Date/Time: Thursday 2023/3/9 at 14:00 UTC\n";
        assert_eq!(body_date(cl).as_deref(), Some("2023-03-09T00:00:00Z"));
        // ISO with dashes, and a plain "Date:" label.
        assert_eq!(
            body_date("### Date: 2024-01-11\n").as_deref(),
            Some("2024-01-11T00:00:00Z")
        );
        // US M/D/YY — the early AllCoreDevs convention.
        assert_eq!(
            body_date("### Meeting Date/Time: Friday 7/14/17 at 14:00 UTC\n").as_deref(),
            Some("2017-07-14T00:00:00Z")
        );
        // Breakout rooms label it differently and bracket the value.
        assert_eq!(
            body_date("**Date & Time**: [Aug 16, 2024, 14:00-15:00 UTC](https://x)\n").as_deref(),
            Some("2024-08-16T00:00:00Z")
        );
        // A month with no year anywhere is genuinely unparseable — better
        // to fall through than to invent a year.
        assert_eq!(body_date("### Meeting Date/Time: Friday 6 March at 14:00 UTC\n"), None);
        // Must stay silent on every source that predates ethereum/pm —
        // a wrong date is worse than none, since citations are trusted.
        assert_eq!(body_date("# Electra -- The Beacon Chain\n\nSome prose.\n"), None);
        assert_eq!(body_date("### Meeting Date/Time: TBD\n"), None);
        // Only the head of the file is scanned.
        let late = format!("{}### Meeting Date/Time: 2020/1/1\n", "filler\n".repeat(40));
        assert_eq!(body_date(&late), None);
    }

    #[test]
    fn loose_dates_reject_what_they_cannot_read() {
        assert_eq!(parse_loose_date(" 2023/12/31 "), Some("2023-12-31T00:00:00Z".into()));
        assert_eq!(parse_loose_date("Sept 4 2020"), Some("2020-09-04T00:00:00Z".into()));
        assert_eq!(parse_loose_date("14:00 UTC"), None);
        assert_eq!(parse_loose_date("2023/13/45"), None);
        assert_eq!(parse_loose_date(""), None);
    }

    #[test]
    fn tracking_the_default_branch_uses_head_everywhere() {
        // GitHub resolves HEAD to the default branch for codeload
        // tarballs and commit feeds alike (verified against the live
        // endpoints), so nothing has to be resolved before fetching.
        assert_eq!(
            tarball_url("https://github.com/ethereum/execution-specs", TRACK_DEFAULT),
            "https://codeload.github.com/ethereum/execution-specs/tar.gz/HEAD"
        );
        assert_eq!(
            commits_atom_url("https://github.com/ethereum/execution-specs", TRACK_DEFAULT, "src/a.py"),
            "https://github.com/ethereum/execution-specs/commits/HEAD/src/a.py.atom"
        );
        // `refs/heads/HEAD` is not a ref — the tracking case must not go
        // through the pinned-branch URL shape.
        assert!(!tarball_url("https://github.com/o/r", TRACK_DEFAULT).contains("refs/heads"));
        // Pinned branches are untouched, slashes and all.
        assert_eq!(
            tarball_url("https://github.com/ethereum/execution-specs", "forks/amsterdam"),
            "https://codeload.github.com/ethereum/execution-specs/tar.gz/refs/heads/forks/amsterdam"
        );
        assert_eq!(
            commits_atom_url("https://github.com/ethereum/pm", "master", "Meeting 1.md"),
            "https://github.com/ethereum/pm/commits/master/Meeting%201.md.atom"
        );
    }

    #[test]
    fn atom_urls_encode_paths_that_are_legal_filenames_but_illegal_urls() {
        // Real ethereum/pm paths: spaces, ampersands, parentheses.
        assert_eq!(
            commits_atom_url("https://github.com/ethereum/pm", "master", "AllCoreDevs-EL-Meetings/Meeting 1&2.md"),
            "https://github.com/ethereum/pm/commits/master/AllCoreDevs-EL-Meetings/Meeting%201%262.md.atom"
        );
        assert_eq!(
            percent_encode_path("Breakout-Room-Meetings/(e)PBS/Meeting 02.md"),
            "Breakout-Room-Meetings/%28e%29PBS/Meeting%2002.md"
        );
        // Ordinary paths are untouched — separators and unreserved chars.
        assert_eq!(
            percent_encode_path("specs/electra/beacon-chain.md"),
            "specs/electra/beacon-chain.md"
        );
        // Non-ASCII encodes per UTF-8 byte.
        assert_eq!(percent_encode_path("caf\u{e9}.md"), "caf%C3%A9.md");
    }

    #[test]
    fn default_branch_is_read_without_a_json_parser() {
        let body = r#"{"id":1,"name":"execution-specs","default_branch":"forks/amsterdam"}"#;
        assert_eq!(json_string_field(body, "default_branch").as_deref(), Some("forks/amsterdam"));
        assert_eq!(json_string_field(body, "missing"), None);
        assert_eq!(json_string_field("not json", "default_branch"), None);
    }
}
