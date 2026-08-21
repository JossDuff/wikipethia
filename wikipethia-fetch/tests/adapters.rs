//! Offline tests of the repo and feed adapters: tarball unpack + filtering,
//! change detection, deletion pass, commit-feed dates, RSS walk, and the
//! parse paths. A fake Fetcher serves committed fixtures — no network, no
//! git binary, anywhere.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use wikipethia_fetch::{Adapter, FeedAdapter, FetchError, Fetcher, RepoAdapter, SyncIntent};
use serde_json::Value;

/// A plain sync: no cap, no widening, no forced refetch. What the CLI passes
/// for `corpus sync` with no flags.
fn everything() -> SyncIntent {
    SyncIntent::default()
}

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

/// The branch's own commit feed, which sync reads to learn the head commit.
const BRANCH_ATOM: &str = "https://github.com/ethereum/testrepo/commits/master.atom";

/// [`repo_web`] that also answers the head-commit lookup, so the
/// unchanged-repo shortcut is reachable. Most tests deliberately leave it
/// out — an unanswerable lookup means "head unknown", which falls through to
/// the full tarball path and keeps those tests about what they were about.
fn repo_web_with_head() -> (FakeWeb, Rc<RefCell<Vec<String>>>) {
    let (mut web, requests) = repo_web();
    web.texts
        .insert(BRANCH_ATOM.into(), fixture_text("commits.atom"));
    (web, requests)
}

fn tarball_requested(requests: &Rc<RefCell<Vec<String>>>) -> bool {
    requests.borrow().iter().any(|u| u == TARBALL)
}

#[test]
fn repo_sync_unpacks_filters_and_dates() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, requests) = repo_web();

    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.fetched, 2, "two wanted .md files in the tarball");

    // Prefix stripped, paths filter applied: README.md and the .svg are out.
    assert!(dir.path().join("files/EIPS/eip-1.md").exists());
    assert!(dir.path().join("files/specs/phase0/beacon-chain.md").exists());
    assert!(!dir.path().join("files/README.md").exists());
    assert!(!dir.path().join("files/EIPS/assets/diagram.svg").exists());

    // The dates pass hit only the frontmatter-less file.
    // Count the per-FILE commit feed specifically: sync also reads the
    // branch's own feed to learn its head commit, and that is not a date fetch.
    let atom_hits = requests.borrow().iter().filter(|u| *u == SPEC_ATOM).count();
    assert_eq!(atom_hits, 1);
    let dates = fs::read_to_string(dir.path().join("dates.json")).unwrap();
    assert!(dates.contains("2026-03-14T09:30:00Z"), "{dates}");

    // Resync: byte-identical files are skipped, no atom refetch.
    let (mut web2, requests2) = repo_web();
    let stats = adapter.sync(&mut web2, &everything()).unwrap();
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.skipped, 2);
    assert!(requests2.borrow().iter().all(|u| *u != SPEC_ATOM));
}

#[test]
fn repo_sync_prunes_files_removed_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, &everything()).unwrap();

    // A file that exists locally but not in the (next) tarball disappears.
    let stray = dir.path().join("files/EIPS/eip-9999.md");
    fs::write(&stray, "---\neip: 9999\ncreated: 2020-01-01\n---\ngone").unwrap();
    let (mut web2, _) = repo_web();
    adapter.sync(&mut web2, &everything()).unwrap();
    assert!(!stray.exists(), "upstream-removed file must be pruned");
}

#[test]
fn a_tarball_matching_nothing_refuses_to_wipe_the_local_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, &everything()).unwrap();
    assert!(dir.path().join("files/EIPS/eip-1.md").exists());

    // Upstream restructures under the SAME configured paths: the new
    // tarball has no EIPS/ or specs/ at all. execution-specs has already
    // done exactly this once (src/ethereum/<fork> → src/ethereum/forks/
    // <fork>), and with branch = "default" it can happen without anyone
    // editing sources.toml. Pruning here would silently delete the source.
    let mut web2 = FakeWeb::default();
    web2.bytes
        .insert(TARBALL.into(), fixture_bytes("repo_restructured.tar.gz"));
    let err = adapter.sync(&mut web2, &everything()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("matched 0 files"), "{msg}");
    assert!(msg.contains("paths"), "error must point at the likely cause: {msg}");

    // Nothing was deleted, and dates.json survived.
    assert!(dir.path().join("files/EIPS/eip-1.md").exists());
    assert!(dir.path().join("files/specs/phase0/beacon-chain.md").exists());
    assert!(dir.path().join("dates.json").exists());
}

