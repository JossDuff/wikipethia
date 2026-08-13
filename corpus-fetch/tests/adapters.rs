//! Offline tests of the repo and feed adapters: tarball unpack + filtering,
//! change detection, deletion pass, commit-feed dates, RSS walk, and the
//! parse paths. A fake Fetcher serves committed fixtures — no network, no
//! git binary, anywhere.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use corpus_fetch::{Adapter, FeedAdapter, FetchError, Fetcher, RepoAdapter};
use serde_json::Value;

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read(&path).expect("fixture exists")
}

fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture_bytes(name)).expect("fixture is utf-8")
}

/// Serves canned text/bytes responses by exact URL, recording requests.
#[derive(Default)]
struct FakeWeb {
    texts: HashMap<String, String>,
    bytes: HashMap<String, Vec<u8>>,
    requests: Rc<RefCell<Vec<String>>>,
}

impl Fetcher for FakeWeb {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        Err(FetchError::Shape(format!("unexpected get_json({url})")))
    }

    fn get_text(&mut self, url: &str) -> Result<String, FetchError> {
        self.requests.borrow_mut().push(url.to_string());
        self.texts
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Shape(format!("no text fixture for {url}")))
    }

    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.requests.borrow_mut().push(url.to_string());
        self.bytes
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Shape(format!("no bytes fixture for {url}")))
    }
}

const TARBALL: &str = "https://codeload.github.com/ethereum/testrepo/tar.gz/refs/heads/master";
const SPEC_ATOM: &str =
    "https://github.com/ethereum/testrepo/commits/master/specs/phase0/beacon-chain.md.atom";

fn repo_adapter(data_dir: &Path, paths: &[&str]) -> RepoAdapter {
    RepoAdapter {
        source_id: "testrepo".into(),
        repo_url: "https://github.com/ethereum/testrepo".into(),
        branch: "master".into(),
        paths: paths.iter().map(|s| s.to_string()).collect(),
        doc_url: "https://example.org/{path}".into(),
        file_types: vec!["md".into()],
        data_dir: data_dir.to_path_buf(),
        dates: Default::default(),
    }
}

fn repo_web() -> (FakeWeb, Rc<RefCell<Vec<String>>>) {
    let mut web = FakeWeb::default();
    web.bytes.insert(TARBALL.into(), fixture_bytes("repo.tar.gz"));
    web.texts.insert(SPEC_ATOM.into(), fixture_text("commits.atom"));
    let requests = Rc::clone(&web.requests);
    (web, requests)
}

#[test]
fn repo_sync_unpacks_filters_and_dates() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, requests) = repo_web();

    let stats = adapter.sync(&mut web, None).unwrap();
    assert_eq!(stats.fetched, 2, "two wanted .md files in the tarball");

    // Prefix stripped, paths filter applied: README.md and the .svg are out.
    assert!(dir.path().join("files/EIPS/eip-1.md").exists());
    assert!(dir.path().join("files/specs/phase0/beacon-chain.md").exists());
    assert!(!dir.path().join("files/README.md").exists());
    assert!(!dir.path().join("files/EIPS/assets/diagram.svg").exists());

    // The dates pass hit only the frontmatter-less file.
    let atom_hits = requests.borrow().iter().filter(|u| u.ends_with(".atom")).count();
    assert_eq!(atom_hits, 1);
    let dates = fs::read_to_string(dir.path().join("dates.json")).unwrap();
    assert!(dates.contains("2026-03-14T09:30:00Z"), "{dates}");

    // Resync: byte-identical files are skipped, no atom refetch.
    let (mut web2, requests2) = repo_web();
    let stats = adapter.sync(&mut web2, None).unwrap();
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.skipped, 2);
    assert!(requests2.borrow().iter().all(|u| !u.ends_with(".atom")));
}

#[test]
fn repo_sync_prunes_files_removed_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, None).unwrap();

    // A file that exists locally but not in the (next) tarball disappears.
    let stray = dir.path().join("files/EIPS/eip-9999.md");
    fs::write(&stray, "---\neip: 9999\ncreated: 2020-01-01\n---\ngone").unwrap();
    let (mut web2, _) = repo_web();
    adapter.sync(&mut web2, None).unwrap();
    assert!(!stray.exists(), "upstream-removed file must be pruned");
}

