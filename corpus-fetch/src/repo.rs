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
    /// Repo-relative directories whose .md files are indexed.
    pub paths: Vec<String>,
    /// Canonical-URL template containing `{stem}` (flat repos — doc ids
    /// become `<source>/<stem>`) or `{path}` (nested — `<source>/<relpath>`).
    pub doc_url: String,
    /// data/<source_id>.
    pub data_dir: PathBuf,
}

/// "https://codeload.github.com/{owner}/{repo}/tar.gz/refs/heads/{branch}"
fn tarball_url(repo_url: &str, branch: &str) -> String {
    let ownerrepo = repo_url
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/');
    format!("https://codeload.github.com/{ownerrepo}/tar.gz/refs/heads/{branch}")
}

/// "https://github.com/{owner}/{repo}/commits/{branch}/{relpath}.atom"
fn commits_atom_url(repo_url: &str, branch: &str, relpath: &str) -> String {
    format!("{repo_url}/commits/{branch}/{relpath}.atom")
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

    fn wanted(&self, relpath: &str) -> bool {
        relpath.ends_with(".md")
            && self
                .paths
                .iter()
                .any(|p| relpath.starts_with(&format!("{p}/")))
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
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    walk(&path, out)?;
                } else if path.extension().is_some_and(|ext| ext == "md") {
                    out.push(path);
                }
            }
            Ok(())
        }
        let files_dir = self.files_dir();
        let mut paths = Vec::new();
        walk(&files_dir, &mut paths).map_err(|source| FetchError::Io {
            path: files_dir,
            source,
        })?;
        paths.sort();
        Ok(paths)
    }

    /// One tarball request; atom-feed requests only for changed files that
    /// carry no frontmatter date. Interruptible: dates.json persists after
    /// every fetch, and unchanged files are skipped byte-identically.
    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        limit: Option<usize>,
    ) -> Result<SyncStats, FetchError> {
        let bytes = fetcher.get_bytes(&tarball_url(&self.repo_url, &self.branch))?;
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

        // Dates pass: only changed files with no frontmatter `created:`
        // need a commit-feed lookup (EIPs/ERCs never do; consensus-specs
        // always do, once per changed file).
        for relpath in dirty {
            let content = fs::read_to_string(self.files_dir().join(&relpath)).unwrap_or_default();
            if frontmatter(&content).is_some_and(|fm| fm.contains_key("created")) {
                continue;
            }
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
            // EIP/ERC style: everything worth knowing is in the frontmatter.
            let number = fm.get("eip").or_else(|| fm.get("erc"));
            let designator = if fm.contains_key("erc") { "ERC" } else { "EIP" };
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
                url: self.doc_url.replace("{stem}", &stem).replace(
                    "{path}",
                    relpath.strip_suffix(".md").unwrap_or(&relpath),
                ),
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
            // Spec style: no frontmatter, date from the commit-feed cache.
            let id_path = relpath.strip_suffix(".md").unwrap_or(&relpath);
            let title = content
                .lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or(id_path)
                .to_string();
            let published = self.load_dates().get(&relpath).cloned().unwrap_or_default();
            if published.is_empty() {
                eprintln!("warn: {relpath} has no recorded date — run sync to fill dates.json");
            }
            Document {
                id: format!("{}/{id_path}", self.source_id),
                source: self.source_id.clone(),
                url: self
                    .doc_url
                    .replace("{stem}", &stem)
                    .replace("{path}", &relpath),
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
