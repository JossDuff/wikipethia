//! `wikipethia publish`: snapshot the corpus, compress and hash it, and
//! publish it as a GitHub release — the download that spares every adopter
//! the multi-hour build and its ~7,100-request forum crawl.
//!
//! A maintainer command, run from this repository with the `gh` CLI
//! authenticated. No CI: the corpus lives on the maintainer's machine, and
//! each release is a deliberate snapshot, not a pipeline stage.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use wikipethia_core::store::SourceStats;
use wikipethia_core::{SCHEMA_VERSION, Store, WriterLock};
use wikipethia_embed::{DIM, MODEL_ID};

use crate::manifest::{Kind, Manifest};

/// Best ratio zstd offers without the long-window flags. Compression runs
/// once per release on the maintainer's machine; every downloader pays the
/// transfer, so the trade is lopsided in favor of spending minutes here.
const ZSTD_LEVEL: i32 = 19;

pub struct Config {
    pub db: PathBuf,
    pub tag: Option<String>,
    pub out: PathBuf,
    pub dry_run: bool,
}

pub fn run(cfg: &Config) -> anyhow::Result<()> {
    // The lock is a fence, not a write intent: a snapshot taken mid-index or
    // mid-embed ships a half-built corpus, and the lock is the one signal
    // every writer already respects. Readers are never blocked by it.
    let lock = WriterLock::acquire(&cfg.db, "publish")?;
    let store = Store::open_existing(&cfg.db)
        .with_context(|| format!("opening {}", cfg.db.display()))?;
    let (stats, documents, vectors) = preflight(&store)?;
    // The checkpoints ARE the product, as much as the documents: a snapshot
    // without forum watermarks sends every downloader's first update on the
    // full ~7,100-request recrawl publishing exists to prevent — verified
    // the expensive way against a checkpoint-less snapshot. Repos and feeds
    // are exempt: their refetch cost is mirror-driven, not checkpoint-driven.
    // (This is also why publish runs from the clone: it reads sources.toml.)
    for source in Manifest::load()?.sources.iter().filter(|s| s.kind == Kind::Discourse) {
        if store.checkpoint(&source.id)?.is_none() {
            bail!(
                "no sync checkpoint recorded for {} — run `wikipethia update` once before \
                 publishing (checkpoints live in the database now and migrate on first sync)",
                source.id
            );
        }
    }

    let tag = match &cfg.tag {
        Some(tag) => tag.clone(),
        None => format!("corpus-{}", utc_today()),
    };
    // Before the vacuum, which is minutes of work on the real corpus. Under
    // --dry-run `gh` may be absent entirely, so the check is skipped rather
    // than half-run.
    if !cfg.dry_run {
        ensure_tag_free(&tag)?;
    }

    fs::create_dir_all(&cfg.out)
        .with_context(|| format!("creating {}", cfg.out.display()))?;
    let snapshot = cfg.out.join(format!("{tag}.sqlite"));
    let artifact = cfg.out.join(format!("{tag}.sqlite.zst"));
    let checksum = cfg.out.join(format!("{tag}.sqlite.zst.sha256"));
    let notes = cfg.out.join(format!("{tag}-notes.md"));
    // Leftovers from an interrupted attempt: VACUUM INTO refuses an existing
    // destination, and a stale artifact must never be what gets uploaded.
    for stale in [&snapshot, &artifact, &checksum, &notes] {
        if stale.exists() {
            fs::remove_file(stale).with_context(|| format!("removing stale {}", stale.display()))?;
        }
    }

    eprintln!("publish: snapshotting {} → {}…", cfg.db.display(), snapshot.display());
    store.vacuum_into(&snapshot)?;
    drop(store);
    drop(lock);

    // The artifact declares that it ships without the raw-file mirror, so a
    // downloader's sync stays incremental and their index never reads the
    // missing files as deletions. Stamped into the snapshot only — the live
    // corpus keeps its mirror and its flags stay unset.
    {
        let snap = Store::open(&snapshot)?;
        for source in &stats {
            snap.set_mirror_absent(&source.id, true)?;
        }
        // The vacuum ran under our own writer lock, so its row is in the
        // copy — a phantom writer to every downloader if it shipped.
        snap.clear_writer_lock()?;
    }
    // A cleanly closed last connection removes its WAL sidecars; anything
    // still beside the snapshot would be silently missing from the artifact.
    for suffix in ["-wal", "-shm"] {
        let sidecar = cfg.out.join(format!("{tag}.sqlite{suffix}"));
        if sidecar.exists() {
            bail!("{} survived the snapshot close — refusing to ship a torn file", sidecar.display());
        }
    }
    let raw_size = fs::metadata(&snapshot)?.len();

    eprintln!("publish: compressing (zstd level {ZSTD_LEVEL}, minutes on a full corpus)…");
    zstd::stream::copy_encode(
        BufReader::new(File::open(&snapshot)?),
        BufWriter::new(File::create(&artifact)?),
        ZSTD_LEVEL,
    )
    .with_context(|| format!("compressing {}", snapshot.display()))?;
    fs::remove_file(&snapshot)?;
    let zst_size = fs::metadata(&artifact)?.len();

    let digest = sha256_of(&artifact)?;
    // `sha256sum -c`-compatible: hash, two spaces, bare filename.
    fs::write(&checksum, format!("{digest}  {tag}.sqlite.zst\n"))?;

    fs::write(&notes, release_notes(&tag, &stats, documents, vectors))?;

    println!("artifact   {} ({})", artifact.display(), human_size(zst_size));
    println!("uncompressed {} → {:.0}% of original", human_size(raw_size), zst_size as f64 / raw_size as f64 * 100.0);
    println!("sha256     {digest}");
    println!("tag        {tag}");

    let create = format!(
        "gh release create {tag} {} {} --title {tag} --notes-file {}",
        artifact.display(),
        checksum.display(),
        notes.display()
    );
    if cfg.dry_run {
        println!("dry run — inspect {} and release with:", cfg.out.display());
        println!("  {create}");
        return Ok(());
    }

    eprintln!("publish: creating release {tag}…");
    let output = Command::new("gh")
        .args(["release", "create", &tag])
        .arg(&artifact)
        .arg(&checksum)
        .args(["--title", &tag, "--notes-file"])
        .arg(&notes)
        .output()
        .context("running `gh` — install the GitHub CLI and run `gh auth login`")?;
    if !output.status.success() {
        bail!(
            "gh release create failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // gh prints the release URL on stdout.
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

/// The READY check `status` performs, as refusals: a snapshot of a corpus
/// that cannot serve both search arms is not worth a release.
fn preflight(store: &Store) -> anyhow::Result<(Vec<SourceStats>, usize, usize)> {
    let documents = store.count()?;
    if documents == 0 {
        bail!("corpus holds no documents — run `wikipethia build` first");
    }
    let vectors = store.embedding_count()?;
    if vectors == 0 {
        bail!("corpus has no embeddings — run `wikipethia embed` first");
    }
    let missing = store.missing_embedding_count()?;
    if missing > 0 {
        bail!("{missing} chunk(s) still lack a vector — run `wikipethia embed` first");
    }
    match store.embedding_model()? {
        Some((model, dim)) if model == MODEL_ID && dim == DIM => {}
        Some((model, dim)) => bail!(
            "vectors were built by {model} ({dim}d), this build embeds with {MODEL_ID} ({DIM}d) \
             — re-embed with `wikipethia embed --force`"
        ),
        None => bail!("no embedding model recorded — run `wikipethia embed`"),
    }
    Ok((store.source_stats()?, documents, vectors))
}

/// Refuse a tag that already names a release: snapshots are immutable once
/// published. Only success is conclusive — `gh release view` also fails on
/// a missing tag, so any failure here falls through and `create` gives the
/// real error if something else is wrong.
fn ensure_tag_free(tag: &str) -> anyhow::Result<()> {
    let probe = Command::new("gh")
        .args(["release", "view", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running `gh` — install the GitHub CLI and run `gh auth login`")?;
    if probe.success() {
        bail!("release {tag} already exists — pass --tag to name this snapshot differently");
    }
    Ok(())
}

fn sha256_of(path: &std::path::Path) -> anyhow::Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("writing to a String");
    }
    Ok(out)
}

/// The release page is the download's documentation: what the snapshot
/// holds, what built it, and the three commands from asset to answers.
fn release_notes(tag: &str, stats: &[SourceStats], documents: usize, vectors: usize) -> String {
    let mut notes = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(notes, "A ready-built wikipethia corpus: {documents} documents, {vectors} vectors, schema v{SCHEMA_VERSION}, embedded with {MODEL_ID} ({DIM} dimensions).");
    let _ = writeln!(notes);
    let _ = writeln!(notes, "```");
    let _ = writeln!(notes, "sha256sum -c {tag}.sqlite.zst.sha256");
    let _ = writeln!(notes, "zstd -d {tag}.sqlite.zst -o corpus.sqlite");
    let _ = writeln!(notes, "wikipethia mcp --db corpus.sqlite");
    let _ = writeln!(notes, "```");
    let _ = writeln!(notes);
    let _ = writeln!(notes, "`wikipethia update` keeps a downloaded corpus current — its sync checkpoints travel inside the file. Licensing is per source; see the [README's licensing table](../../blob/main/README.md#licensing) — every document carries its `source`, so the corpus can be filtered to the licenses a use needs.");
    let _ = writeln!(notes);
    let _ = writeln!(notes, "| Source | Documents |");
    let _ = writeln!(notes, "|---|---|");
    for source in stats {
        let _ = writeln!(notes, "| {} | {} |", source.id, source.count);
    }
    notes
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1}GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0}MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// Today's UTC date as YYYY-MM-DD, for the default tag. Days-to-civil is
/// Howard Hinnant's algorithm; fifteen lines beat a calendar dependency the
/// crate would use for exactly this string.
fn utc_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is set after 1970")
        .as_secs();
    let (year, month, day) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_unix(secs: u64) -> (i64, u32, u32) {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_unix_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        assert_eq!(civil_from_unix(86_399), (1970, 1, 1));
        assert_eq!(civil_from_unix(86_400), (1970, 1, 2));
        // Leap day, and the day after.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29));
        assert_eq!(civil_from_unix(1_709_251_200), (2024, 3, 1));
        // Century non-leap boundary in the past era.
        assert_eq!(civil_from_unix(4_107_542_400), (2100, 3, 1));
        // The scoping day of this feature.
        assert_eq!(civil_from_unix(1_787_616_000), (2026, 8, 25));
    }

    #[test]
    fn release_notes_carry_the_verify_and_run_steps() {
        let stats = vec![SourceStats {
            id: "eips".into(),
            url: None,
            tier: Some("spec".into()),
            count: 585,
        }];
        let notes = release_notes("corpus-2026-08-25", &stats, 585, 1000);
        assert!(notes.contains("sha256sum -c corpus-2026-08-25.sqlite.zst.sha256"));
        assert!(notes.contains("zstd -d corpus-2026-08-25.sqlite.zst"));
        assert!(notes.contains("wikipethia mcp --db corpus.sqlite"));
        assert!(notes.contains("| eips | 585 |"));
        assert!(notes.contains("585 documents"));
    }

    #[test]
    fn checksum_digest_is_lowercase_hex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x");
        fs::write(&file, b"wikipethia").unwrap();
        let digest = sha256_of(&file).unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
