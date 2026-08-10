//! sources.toml — the manifest of corpus sources. This is the file the M6
//! gate is about: adding a source of an existing kind must be an edit here
//! and nothing else.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use corpus_fetch::{Adapter, DiscourseAdapter};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub sources: Vec<Source>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Directory under data/ and the doc-id prefix — stable forever.
    pub id: String,
    pub kind: Kind,
    pub url: String,
    /// Opaque source-quality label carried on every search result.
    pub tier: String,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Discourse,
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
        let manifest: Manifest = toml::from_str(text).context("parsing sources.toml")?;
        if manifest.sources.is_empty() {
            bail!("sources.toml: no [[sources]] entries");
        }
        for (i, source) in manifest.sources.iter().enumerate() {
            let ok_id = !source.id.is_empty()
                && source.id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && source
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !ok_id {
                bail!(
                    "sources.toml: id {:?} must be [a-z0-9_-] and start alphanumeric \
                     (it is a directory name and a doc-id prefix)",
                    source.id
                );
            }
            if manifest.sources[..i].iter().any(|s| s.id == source.id) {
                bail!("sources.toml: duplicate source id {:?}", source.id);
            }
            if !source.url.starts_with("http") || source.url.ends_with('/') {
                bail!(
                    "sources.toml: url for {:?} must be absolute with no trailing slash, \
                     got {:?}",
                    source.id,
                    source.url
                );
            }
            if source.tier.trim().is_empty() {
                bail!("sources.toml: tier for {:?} is empty", source.id);
            }
        }
        Ok(manifest)
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

/// The one place a manifest entry becomes a concrete adapter. New kinds
/// (M7) add a variant and an arm here.
pub fn adapter_for(source: &Source) -> Box<dyn Adapter> {
    match source.kind {
        Kind::Discourse => Box::new(DiscourseAdapter {
            source_id: source.id.clone(),
            base_url: source.url.clone(),
            data_dir: PathBuf::from("data").join(&source.id),
        }),
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
    "#;

    #[test]
    fn parses_the_two_source_manifest() {
        let manifest = Manifest::parse(GOOD).unwrap();
        assert_eq!(manifest.sources.len(), 2);
        assert_eq!(manifest.sources[0].id, "ethresearch");
        assert_eq!(manifest.sources[1].kind, Kind::Discourse);
        assert_eq!(manifest.sources[1].tier, "standards");
    }

    #[test]
    fn select_returns_all_or_one_and_names_valid_ids_on_miss() {
        let manifest = Manifest::parse(GOOD).unwrap();
        assert_eq!(manifest.select(None).unwrap().len(), 2);
        assert_eq!(
            manifest.select(Some("ethmagicians")).unwrap()[0].id,
            "ethmagicians"
        );
        let err = manifest.select(Some("ethmagician")).unwrap_err().to_string();
        assert!(err.contains("ethresearch, ethmagicians"), "{err}");
    }

    #[test]
    fn rejects_bad_manifests() {
        // Duplicate id.
        let dup = GOOD.replace("ethmagicians", "ethresearch");
        assert!(Manifest::parse(&dup).unwrap_err().to_string().contains("duplicate"));
        // Unknown kind.
        let rss = GOOD.replace("\"discourse\"", "\"rss\"");
        assert!(Manifest::parse(&rss).is_err());
        // Bad id charset (slash would escape data/).
        let slash = GOOD.replace("\"ethmagicians\"", "\"eth/magicians\"");
        assert!(Manifest::parse(&slash).unwrap_err().to_string().contains("a-z0-9"));
        // Trailing slash on url.
        let slashurl = GOOD.replace("https://ethresear.ch", "https://ethresear.ch/");
        assert!(Manifest::parse(&slashurl).unwrap_err().to_string().contains("trailing"));
        // Missing field.
        assert!(Manifest::parse("[[sources]]\nid = \"x\"").is_err());
        // Empty file (missing sources field) and explicit empty list.
        assert!(Manifest::parse("").is_err());
        assert!(
            Manifest::parse("sources = []")
                .unwrap_err()
                .to_string()
                .contains("no [[sources]]")
        );
    }

    #[test]
    fn adapter_for_builds_the_discourse_adapter() {
        let manifest = Manifest::parse(GOOD).unwrap();
        // Smoke: the trait object parses with the right source id.
        let adapter = adapter_for(manifest.sources[1..].first().unwrap());
        let topic = serde_json::json!({
            "id": 1, "title": "T", "post_stream": { "stream": [5], "posts": [
                { "id": 5, "post_type": 1, "post_number": 1, "username": "a",
                  "created_at": "2020-01-01T00:00:00Z", "raw": "hi" }
            ]}
        });
        let docs = adapter.parse(&topic).unwrap();
        assert_eq!(docs[0].id, "ethmagicians/post/5");
    }
}