#[test]
fn repo_parse_frontmatter_and_spec_paths() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web();
    adapter.sync(&mut web, &everything()).unwrap();

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
    adapter.sync(&mut web, &everything()).unwrap();
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

    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.fetched, 2);
    assert!(dir.path().join("feed.xml").exists());
    assert!(dir.path().join("posts/general-2026-05-18-fv.json").exists());

    // Rerun: the pages ARE refetched — a feed whose descriptions are teasers
    // offers no timestamp to compare, so the article itself is the only thing
    // that can say whether it changed. What must not happen is a write.
    requests.borrow_mut().clear();
    let before = fs::metadata(dir.path().join("posts/general-2026-05-18-fv.json"))
        .unwrap()
        .modified()
        .unwrap();
    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.skipped, 2);
    assert_eq!(requests.borrow().len(), 3, "feed plus both articles");
    let after = fs::metadata(dir.path().join("posts/general-2026-05-18-fv.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "an unchanged article must not be rewritten");
}

#[test]
fn feed_sync_picks_up_an_article_edited_after_publication() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "vitalik", "https://vitalik.eth.limo/feed.xml");
    const FV: &str = "https://vitalik.eth.limo/general/2026/05/18/fv.html";
    const OBFUSCATION: &str =
        "https://vitalik.eth.limo/general/2026/07/28/obfuscation_part_ii_diamond_io.html";

    let mut web = FakeWeb::default();
    web.texts.insert(
        "https://vitalik.eth.limo/feed.xml".into(),
        fixture_text("feed_vitalik.xml"),
    );
    let post = fixture_text("post_vitalik.html");
    web.texts.insert(OBFUSCATION.into(), post.clone());
    web.texts.insert(FV.into(), post.clone());
    adapter.sync(&mut web, &everything()).unwrap();

    // The author fixes a typo. Nothing in the feed changes: no per-item
    // timestamp exists to move, and the file is still right there on disk —
    // which is exactly why presence-means-done froze corrections out.
    web.texts
        .insert(FV.into(), post.replace("Provers are slow", "Provers are fast"));
    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.updated, 1, "the edited article");
    assert_eq!(stats.skipped, 1, "the untouched one");
    assert_eq!(stats.fetched, 0, "neither article is new");

    let docs = adapter
        .parse_file(&dir.path().join("posts/general-2026-05-18-fv.json"))
        .unwrap();
    assert!(docs[0].content.contains("Provers are fast"));
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
    let stats = adapter.sync(&mut web, &everything()).unwrap();
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
    adapter.sync(&mut web, &everything()).unwrap();

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
    adapter.sync(&mut web, &everything()).unwrap();

    // Simulate an interruption having lost the dates file: files exist and
    // are byte-identical, but the date is gone.
    fs::remove_file(dir.path().join("dates.json")).unwrap();
    let (mut web2, requests2) = repo_web();
    let stats = adapter.sync(&mut web2, &everything()).unwrap();
    assert_eq!(stats.fetched, 0, "files unchanged");
    let atom_hits = requests2.borrow().iter().filter(|u| *u == SPEC_ATOM).count();
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
    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.fetched, 1, "the live item must not be starved");
    assert!(dir.path().join("posts/general-2026-05-18-fv.json").exists());
    assert!(!dir
        .path()
        .join("posts/general-2026-07-28-obfuscation_part_ii_diamond_io.json")
        .exists());
}

#[test]
fn repo_sync_skips_the_tarball_when_the_head_commit_has_not_moved() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, requests) = repo_web_with_head();
    adapter.sync(&mut web, &everything()).unwrap();
    assert!(tarball_requested(&requests), "the first sync must fetch it");

    // Nothing has been pushed. The tarball is the most expensive request in a
    // routine update — six minutes for EIPs — and it would teach nothing.
    let (mut web2, requests2) = repo_web_with_head();
    let stats = adapter.sync(&mut web2, &everything()).unwrap();
    assert!(!tarball_requested(&requests2), "unchanged head, no download");
    assert_eq!(stats, wikipethia_fetch::SyncStats::default());
    assert!(dir.path().join("files/EIPS/eip-1.md").exists(), "and nothing is lost");
}

#[test]
fn a_paths_change_forces_a_resync_even_when_upstream_stands_still() {
    let dir = tempfile::tempdir().unwrap();
    let (mut web, _) = repo_web_with_head();
    repo_adapter(dir.path(), &["EIPS"])
        .sync(&mut web, &everything())
        .unwrap();
    assert!(!dir.path().join("files/specs/phase0/beacon-chain.md").exists());

    // sources.toml now asks for specs/ too. The head commit is identical, so
    // a SHA check alone would skip the download and the new path would stay
    // missing until someone else happened to push.
    let (mut web2, requests2) = repo_web_with_head();
    let stats = repo_adapter(dir.path(), &["EIPS", "specs"])
        .sync(&mut web2, &everything())
        .unwrap();
    assert!(tarball_requested(&requests2), "the config changed, so the tree must be re-read");
    assert_eq!(stats.fetched, 1, "the newly wanted file");
    assert!(dir.path().join("files/specs/phase0/beacon-chain.md").exists());
}

