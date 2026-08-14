//! How a run reports itself.
//!
//! The rules, which the rest of the CLI is expected to hold to:
//!
//! 1. **stdout is results, stderr is narration.** A summary line goes to
//!    stdout; per-item progress, warnings, and heartbeats go to stderr. So
//!    `corpus update > log` keeps the shape of the run and leaves the chatter
//!    on the terminal, and `corpus-mcp`'s protocol-only-stdout discipline has
//!    an analogue here.
//! 2. **Nothing is printed for a no-op.** Before the sync became incremental
//!    a routine run emitted one "already on disk" line per topic — 7,094 of
//!    them across the two forums, none of them information. Unchanged items
//!    are counted and reported as a count.
//! 3. **One row per source, one summary per run.** Rows are column-aligned
//!    from the manifest's widest source id, so stdout stays a readable table
//!    even while stderr detail interleaves between the rows.

use std::time::Duration;

use corpus_fetch::SyncStats;

/// Which of the two pipeline commands is running. They share all three
/// stages; what differs is what the operator is owed up front.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// Clone day. Says what it is about to cost before it starts costing it.
    Build,
    /// Scheduled or routine. Assumes the reader knows the shape already.
    Update,
}

impl Run {
    pub fn verb(self) -> &'static str {
        match self {
            Run::Build => "build",
            Run::Update => "update",
        }
    }
}

/// `3m12s`, `0m48s`, `1h07m`. Long enough runs stop caring about seconds.
pub fn hms(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// An aligned two-column table of per-source rows, sized once from the
/// manifest so every stage's rows line up with every other stage's.
pub struct Table {
    width: usize,
}

impl Table {
    pub fn new<'a>(source_ids: impl IntoIterator<Item = &'a str>) -> Self {
        Table {
            width: source_ids.into_iter().map(str::len).max().unwrap_or(0),
        }
    }

    /// `  ethresearch      4 pages walked, 3 updated`
    pub fn row(&self, source_id: &str, detail: &str) {
        println!("  {:<width$}  {detail}", source_id, width = self.width);
    }

    /// [`Table::row`] with the elapsed time right of the detail.
    pub fn timed_row(&self, source_id: &str, detail: &str, elapsed: Duration) {
        // The detail column is padded so the times align too, but never
        // truncated — a long detail pushes its own time right rather than
        // losing what it had to say.
        println!(
            "  {:<width$}  {:<44}  {}",
            source_id,
            detail,
            hms(elapsed),
            width = self.width
        );
    }
}

/// `[2/3] index` — the stage banner shared by `build` and `update`.
pub fn stage(n: usize, of: usize, name: &str) {
    println!("[{n}/{of}] {name}");
}

/// What a sync did, in a phrase.
///
/// The counts are ordered by how much the reader cares: how far the walk
/// reached, then what changed, then what didn't. "up to date" replaces the
/// changed counts entirely when there are none — the whole point of the row
/// in that case is that there is nothing to read.
pub fn describe_sync(stats: &SyncStats) -> String {
    let mut parts: Vec<String> = Vec::new();
    if stats.pages > 0 {
        parts.push(format!(
            "{} page{} walked",
            stats.pages,
            plural(stats.pages)
        ));
    }
    if stats.fetched > 0 {
        parts.push(format!("{} new", stats.fetched));
    }
    if stats.updated > 0 {
        parts.push(format!("{} updated", stats.updated));
    }
    if stats.pruned > 0 {
        parts.push(format!("{} pruned", stats.pruned));
    }
    if !stats.changed() && stats.pruned == 0 {
        parts.push("up to date".into());
    } else if stats.skipped > 0 {
        parts.push(format!("{} unchanged", stats.skipped));
    }
    if parts.is_empty() {
        parts.push("nothing to do".into());
    }
    parts.join(", ")
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(fetched: usize, updated: usize, skipped: usize, pages: usize) -> SyncStats {
        SyncStats {
            fetched,
            updated,
            skipped,
            pages,
            ..SyncStats::default()
        }
    }

    #[test]
    fn a_quiet_walk_says_so_without_reciting_counts() {
        // The case that used to be 3,053 lines of "already on disk".
        assert_eq!(
            describe_sync(&stats(0, 0, 118, 3)),
            "3 pages walked, up to date"
        );
    }

    #[test]
    fn a_productive_walk_leads_with_what_changed() {
        assert_eq!(
            describe_sync(&stats(1, 3, 118, 4)),
            "4 pages walked, 1 new, 3 updated, 118 unchanged"
        );
    }

    #[test]
    fn a_skipped_source_reports_nothing_rather_than_zeroes() {
        // A repo short-circuited on an unchanged head SHA never looks at a
        // file, so it has no counts at all to report.
        assert_eq!(describe_sync(&SyncStats::default()), "up to date");
    }

    #[test]
    fn pruning_alone_still_counts_as_a_change() {
        let stats = SyncStats {
            pruned: 2,
            skipped: 900,
            ..SyncStats::default()
        };
        assert_eq!(describe_sync(&stats), "2 pruned, 900 unchanged");
    }

    #[test]
    fn one_page_is_singular() {
        assert_eq!(describe_sync(&stats(0, 0, 4, 1)), "1 page walked, up to date");
    }

    #[test]
    fn durations_switch_to_hours_when_they_earn_it() {
        assert_eq!(hms(Duration::from_secs(48)), "0m48s");
        assert_eq!(hms(Duration::from_secs(192)), "3m12s");
        assert_eq!(hms(Duration::from_secs(4020)), "1h07m");
    }
}
