use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

pub(crate) const COMPATIBILITY_SCOPE: &str = "schema_and_selected_codex_binary";

#[derive(Serialize)]
pub(crate) struct HistoryCoverage {
    archived_threads: u64,
    supported_threads: u64,
    unsupported_threads: u64,
    formats: FormatCounts,
    scope: &'static str,
}

#[derive(Serialize)]
struct FormatCounts {
    legacy: u64,
    paginated: u64,
    other: u64,
}

impl HistoryCoverage {
    pub(crate) fn read(db: &Connection) -> Result<Self> {
        // One fixed-size aggregate result, including all unknown modes in
        // `other`; no titles, transcripts or arbitrarily many format labels.
        let (archived_threads, legacy, paginated): (i64, i64, i64) = db.query_row(
            "SELECT count(*), coalesce(sum(history_mode='legacy'),0), \
             coalesce(sum(history_mode='paginated'),0) FROM threads WHERE archived=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let (archived_threads, legacy, paginated) = (
            u64::try_from(archived_threads)?,
            u64::try_from(legacy)?,
            u64::try_from(paginated)?,
        );
        let supported_threads = [("legacy", legacy), ("paginated", paginated)]
            .into_iter()
            .filter(|(mode, _)| crate::engine::supported_history_mode(mode))
            .map(|(_, count)| count)
            .sum();
        Ok(Self {
            archived_threads,
            supported_threads,
            unsupported_threads: archived_threads - supported_threads,
            formats: FormatCounts {
                legacy,
                paginated,
                other: archived_threads - legacy - paginated,
            },
            scope: "database_history_modes_only; use preview for artifact, lineage and retention eligibility",
        })
    }

    pub(crate) fn archived_threads(&self) -> u64 {
        self.archived_threads
    }

    pub(crate) fn message(&self) -> String {
        let warning = if self.archived_threads == 0 {
            "No archived threads to assess."
        } else if self.supported_threads == 0 {
            "Warning: no archived threads have a supported history format; cleanup will skip all of them."
        } else if self.unsupported_threads > 0 {
            "Warning: mixed history-format coverage; unsupported formats will be skipped."
        } else {
            "All archived threads use supported history formats."
        };
        format!(
            "Archive history formats: {} total, {} supported, {} unsupported (legacy {}, paginated {}, other {}).\n{warning}\nFormat counts do not establish deletion eligibility. Use preview to check artifacts, lineage and retention.",
            self.archived_threads,
            self.supported_threads,
            self.unsupported_threads,
            self.formats.legacy,
            self.formats.paginated,
            self.formats.other,
        )
    }
}