#[test]
fn repo_parse_frontmatter_and_spec_paths() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, None).unwrap();

    // EIP path: everything from frontmatter.
    let docs = adapter
        .parse_file(&dir.path().join("files/EIPS/eip-1.md"))
        .unwrap();
    assert_eq!(docs.len(), 1);
    let eip = &docs[0];
    assert_eq!(eip.id, "testrepo/eip-1");
    assert_eq!(eip.title, "EIP-1: EIP Purpose and Guidelines");
    assert_eq!(eip.author.as_deref(), Some("Martin Becze <mb@ethereum.org>"));
    assert_eq!(eip.published, "2015-10-27T00:00:00Z");
    assert_eq!(eip.url, "https://example.org/EIPS/eip-1.md");
    assert!(eip.content.contains("EIP stands for"));
    assert!(!eip.content.contains("created:"), "frontmatter must not leak");
    let tags = eip.meta["tags"].as_array().unwrap();
    assert!(tags.iter().any(|t| t == "Living"));

    // Spec path: H1 title, dates.json date, no author.
    let docs = adapter
        .parse_file(&dir.path().join("files/specs/phase0/beacon-chain.md"))
        .unwrap();
    let spec = &docs[0];
    assert_eq!(spec.id, "testrepo/specs/phase0/beacon-chain");
    assert_eq!(spec.title, "Phase 0 -- The Beacon Chain");
    assert_eq!(spec.author, None);
    assert_eq!(spec.published, "2026-03-14T09:30:00Z");
    assert!(spec.content.contains("is_active_validator"));
}

#[test]
fn repo_raw_files_walks_recursively_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, None).unwrap();
    let names: Vec<String> = adapter
        .raw_files()
        .unwrap()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].ends_with("EIPS/eip-1.md"));
    assert!(names[1].ends_with("specs/phase0/beacon-chain.md"));
}

fn feed_adapter(data_dir: &Path, source_id: &str, feed_url: &str) -> FeedAdapter {
    FeedAdapter {
        source_id: source_id.into(),
        feed_url: feed_url.into(),
        data_dir: data_dir.to_path_buf(),
    }
}

#[test]
fn feed_sync_fetches_posts_and_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "vitalik", "https://vitalik.eth.limo/feed.xml");
    let mut web = FakeWeb::default();
    web.texts.insert(
        "https://vitalik.eth.limo/feed.xml".into(),
        fixture_text("feed_vitalik.xml"),
    );
    let post = fixture_text("post_vitalik.html");
    web.texts.insert(
        "https://vitalik.eth.limo/general/2026/07/28/obfuscation_part_ii_diamond_io.html".into(),
        post.clone(),
    );
    web.texts
        .insert("https://vitalik.eth.limo/general/2026/05/18/fv.html".into(), post);
    let requests = Rc::clone(&web.requests);

    let stats = adapter.sync(&mut web, None).unwrap();
    assert_eq!(stats.fetched, 2);
    assert!(dir.path().join("feed.xml").exists());
    assert!(dir.path().join("posts/general-2026-05-18-fv.json").exists());

    // Rerun: only the feed itself is refetched.
    requests.borrow_mut().clear();
    let stats = adapter.sync(&mut web, None).unwrap();
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.skipped, 2);
    assert_eq!(*requests.borrow(), vec!["https://vitalik.eth.limo/feed.xml"]);
}

#[test]
fn feed_full_content_description_skips_the_post_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "efblog", "https://blog.ethereum.org/en/feed.xml");
    let mut web = FakeWeb::default();
    web.texts.insert(
        "https://blog.ethereum.org/en/feed.xml".into(),
        fixture_text("feed_efblog.xml"),
    );
    // Deliberately NO fixture for the post URL: a fetch attempt would error.
    let stats = adapter.sync(&mut web, None).unwrap();
    assert_eq!(stats.fetched, 1);

    // And the wrapper parses into a document with the feed's metadata.
    let files = adapter.raw_files().unwrap();
    let docs = adapter.parse_file(&files[0]).unwrap();
    let doc = &docs[0];
    assert_eq!(doc.id, "efblog/2025/07/31/lean-ethereum");
    assert_eq!(doc.published, "2025-07-31T00:00:00Z");
    assert_eq!(doc.author.as_deref(), Some("Ethereum Foundation"));
    assert!(doc.content.contains("three pillars"));
    assert!(!doc.content.contains("<p>"), "HTML must be converted to text");
}

