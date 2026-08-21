//! sources.toml — the manifest of corpus sources. Adding a source of an
//! existing kind must be an edit here and nothing else (the M6 gate).
//!
//! Parsing is two-stage: a flat `RawSource` with every per-kind field
//! optional (so `deny_unknown_fields` keeps working — serde's tagged enums
//! silently drop it), then per-kind validation into the typed
//! [`SourceSpec`].

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use wikipethia_fetch::{Adapter, DiscourseAdapter, FeedAdapter, RepoAdapter};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    sources: Vec<RawSource>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct RawSource {
    id: String,
    kind: Kind,
    url: String,
    tier: String,
    // repo-only fields — validated per kind below.
    branch: Option<String>,
    paths: Option<Vec<String>>,
    doc_url: Option<String>,
    /// Extensions to ingest, without dots. Defaults to `["md"]`, which is
    /// every source that predates execution-specs' Python.
    file_types: Option<Vec<String>>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Discourse,
    Repo,
    Feed,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Discourse => "discourse",
            Kind::Repo => "repo",
            Kind::Feed => "feed",
        }
    }

    /// Kinds that run an open-ended crawl loop against their host. The
    /// per-host politeness rule is enforced by host-uniqueness for these;
    /// repo sources make a bounded handful of requests and are exempt.
    fn crawls(self) -> bool {
        matches!(self, Kind::Discourse | Kind::Feed)
    }
}

#[derive(Debug)]
pub struct Manifest {
    pub sources: Vec<Source>,
}

#[derive(Debug)]
pub struct Source {
    /// Directory under data/ and the doc-id prefix — stable forever.
    pub id: String,
    pub kind: Kind,
    pub url: String,
    /// Opaque source-quality label carried on every search result.
    pub tier: String,
    pub spec: SourceSpec,
}

#[derive(Debug)]
pub enum SourceSpec {
    Discourse,
    Repo {
        branch: String,
        paths: Vec<String>,
        doc_url: String,
        /// Extensions without dots, never empty (defaulted to `["md"]`).
        file_types: Vec<String>,
    },
    Feed,
}

impl Manifest {
    pub fn load() -> anyhow::Result<Self> {
        let text = fs::read_to_string("sources.toml").context(
            "reading sources.toml — the manifest is committed at the repo root; \
             run from there",
        )?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: RawManifest = toml::from_str(text).context("parsing sources.toml")?;
        if raw.sources.is_empty() {
            bail!("sources.toml: no [[sources]] entries");
        }
        let mut sources: Vec<Source> = Vec::with_capacity(raw.sources.len());
        for raw_source in raw.sources {
            let source = validate(raw_source)?;
            if sources.iter().any(|s| s.id == source.id) {
                bail!("sources.toml: duplicate source id {:?}", source.id);
            }
            if sources.iter().any(|s| s.url == source.url) {
                bail!("sources.toml: duplicate source url {:?}", source.url);
            }
            // The "one request per second per host" hard rule (CLAUDE.md) is
            // implemented as one sequential rate-limited client per SOURCE —
            // per-host only if no two CRAWLING sources share a host. Repo
            // sources (one tarball + a bounded set of commit-feed requests,
            // through the same client) are exempt: github.com hosting several
            // repos is the normal case, and the worst effect is a single
            // sub-second boundary between two sources' bounded request runs.
            if source.kind.crawls()
                && sources
                    .iter()
                    .any(|s| s.kind.crawls() && host(&s.url) == host(&source.url))
            {
                bail!(
                    "sources.toml: {:?} shares a host with an earlier crawling source — \
                     two crawls on one host would each get their own rate limiter and \
                     break the one-request-per-second-per-host rule",
                    source.id
                );
            }
            sources.push(source);
        }
        Ok(Manifest { sources })
    }