#[test]
fn force_overrides_the_unchanged_head_shortcut() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = repo_adapter(dir.path(), &["EIPS", "specs"]);
    let (mut web, _) = repo_web_with_head();
    adapter.sync(&mut web, &everything()).unwrap();

    let (mut web2, requests2) = repo_web_with_head();
    let forced = SyncIntent {
        force: true,
        ..SyncIntent::default()
    };
    adapter.sync(&mut web2, &forced).unwrap();
    assert!(tarball_requested(&requests2), "--force must reach past the checkpoint");
}

#[test]
fn a_routine_feed_sync_only_rechecks_recent_items() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "vitalik", "https://vitalik.eth.limo/feed.xml");
    const FV: &str = "https://vitalik.eth.limo/general/2026/05/18/fv.html";
    const OBFUSCATION: &str =
        "https://vitalik.eth.limo/general/2026/07/28/obfuscation_part_ii_diamond_io.html";

    // 40 filler items ahead of the two real ones pushes them past the
    // recheck window. Both real feeds are full archives — 632 items for the
    // EF blog — so this is the normal case, not a contrived one.
    let real = fixture_text("feed_vitalik.xml");
    let filler: String = (0..40)
        .map(|i| {
            format!(
                "<item><title>Filler {i}</title>\
                 <link>https://vitalik.eth.limo/general/2026/09/{:02}/filler{i}.html</link>\
                 <pubDate>Mon, 01 Sep 2026 00:00:00 GMT</pubDate>\
                 <description>x</description></item>",
                i + 1
            )
        })
        .collect();
    let padded = real.replacen("<item>", &format!("{filler}<item>"), 1);

    let mut web = FakeWeb::default();
    web.texts
        .insert("https://vitalik.eth.limo/feed.xml".into(), padded);
    let post = fixture_text("post_vitalik.html");
    web.texts.insert(OBFUSCATION.into(), post.clone());
    web.texts.insert(FV.into(), post.clone());
    for i in 0..40 {
        web.texts.insert(
            format!("https://vitalik.eth.limo/general/2026/09/{:02}/filler{i}.html", i + 1),
            post.clone(),
        );
    }
    let requests = Rc::clone(&web.requests);
    adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(fs::read_dir(dir.path().join("posts")).unwrap().count(), 42);

    // Second run: only the newest 30 are re-read. The two real articles sit
    // at positions 40 and 41 and cost nothing.
    requests.borrow_mut().clear();
    let stats = adapter.sync(&mut web, &everything()).unwrap();
    assert_eq!(stats.skipped, 42, "everything is unchanged either way");
    assert_eq!(requests.borrow().len(), 31, "the feed plus 30 articles");
    assert!(
        !requests.borrow().iter().any(|u| u == FV),
        "an item past the window must not be refetched"
    );

    // --full reaches all of them, which is how a correction to an old
    // article is recovered.
    requests.borrow_mut().clear();
    let full = SyncIntent {
        full: true,
        ..SyncIntent::default()
    };
    adapter.sync(&mut web, &full).unwrap();
    assert!(requests.borrow().iter().any(|u| u == FV));
}

#[test]
fn a_full_content_feed_still_notices_corrections_to_old_articles() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = feed_adapter(dir.path(), "efblog", "https://blog.ethereum.org/en/feed.xml");
    let feed = fixture_text("feed_efblog.xml");
    let mut web = FakeWeb::default();
    web.texts
        .insert("https://blog.ethereum.org/en/feed.xml".into(), feed.clone());
    adapter.sync(&mut web, &everything()).unwrap();

    // Pad the feed so the real item sits well past the recheck window. For a
    // feed that carries whole articles in its descriptions, comparing it
    // still costs no request — the corrected text arrived with the feed — so
    // skipping on position alone would serve stale text for no saving.
    // Over 1000 chars with a <p>, so `is_full_content` holds and the item
    // needs no request — the same shape as a real EF-blog description.
    let body = format!("<p>{}</p>", "filler ".repeat(200));
    let filler: String = (0..40)
        .map(|i| {
            format!(
                "<item><title>Filler {i}</title>\
                 <link>https://blog.ethereum.org/en/2026/09/{:02}/filler{i}</link>\
                 <pubDate>Tue, 01 Sep 2026 00:00:00 GMT</pubDate>\
                 <description>{body}</description></item>",
                i + 1,
            )
        })
        .collect();
    let corrected = feed
        .replacen("<item>", &format!("{filler}<item>"), 1)
        .replace("three pillars", "four pillars");

    let mut web2 = FakeWeb::default();
    web2.texts
        .insert("https://blog.ethereum.org/en/feed.xml".into(), corrected);
    let requests = Rc::clone(&web2.requests);
    let stats = adapter.sync(&mut web2, &everything()).unwrap();
    assert_eq!(stats.updated, 1, "the corrected article");
    assert_eq!(
        requests.borrow().len(),
        1,
        "and it cost nothing but the feed itself"
    );

    let docs = adapter
        .parse_file(&dir.path().join("posts/2025-07-31-lean-ethereum.json"))
        .unwrap();
    assert!(docs[0].content.contains("four pillars"));
}