#[test]
fn feed_parse_extracts_article_text_from_fetched_html() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "vitalik", "https://vitalik.eth.limo/feed.xml");
    let mut web = FakeWeb::default();
    web.texts.insert(
        "https://vitalik.eth.limo/feed.xml".into(),
        fixture_text("feed_vitalik.xml"),
    );
    let post = fixture_text("post_vitalik.html");
    web.texts.insert(
        "https://vitalik.eth.limo/general/2026/07/28/obfuscation_part_ii_diamond_io.html".into(),
        post.clone(),
    );
    web.texts
        .insert("https://vitalik.eth.limo/general/2026/05/18/fv.html".into(), post);
    adapter.sync(&mut web, None).unwrap();

    let docs = adapter
        .parse_file(&dir.path().join("posts/general-2026-05-18-fv.json"))
        .unwrap();
    let doc = &docs[0];
    assert_eq!(doc.id, "vitalik/general/2026/05/18/fv");
    assert!(doc.content.contains("# A shallow dive into formal verification"));
    assert!(doc.content.contains("```\ndef verify"), "{}", doc.content);
    assert!(doc.content.contains("- Provers are slow"));
    assert!(!doc.content.contains("Home"), "nav chrome must be dropped");
    assert!(doc.content.contains("positive & accelerating"), "entities unescaped");
}

#[test]
fn erc_files_use_the_eip_key_but_title_as_erc() {
    // Real ethereum/ERCs files carry `eip:` frontmatter (kept after the
    // 2023 split); the designator comes from the file name.
    let dir = tempfile::tempdir().unwrap();
    let erc_dir = dir.path().join("files/ERCS");
    fs::create_dir_all(&erc_dir).unwrap();
    fs::write(
        erc_dir.join("erc-1046.md"),
        "---\neip: 1046\ntitle: tokenURI Interoperability\nauthor: someone\n\
         created: 2018-04-13\nstatus: Final\n---\n\nBody.",
    )
    .unwrap();
    let adapter = RepoAdapter {
        source_id: "ercs".into(),
        repo_url: "https://github.com/ethereum/ERCs".into(),
        branch: "master".into(),
        paths: vec!["ERCS".into()],
        doc_url: "https://ercs.ethereum.org/ERCS/{stem}".into(),
        file_types: vec!["md".into()],
        data_dir: dir.path().to_path_buf(),
        dates: Default::default(),
    };
    let docs = adapter
        .parse_file(&erc_dir.join("erc-1046.md"))
        .unwrap();
    assert_eq!(docs[0].title, "ERC-1046: tokenURI Interoperability");
    assert_eq!(docs[0].id, "ercs/erc-1046");
}

#[test]
fn interrupted_dates_pass_resumes_from_disk_state() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, None).unwrap();

    // Simulate an interruption having lost the dates file: files exist and
    // are byte-identical, but the date is gone.
    fs::remove_file(dir.path().join("dates.json")).unwrap();
    let (mut web2, requests2) = repo_web();
    let stats = adapter.sync(&mut web2, None).unwrap();
    assert_eq!(stats.fetched, 0, "files unchanged");
    let atom_hits = requests2.borrow().iter().filter(|u| u.ends_with(".atom")).count();
    assert_eq!(atom_hits, 1, "the dateless spec file must be re-dated");
    assert!(fs::read_to_string(dir.path().join("dates.json"))
        .unwrap()
        .contains("2026-03-14"));
}

#[test]
fn a_dead_article_link_is_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "vitalik", "https://vitalik.eth.limo/feed.xml");
    let mut web = FakeWeb::default();
    web.texts.insert(
        "https://vitalik.eth.limo/feed.xml".into(),
        fixture_text("feed_vitalik.xml"),
    );
    // Only the SECOND item's page exists; the first (and its un-rebased
    // fallback) is dead.
    web.texts.insert(
        "https://vitalik.eth.limo/general/2026/05/18/fv.html".into(),
        fixture_text("post_vitalik.html"),
    );
    let stats = adapter.sync(&mut web, None).unwrap();
    assert_eq!(stats.fetched, 1, "the live item must not be starved");
    assert!(dir.path().join("posts/general-2026-05-18-fv.json").exists());
    assert!(!dir
        .path()
        .join("posts/general-2026-07-28-obfuscation_part_ii_diamond_io.json")
        .exists());
}