    /// All sources, or just `id` — unknown ids error listing what exists.
    pub fn select(&self, id: Option<&str>) -> anyhow::Result<Vec<&Source>> {
        match id {
            None => Ok(self.sources.iter().collect()),
            Some(id) => match self.sources.iter().find(|s| s.id == id) {
                Some(source) => Ok(vec![source]),
                None => bail!(
                    "unknown source {id:?} — sources.toml defines: {}",
                    self.sources
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        }
    }
}

fn validate(raw: RawSource) -> anyhow::Result<Source> {
    let ok_id = !raw.id.is_empty()
        && raw
            .id
            .starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && raw
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok_id {
        bail!(
            "sources.toml: id {:?} must be [a-z0-9_-] and start alphanumeric \
             (it is a directory name and a doc-id prefix)",
            raw.id
        );
    }
    if !raw.url.starts_with("http") || raw.url.ends_with('/') {
        bail!(
            "sources.toml: url for {:?} must be absolute with no trailing slash, got {:?}",
            raw.id,
            raw.url
        );
    }
    if raw.tier.trim().is_empty() {
        bail!("sources.toml: tier for {:?} is empty", raw.id);
    }

    let spec = match raw.kind {
        Kind::Repo => {
            let branch = raw
                .branch
                .clone()
                .ok_or_else(|| anyhow::anyhow!("sources.toml: repo {:?} needs `branch`", raw.id))?;
            let paths = raw
                .paths
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("sources.toml: repo {:?} needs non-empty `paths`", raw.id)
                })?;
            // A trailing slash would make the tarball filter match nothing
            // and the deletion pass then wipe the whole local mirror.
            for path in &paths {
                if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
                    bail!(
                        "sources.toml: paths entry {path:?} for {:?} must be a bare \
                         repo-relative directory (no leading or trailing slash)",
                        raw.id
                    );
                }
            }
            let doc_url = raw.doc_url.clone().ok_or_else(|| {
                anyhow::anyhow!("sources.toml: repo {:?} needs `doc_url`", raw.id)
            })?;
            let placeholders =
                doc_url.contains("{stem}") as u8 + doc_url.contains("{path}") as u8;
            if placeholders != 1 {
                bail!(
                    "sources.toml: doc_url for {:?} must contain exactly one of \
                     {{stem}} or {{path}}, got {doc_url:?}",
                    raw.id
                );
            }
            // A tracking source whose doc_url names a fixed branch would
            // sync fine forever while every citation points at a ref that
            // freezes and is eventually deleted — silent, and invisible in
            // any test. The two settings only make sense together.
            if branch == wikipethia_fetch::TRACK_DEFAULT && !doc_url.contains("/blob/HEAD/") {
                bail!(
                    "sources.toml: {:?} tracks the default branch, so its doc_url must \
                     use /blob/HEAD/ — a fixed branch there would freeze every citation \
                     at the ref that was current today. Got {doc_url:?}",
                    raw.id
                );
            }
            // Defaulting to markdown keeps every pre-existing source
            // byte-identical in behavior.
            let file_types = raw.file_types.clone().unwrap_or_else(|| vec!["md".into()]);
            if file_types.is_empty() {
                bail!("sources.toml: `file_types` for {:?} must not be empty", raw.id);
            }
            for ext in &file_types {
                // A dotted or wildcarded value would silently match nothing
                // and the deletion pass would then wipe the local mirror.
                if ext.is_empty() || ext.starts_with('.') || ext.contains(['*', '/', '\\']) {
                    bail!(
                        "sources.toml: file_types entry {ext:?} for {:?} must be a bare \
                         extension without a dot, e.g. \"md\" or \"py\"",
                        raw.id
                    );
                }
            }
            SourceSpec::Repo {
                branch,
                paths,
                doc_url,
                file_types,
            }
        }
        kind => {
            for (name, set) in [
                ("branch", raw.branch.is_some()),
                ("paths", raw.paths.is_some()),
                ("doc_url", raw.doc_url.is_some()),
                ("file_types", raw.file_types.is_some()),
            ] {
                if set {
                    bail!(
                        "sources.toml: `{name}` is a repo-only field, but {:?} is kind {:?}",
                        raw.id,
                        kind.name()
                    );
                }
            }
            match kind {
                Kind::Discourse => SourceSpec::Discourse,
                Kind::Feed => SourceSpec::Feed,
                Kind::Repo => unreachable!(),
            }
        }
    };
    Ok(Source {
        id: raw.id,
        kind: raw.kind,
        url: raw.url,
        tier: raw.tier,
        spec,
    })
}

fn host(url: &str) -> &str {
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    rest.split('/').next().unwrap_or(rest)
}

/// The one place a manifest entry becomes a concrete adapter. New kinds add
/// a variant and an arm here.
pub fn adapter_for(source: &Source) -> Box<dyn Adapter> {
    let data_dir = PathBuf::from("data").join(&source.id);
    match &source.spec {
        SourceSpec::Discourse => Box::new(discourse_adapter(source)),
        SourceSpec::Repo {
            branch,
            paths,
            doc_url,
            file_types,
        } => Box::new(RepoAdapter {
            source_id: source.id.clone(),
            repo_url: source.url.clone(),
            branch: branch.clone(),
            paths: paths.clone(),
            doc_url: doc_url.clone(),
            file_types: file_types.clone(),
            data_dir,
            dates: Default::default(),
        }),
        SourceSpec::Feed => Box::new(FeedAdapter {
            source_id: source.id.clone(),
            feed_url: source.url.clone(),
            data_dir,
        }),
    }
}

/// Also used directly by `sync --topic`, which needs the concrete type for
/// its Discourse-only `sync_topic` — one constructor so the data layout
/// can never drift between the two call sites.
pub fn discourse_adapter(source: &Source) -> DiscourseAdapter {
    DiscourseAdapter {
        source_id: source.id.clone(),
        base_url: source.url.clone(),
        data_dir: PathBuf::from("data").join(&source.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [[sources]]
        id = "ethresearch"
        kind = "discourse"
        url = "https://ethresear.ch"
        tier = "research"

        [[sources]]
        id = "ethmagicians"
        kind = "discourse"
        url = "https://ethereum-magicians.org"
        tier = "standards"

        [[sources]]
        id = "eips"
        kind = "repo"
        url = "https://github.com/ethereum/EIPs"
        branch = "master"
        paths = ["EIPS"]
        doc_url = "https://eips.ethereum.org/EIPS/{stem}"
        tier = "spec"

        [[sources]]
        id = "consensusspecs"
        kind = "repo"
        url = "https://github.com/ethereum/consensus-specs"
        branch = "master"
        paths = ["specs"]
        doc_url = "https://github.com/ethereum/consensus-specs/blob/master/{path}"
        tier = "spec"

        [[sources]]
        id = "vitalik"
        kind = "feed"
        url = "https://vitalik.eth.limo/feed.xml"
        tier = "blog"
    "#;

    #[test]
    fn parses_all_three_kinds_and_repos_may_share_a_host() {
        let manifest = Manifest::parse(GOOD).unwrap();
        assert_eq!(manifest.sources.len(), 5);
        // Two repo sources on github.com are legal.
        assert!(matches!(manifest.sources[2].spec, SourceSpec::Repo { .. }));
        assert!(matches!(manifest.sources[3].spec, SourceSpec::Repo { .. }));
        assert!(matches!(manifest.sources[4].spec, SourceSpec::Feed));
    }

    #[test]
    fn select_returns_all_or_one_and_names_valid_ids_on_miss() {
        let manifest = Manifest::parse(GOOD).unwrap();
        assert_eq!(manifest.select(None).unwrap().len(), 5);
        assert_eq!(manifest.select(Some("eips")).unwrap()[0].id, "eips");
        let err = manifest.select(Some("nope")).unwrap_err().to_string();
        assert!(err.contains("ethresearch"), "{err}");
    }

    #[test]
    fn crawling_kinds_still_reject_shared_hosts() {
        let two_feeds = r#"
            [[sources]]
            id = "a"
            kind = "feed"
            url = "https://example.org/a.xml"
            tier = "blog"

            [[sources]]
            id = "b"
            kind = "feed"
            url = "https://example.org/b.xml"
            tier = "blog"
        "#;
        let err = Manifest::parse(two_feeds).unwrap_err().to_string();
        assert!(err.contains("shares a host"), "{err}");
    }

    #[test]
    fn tracking_sources_must_use_a_head_doc_url() {
        let toml = |branch: &str, doc_url: &str| {
            format!(
                "[[sources]]\nid = \"x\"\nkind = \"repo\"\n\
                 url = \"https://github.com/o/r\"\nbranch = \"{branch}\"\n\
                 paths = [\"src\"]\ndoc_url = \"{doc_url}\"\ntier = \"spec\"\n"
            )
        };
        // A tracking source whose doc_url pins a branch would sync green
        // forever while every citation froze at today's ref.
        assert!(
            Manifest::parse(&toml("default", "https://github.com/o/r/blob/forks/x/{path}")).is_err()
        );
        assert!(
            Manifest::parse(&toml("default", "https://github.com/o/r/blob/HEAD/{path}")).is_ok()
        );
        // Pinned sources are unaffected — they may name any ref.
        assert!(
            Manifest::parse(&toml("master", "https://github.com/o/r/blob/master/{path}")).is_ok()
        );
    }

    #[test]
    fn rejects_bad_manifests() {
        let dup_id = GOOD.replace("id = \"consensusspecs\"", "id = \"eips\"");
        assert!(Manifest::parse(&dup_id).unwrap_err().to_string().contains("duplicate source id"));

        let dup_url = GOOD.replace(
            "url = \"https://github.com/ethereum/consensus-specs\"",
            "url = \"https://github.com/ethereum/EIPs\"",
        );
        assert!(Manifest::parse(&dup_url).unwrap_err().to_string().contains("duplicate source url"));

        // Unknown kind.
        assert!(Manifest::parse(&GOOD.replace("\"feed\"", "\"rss\"")).is_err());

        // Repo-only field on a discourse source.
        let stray = GOOD.replace(
            "url = \"https://ethresear.ch\"",
            "url = \"https://ethresear.ch\"\nbranch = \"main\"",
        );
        assert!(Manifest::parse(&stray).unwrap_err().to_string().contains("repo-only"));

        // Repo missing its fields.
        let no_branch = GOOD.replace("branch = \"master\"\n        paths = [\"EIPS\"]", "paths = [\"EIPS\"]");
        assert!(Manifest::parse(&no_branch).unwrap_err().to_string().contains("needs `branch`"));

        // doc_url without a placeholder.
        let no_ph = GOOD.replace("https://eips.ethereum.org/EIPS/{stem}", "https://eips.ethereum.org/EIPS/x");
        assert!(Manifest::parse(&no_ph).unwrap_err().to_string().contains("exactly one"));

        // Bad id charset.
        let slash = GOOD.replace("\"ethmagicians\"", "\"eth/magicians\"");
        assert!(Manifest::parse(&slash).unwrap_err().to_string().contains("a-z0-9"));
    }

    #[test]
    fn adapter_for_builds_all_kinds() {
        let manifest = Manifest::parse(GOOD).unwrap();
        for source in &manifest.sources {
            // Smoke: construction succeeds; parse is exercised per adapter
            // in wikipethia-fetch's own tests.
            let _ = adapter_for(source);
        }
        let topic = serde_json::json!({
            "id": 1, "title": "T", "post_stream": { "stream": [5], "posts": [
                { "id": 5, "post_type": 1, "post_number": 1, "username": "a",
                  "created_at": "2020-01-01T00:00:00Z", "raw": "hi" }
            ]}
        });
        let docs = adapter_for(&manifest.sources[1]).parse(&topic).unwrap();
        assert_eq!(docs[0].id, "ethmagicians/post/5");
    }
}
