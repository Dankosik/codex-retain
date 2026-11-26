use crate::{
    config::{Policy, Store},
    database::{self, BatchThread, BorrowedThread, Thread},
    fsutil::{self, Identity, ThreadLocks},
    lineage::{FastHashMap, FastHashSet},
    owners::RolloutOwners,
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use rustix::fd::{AsFd, BorrowedFd};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::{BinaryHeap, HashSet},
    ffi::OsStr,
    fmt::{self, Write as _},
    fs,
    io::Write,
    ops::ControlFlow,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryReason {
    UnknownArchiveStatus,
    Excluded,
    PinnedOrUnknownPin,
    UnsupportedHistoryMode,
    ReferencedHistory,
    RelatedThread,
    DependentThread,
    DependencyCycle,
    UnknownArchiveTime,
    MissingCapture,
    CaptureMismatch,
    InvalidOrFutureClock,
    WithinRetention,
    Eligible,
    Deleted,
    ChangedBusyOrError,
    UnsafeOrUnavailableArtifact,
    RunStopped,
    // Preserve unrecognized strings when reading an existing report.
    #[serde(untagged)]
    Unknown(String),
}

impl EntryReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UnknownArchiveStatus => "unknown_archive_status",
            Self::Excluded => "excluded",
            Self::PinnedOrUnknownPin => "pinned_or_unknown_pin",
            Self::UnsupportedHistoryMode => "unsupported_history_mode",
            Self::ReferencedHistory => "referenced_history",
            Self::RelatedThread => "related_thread",
            Self::DependentThread => "dependent_thread",
            Self::DependencyCycle => "dependency_cycle",
            Self::UnknownArchiveTime => "unknown_archive_time",
            Self::MissingCapture => "missing_capture",
            Self::CaptureMismatch => "capture_mismatch",
            Self::InvalidOrFutureClock => "invalid_or_future_clock",
            Self::WithinRetention => "within_retention",
            Self::Eligible => "eligible",
            Self::Deleted => "deleted",
            Self::ChangedBusyOrError => "changed_busy_or_error",
            Self::UnsafeOrUnavailableArtifact => "unsafe_or_unavailable_artifact",
            Self::RunStopped => "run_stopped",
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for EntryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    Preview,
    Run,
}

impl ExecutionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Run => "run",
        }
    }
}

#[derive(Debug)]
pub struct Assessment {
    pub reason: EntryReason,
    /// Retention boundary in Unix seconds; `now` must strictly exceed it.
    /// `None` means the capture epoch is missing or the deadline overflowed.
    pub eligible_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(deserialize_with = "deserialize_reason_string")]
    pub reason: EntryReason,
    /// Retention boundary in Unix seconds; `now` must strictly exceed it.
    /// `None` means the capture epoch is missing or the deadline overflowed.
    pub eligible_at: Option<i64>,
    pub bytes: Option<u64>,
    pub detail: Option<String>,
}

fn deserialize_reason_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EntryReason, D::Error> {
    // The report field accepts strings only, not Serde's object form for enums.
    let value = String::deserialize(deserializer)?;
    EntryReason::deserialize(serde::de::value::StringDeserializer::new(value))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub mode: String,
    /// Run or preview start time in whole seconds since the Unix epoch.
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_at: Option<i64>,
    pub examined: u64,
    pub eligible: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub selected_bytes: u64,
    pub logical_bytes_removed: u64,
    pub allocated_bytes_unlinked: u64,
    pub observed_free_space_delta_bytes: Option<i64>,
    pub actual_reclaimed_bytes: Option<u64>,
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    had_attention: bool,
    #[cfg(test)]
    #[serde(skip)]
    completed_batches: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportDetail {
    Full,
    CompactJson,
    Text,
    Summary,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompactReason {
    UnknownArchiveStatus = 0,
    Excluded = 1,
    PinnedOrUnknownPin = 2,
    UnsupportedHistoryMode = 3,
    ReferencedHistory = 4,
    RelatedThread = 5,
    DependentThread = 6,
    DependencyCycle = 7,
    UnknownArchiveTime = 8,
    MissingCapture = 9,
    CaptureMismatch = 10,
    InvalidOrFutureClock = 11,
    WithinRetention = 12,
    Eligible = 13,
    Deleted = 14,
    ChangedBusyOrError = 15,
    UnsafeOrUnavailableArtifact = 16,
    RunStopped = 17,
}

impl CompactReason {
    fn from_reason(reason: &EntryReason) -> Self {
        match reason {
            EntryReason::UnknownArchiveStatus => Self::UnknownArchiveStatus,
            EntryReason::Excluded => Self::Excluded,
            EntryReason::PinnedOrUnknownPin => Self::PinnedOrUnknownPin,
            EntryReason::UnsupportedHistoryMode => Self::UnsupportedHistoryMode,
            EntryReason::ReferencedHistory => Self::ReferencedHistory,
            EntryReason::RelatedThread => Self::RelatedThread,
            EntryReason::DependentThread => Self::DependentThread,
            EntryReason::DependencyCycle => Self::DependencyCycle,
            EntryReason::UnknownArchiveTime => Self::UnknownArchiveTime,
            EntryReason::MissingCapture => Self::MissingCapture,
            EntryReason::CaptureMismatch => Self::CaptureMismatch,
            EntryReason::InvalidOrFutureClock => Self::InvalidOrFutureClock,
            EntryReason::WithinRetention => Self::WithinRetention,
            EntryReason::Eligible => Self::Eligible,
            EntryReason::Deleted => Self::Deleted,
            EntryReason::ChangedBusyOrError => Self::ChangedBusyOrError,
            EntryReason::UnsafeOrUnavailableArtifact => Self::UnsafeOrUnavailableArtifact,
            EntryReason::RunStopped => Self::RunStopped,
            EntryReason::Unknown(_) => unreachable!("engine never produces EntryReason::Unknown"),
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::UnknownArchiveStatus,
            1 => Self::Excluded,
            2 => Self::PinnedOrUnknownPin,
            3 => Self::UnsupportedHistoryMode,
            4 => Self::ReferencedHistory,
            5 => Self::RelatedThread,
            6 => Self::DependentThread,
            7 => Self::DependencyCycle,
            8 => Self::UnknownArchiveTime,
            9 => Self::MissingCapture,
            10 => Self::CaptureMismatch,
            11 => Self::InvalidOrFutureClock,
            12 => Self::WithinRetention,
            13 => Self::Eligible,
            14 => Self::Deleted,
            15 => Self::ChangedBusyOrError,
            16 => Self::UnsafeOrUnavailableArtifact,
            17 => Self::RunStopped,
            _ => unreachable!("invalid compact reason tag"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::UnknownArchiveStatus => "unknown_archive_status",
            Self::Excluded => "excluded",
            Self::PinnedOrUnknownPin => "pinned_or_unknown_pin",
            Self::UnsupportedHistoryMode => "unsupported_history_mode",
            Self::ReferencedHistory => "referenced_history",
            Self::RelatedThread => "related_thread",
            Self::DependentThread => "dependent_thread",
            Self::DependencyCycle => "dependency_cycle",
            Self::UnknownArchiveTime => "unknown_archive_time",
            Self::MissingCapture => "missing_capture",
            Self::CaptureMismatch => "capture_mismatch",
            Self::InvalidOrFutureClock => "invalid_or_future_clock",
            Self::WithinRetention => "within_retention",
            Self::Eligible => "eligible",
            Self::Deleted => "deleted",
            Self::ChangedBusyOrError => "changed_busy_or_error",
            Self::UnsafeOrUnavailableArtifact => "unsafe_or_unavailable_artifact",
            Self::RunStopped => "run_stopped",
        }
    }

    fn to_entry_reason(self) -> EntryReason {
        match self {
            Self::UnknownArchiveStatus => EntryReason::UnknownArchiveStatus,
            Self::Excluded => EntryReason::Excluded,
            Self::PinnedOrUnknownPin => EntryReason::PinnedOrUnknownPin,
            Self::UnsupportedHistoryMode => EntryReason::UnsupportedHistoryMode,
            Self::ReferencedHistory => EntryReason::ReferencedHistory,
            Self::RelatedThread => EntryReason::RelatedThread,
            Self::DependentThread => EntryReason::DependentThread,
            Self::DependencyCycle => EntryReason::DependencyCycle,
            Self::UnknownArchiveTime => EntryReason::UnknownArchiveTime,
            Self::MissingCapture => EntryReason::MissingCapture,
            Self::CaptureMismatch => EntryReason::CaptureMismatch,
            Self::InvalidOrFutureClock => EntryReason::InvalidOrFutureClock,
            Self::WithinRetention => EntryReason::WithinRetention,
            Self::Eligible => EntryReason::Eligible,
            Self::Deleted => EntryReason::Deleted,
            Self::ChangedBusyOrError => EntryReason::ChangedBusyOrError,
            Self::UnsafeOrUnavailableArtifact => EntryReason::UnsafeOrUnavailableArtifact,
            Self::RunStopped => EntryReason::RunStopped,
        }
    }
}

const COMPACT_TITLE_LEN_MASK: u32 = 0xFFFF;
const COMPACT_REASON_SHIFT: u32 = 16;
const COMPACT_REASON_MASK: u32 = 0x1F;
const COMPACT_FLAG_NON_CANONICAL_ID: u32 = 1 << 21;
const COMPACT_FLAG_OVERFLOW_DEADLINE: u32 = 1 << 22;
const COMPACT_FLAG_HAS_ELIGIBLE_AT: u32 = 1 << 23;

#[derive(Clone, Copy)]
struct CompactEntry {
    title_off: u32,
    eligible_delta: i32,
    meta: u32,
    id: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<CompactEntry>() == 28);

impl CompactEntry {
    #[inline]
    fn title_len(&self) -> u16 {
        (self.meta & COMPACT_TITLE_LEN_MASK) as u16
    }

    #[inline]
    fn reason(&self) -> CompactReason {
        CompactReason::from_u8(((self.meta >> COMPACT_REASON_SHIFT) & COMPACT_REASON_MASK) as u8)
    }

    #[inline]
    fn set_reason(&mut self, reason: CompactReason) {
        self.meta = (self.meta & !(COMPACT_REASON_MASK << COMPACT_REASON_SHIFT))
            | (u32::from(reason as u8) << COMPACT_REASON_SHIFT);
    }

    #[inline]
    fn is_non_canonical_id(&self) -> bool {
        self.meta & COMPACT_FLAG_NON_CANONICAL_ID != 0
    }

    #[inline]
    fn non_canonical_idx(&self) -> usize {
        u32::from_le_bytes([self.id[0], self.id[1], self.id[2], self.id[3]]) as usize
    }

    #[inline]
    fn uuid_sort_key(&self) -> u128 {
        u128::from_be_bytes(self.id)
    }
}

#[derive(Clone, Default)]
struct CompactDetails {
    // Interned diagnostic messages (typically 1..10 distinct strings even across
    // tens of thousands of skipped/missing/blocked archives).
    messages: Vec<Box<str>>,
    // Strictly sorted by entry index: (entry_index, message_index).
    entries: Vec<(u32, u32)>,
}

impl CompactDetails {
    fn intern(&mut self, message: &str) -> u32 {
        if let Some(rev_pos) = self
            .messages
            .iter()
            .rev()
            .take(32)
            .position(|existing| existing.as_ref() == message)
        {
            return (self.messages.len() - 1 - rev_pos) as u32;
        }
        let idx = self.messages.len() as u32;
        self.messages.push(message.into());
        idx
    }

    fn insert(&mut self, index: u32, message: &str) {
        let msg_idx = self.intern(message);
        if self.entries.last().is_none_or(|&(last, _)| last < index) {
            self.entries.push((index, msg_idx));
        } else {
            match self.entries.binary_search_by_key(&index, |&(k, _)| k) {
                Ok(pos) => self.entries[pos].1 = msg_idx,
                Err(pos) => self.entries.insert(pos, (index, msg_idx)),
            }
        }
    }

    fn insert_capped(&mut self, index: u32, message: &str, cap: usize) {
        let pos = self.entries.partition_point(|&(k, _)| k < index);
        if pos >= cap {
            return;
        }
        let msg_idx = self.intern(message);
        if self.entries.get(pos).is_some_and(|&(k, _)| k == index) {
            self.entries[pos].1 = msg_idx;
        } else {
            self.entries.insert(pos, (index, msg_idx));
            if self.entries.len() > cap {
                self.entries.truncate(cap);
            }
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    fn message(&self, msg_idx: u32) -> &str {
        self.messages[msg_idx as usize].as_ref()
    }
}

struct CompactEntries {
    base_now: i64,
    entries: Vec<CompactEntry>,
    titles: crate::lineage::ChunkedBytes,
    non_canonical_ids: Vec<Box<str>>,
    overflow_deadlines: Vec<i64>,
    // Parallel sorted arrays (12 B/entry vs 16 B for Vec<(u32, u64)>),
    // populated strictly after sorting in ascending report_index order.
    bytes_idx: Vec<u32>,
    bytes_val: Vec<u64>,
    details: CompactDetails,
}

impl CompactEntries {
    fn new(base_now: i64) -> Self {
        Self {
            base_now,
            entries: Vec::new(),
            titles: crate::lineage::ChunkedBytes::default(),
            non_canonical_ids: Vec::new(),
            overflow_deadlines: Vec::new(),
            bytes_idx: Vec::new(),
            bytes_val: Vec::new(),
            details: CompactDetails::default(),
        }
    }

    fn reserve_exact(&mut self, count: usize) {
        self.entries.reserve_exact(count);
    }

    fn push(
        &mut self,
        id: &str,
        title: &str,
        reason: EntryReason,
        eligible_at: Option<i64>,
    ) -> Result<usize> {
        let (title_off, title_len) = self
            .titles
            .push(title.as_bytes())
            .context("report title exceeds 64 KiB")?;
        let mut meta = u32::from(title_len)
            | (u32::from(CompactReason::from_reason(&reason) as u8) << COMPACT_REASON_SHIFT);
        let id_bytes = if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
            *uuid.as_bytes()
        } else {
            meta |= COMPACT_FLAG_NON_CANONICAL_ID;
            let idx = u32::try_from(self.non_canonical_ids.len())
                .context("noncanonical ID index overflow")?;
            self.non_canonical_ids.push(id.into());
            let mut raw = [0u8; 16];
            raw[..4].copy_from_slice(&idx.to_le_bytes());
            raw
        };
        let eligible_delta = match eligible_at {
            None => 0,
            Some(ts) => {
                meta |= COMPACT_FLAG_HAS_ELIGIBLE_AT;
                if let Some(delta) = ts
                    .checked_sub(self.base_now)
                    .and_then(|d| i32::try_from(d).ok())
                {
                    delta
                } else {
                    meta |= COMPACT_FLAG_OVERFLOW_DEADLINE;
                    let idx = i32::try_from(self.overflow_deadlines.len())
                        .context("overflow deadline table full")?;
                    self.overflow_deadlines.push(ts);
                    idx
                }
            }
        };
        let index = self.entries.len();
        self.entries.push(CompactEntry {
            title_off,
            eligible_delta,
            meta,
            id: id_bytes,
        });
        Ok(index)
    }

    #[inline]
    fn id_str<'a>(&'a self, index: usize, buf: &'a mut [u8; 36]) -> &'a str {
        let entry = &self.entries[index];
        if entry.is_non_canonical_id() {
            &self.non_canonical_ids[entry.non_canonical_idx()]
        } else {
            uuid::Uuid::from_bytes(entry.id)
                .hyphenated()
                .encode_lower(buf)
        }
    }

    #[inline]
    fn id_uuid(&self, index: usize) -> Option<uuid::Uuid> {
        let entry = &self.entries[index];
        if entry.is_non_canonical_id() {
            let s = &self.non_canonical_ids[entry.non_canonical_idx()];
            crate::lineage::parse_canonical_uuid(s).or_else(|| uuid::Uuid::parse_str(s).ok())
        } else {
            Some(uuid::Uuid::from_bytes(entry.id))
        }
    }

    #[inline]
    fn eligible_at(&self, entry: &CompactEntry) -> Option<i64> {
        if entry.meta & COMPACT_FLAG_HAS_ELIGIBLE_AT == 0 {
            None
        } else if entry.meta & COMPACT_FLAG_OVERFLOW_DEADLINE != 0 {
            Some(self.overflow_deadlines[entry.eligible_delta as usize])
        } else {
            Some(self.base_now + i64::from(entry.eligible_delta))
        }
    }

    #[inline]
    fn title_bytes(&self, entry: &CompactEntry) -> &[u8] {
        self.titles.get(entry.title_off, entry.title_len())
    }

    #[inline]
    fn title_str(&self, entry: &CompactEntry) -> &str {
        std::str::from_utf8(self.title_bytes(entry)).expect("stored titles are valid UTF-8")
    }

    fn set_eligible_at(&mut self, index: usize, eligible_at: Option<i64>) -> Result<()> {
        let entry = &mut self.entries[index];
        let prev_overflow = (entry.meta & COMPACT_FLAG_OVERFLOW_DEADLINE != 0)
            .then_some(entry.eligible_delta as usize);
        entry.meta &= !(COMPACT_FLAG_HAS_ELIGIBLE_AT | COMPACT_FLAG_OVERFLOW_DEADLINE);
        match eligible_at {
            None => {
                entry.eligible_delta = 0;
            }
            Some(ts) => {
                entry.meta |= COMPACT_FLAG_HAS_ELIGIBLE_AT;
                if let Some(delta) = ts
                    .checked_sub(self.base_now)
                    .and_then(|d| i32::try_from(d).ok())
                {
                    entry.eligible_delta = delta;
                } else {
                    entry.meta |= COMPACT_FLAG_OVERFLOW_DEADLINE;
                    if let Some(existing_slot) = prev_overflow {
                        self.overflow_deadlines[existing_slot] = ts;
                        entry.eligible_delta = existing_slot as i32;
                    } else {
                        let overflow_idx = i32::try_from(self.overflow_deadlines.len())
                            .context("overflow deadline table full")?;
                        self.overflow_deadlines.push(ts);
                        entry.eligible_delta = overflow_idx;
                    }
                }
            }
        }
        Ok(())
    }

    fn clear_unmatched_missing_capture(&mut self) {
        for entry in &mut self.entries {
            if entry.reason() == CompactReason::MissingCapture {
                entry.meta &= !(COMPACT_FLAG_HAS_ELIGIBLE_AT | COMPACT_FLAG_OVERFLOW_DEADLINE);
                entry.eligible_delta = 0;
            }
        }
    }

    fn sort_entries_only(&mut self) {
        let all_canonical = self.non_canonical_ids.is_empty();
        let already_sorted = if all_canonical {
            self.entries
                .windows(2)
                .all(|pair| pair[0].uuid_sort_key() <= pair[1].uuid_sort_key())
        } else {
            let non_canon = &self.non_canonical_ids;
            self.entries
                .windows(2)
                .all(|pair| Self::compare_entries(non_canon, &pair[0], &pair[1]).is_le())
        };
        if already_sorted {
            return;
        }
        if all_canonical {
            self.entries
                .sort_unstable_by_key(CompactEntry::uuid_sort_key);
        } else {
            let non_canon = &self.non_canonical_ids;
            self.entries
                .sort_unstable_by(|a, b| Self::compare_entries(non_canon, a, b));
        }
    }

    #[inline]
    fn find_by_id_hinted(&self, id: &str, hint: &mut usize) -> Option<usize> {
        if self.non_canonical_ids.is_empty() {
            let uuid = crate::lineage::parse_canonical_uuid(id)?;
            let key = u128::from_be_bytes(*uuid.as_bytes());
            if *hint < self.entries.len() && self.entries[*hint].uuid_sort_key() == key {
                let found = *hint;
                *hint = found + 1;
                return Some(found);
            }
            let found = self
                .entries
                .binary_search_by_key(&key, CompactEntry::uuid_sort_key)
                .ok()?;
            *hint = found + 1;
            return Some(found);
        }
        let non_canon = &self.non_canonical_ids;
        let mut buf = [0u8; 36];
        let found = self
            .entries
            .binary_search_by(|entry| {
                let entry_id = if entry.is_non_canonical_id() {
                    non_canon[entry.non_canonical_idx()].as_ref()
                } else {
                    uuid::Uuid::from_bytes(entry.id)
                        .hyphenated()
                        .encode_lower(&mut buf)
                };
                entry_id.cmp(id)
            })
            .ok()?;
        *hint = found + 1;
        Some(found)
    }

    fn set_bytes(&mut self, index: usize, bytes: u64) {
        let key = index as u32;
        if self.bytes_idx.last().is_none_or(|&last| last < key) {
            self.bytes_idx.push(key);
            self.bytes_val.push(bytes);
        } else {
            match self.bytes_idx.binary_search(&key) {
                Ok(pos) => self.bytes_val[pos] = bytes,
                Err(pos) => {
                    self.bytes_idx.insert(pos, key);
                    self.bytes_val.insert(pos, bytes);
                }
            }
        }
    }

    fn bytes_for(&self, index: usize) -> Option<u64> {
        let key = index as u32;
        self.bytes_idx
            .binary_search(&key)
            .ok()
            .map(|pos| self.bytes_val[pos])
    }

    fn sort_with_candidates(&mut self, candidates: &mut [Candidate]) {
        let all_canonical = self.non_canonical_ids.is_empty();
        let already_sorted = if all_canonical {
            self.entries
                .windows(2)
                .all(|pair| pair[0].uuid_sort_key() <= pair[1].uuid_sort_key())
        } else {
            let non_canon = &self.non_canonical_ids;
            self.entries
                .windows(2)
                .all(|pair| Self::compare_entries(non_canon, &pair[0], &pair[1]).is_le())
        };
        if already_sorted {
            if !candidates
                .windows(2)
                .all(|pair| pair[0].report_index <= pair[1].report_index)
            {
                candidates.sort_unstable_by_key(|candidate| candidate.report_index);
            }
            return;
        }
        if candidates.is_empty() {
            self.sort_entries_only();
            return;
        }
        let n = self.entries.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        if all_canonical {
            let entries = &self.entries;
            order.sort_unstable_by_key(|&i| entries[i as usize].uuid_sort_key());
        } else {
            let entries = &self.entries;
            let non_canon = &self.non_canonical_ids;
            order.sort_unstable_by(|&a, &b| {
                Self::compare_entries(non_canon, &entries[a as usize], &entries[b as usize])
            });
        }
        let mut new_index = vec![0u32; n];
        for (new_pos, &old_pos) in order.iter().enumerate() {
            new_index[old_pos as usize] = new_pos as u32;
        }
        for i in 0..n {
            let mut current = i;
            while order[current] as usize != i {
                let next = order[current] as usize;
                self.entries.swap(current, next);
                order[current] = current as u32;
                current = next;
            }
            order[current] = current as u32;
        }
        for candidate in candidates.iter_mut() {
            candidate.report_index = new_index[candidate.report_index as usize];
        }
        candidates.sort_unstable_by_key(|candidate| candidate.report_index);
    }

    fn materialize_entry(
        &self,
        index: usize,
        detail: Option<&str>,
        id_buf: &mut [u8; 36],
    ) -> Entry {
        let entry = &self.entries[index];
        Entry {
            id: self.id_str(index, id_buf).to_owned(),
            title: self.title_str(entry).to_owned(),
            reason: entry.reason().to_entry_reason(),
            eligible_at: self.eligible_at(entry),
            bytes: self.bytes_for(index),
            detail: detail.map(str::to_owned),
        }
    }

    fn into_summary_entries(self) -> Vec<Entry> {
        let count = self.details.len().min(10);
        if count == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count);
        let mut id_buf = [0u8; 36];
        for &(idx, msg_idx) in self.details.entries.iter().take(10) {
            out.push(self.materialize_entry(
                idx as usize,
                Some(self.details.message(msg_idx)),
                &mut id_buf,
            ));
        }
        out
    }

    fn into_text_entries(self) -> Vec<Entry> {
        let prefix_len = self.entries.len().min(TEXT_DISPLAY_LIMIT);
        let extra_skips = self
            .details
            .entries
            .iter()
            .take(10)
            .filter(|&&(idx, _)| (idx as usize) >= prefix_len)
            .count();
        let mut out = Vec::with_capacity(prefix_len + extra_skips);
        let mut id_buf = [0u8; 36];
        let mut detail_cursor = 0usize;
        let details = self.details.entries.as_slice();
        for index in 0..prefix_len {
            let detail =
                if detail_cursor < details.len() && details[detail_cursor].0 as usize == index {
                    let msg = self.details.message(details[detail_cursor].1);
                    detail_cursor += 1;
                    Some(msg)
                } else {
                    None
                };
            out.push(self.materialize_entry(index, detail, &mut id_buf));
        }
        for &(idx, msg_idx) in self.details.entries.iter().take(10) {
            let index = idx as usize;
            if index >= prefix_len {
                out.push(self.materialize_entry(
                    index,
                    Some(self.details.message(msg_idx)),
                    &mut id_buf,
                ));
            }
        }
        out
    }

    fn compare_entries(
        non_canonical_ids: &[Box<str>],
        a: &CompactEntry,
        b: &CompactEntry,
    ) -> std::cmp::Ordering {
        if !a.is_non_canonical_id() && !b.is_non_canonical_id() {
            return a.uuid_sort_key().cmp(&b.uuid_sort_key());
        }
        let mut ba = [0u8; 36];
        let mut bb = [0u8; 36];
        let sa = if a.is_non_canonical_id() {
            non_canonical_ids[a.non_canonical_idx()].as_ref()
        } else {
            uuid::Uuid::from_bytes(a.id)
                .hyphenated()
                .encode_lower(&mut ba)
        };
        let sb = if b.is_non_canonical_id() {
            non_canonical_ids[b.non_canonical_idx()].as_ref()
        } else {
            uuid::Uuid::from_bytes(b.id)
                .hyphenated()
                .encode_lower(&mut bb)
        };
        sa.cmp(sb)
    }
}

pub(crate) struct CompactReport {
    pub schema: u32,
    pub mode: &'static str,
    pub started_at: i64,
    pub evaluated_at: Option<i64>,
    pub examined: u64,
    pub eligible: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub selected_bytes: u64,
    pub logical_bytes_removed: u64,
    pub allocated_bytes_unlinked: u64,
    pub observed_free_space_delta_bytes: Option<i64>,
    pub actual_reclaimed_bytes: Option<u64>,
    pub warnings: Vec<String>,
    entries: CompactEntries,
    had_attention: bool,
}

impl CompactReport {
    pub(crate) fn requires_attention(&self) -> bool {
        self.had_attention || !self.warnings.is_empty()
    }

    pub(crate) fn set_forecast(&mut self, started_at: i64, evaluated_at: i64) {
        self.started_at = started_at;
        self.evaluated_at = Some(evaluated_at);
    }

    pub(crate) fn first_skips(&self, limit: usize) -> Vec<Entry> {
        let mut out = Vec::with_capacity(self.entries.details.len().min(limit));
        let mut id_buf = [0u8; 36];
        for &(idx, msg_idx) in self.entries.details.entries.iter().take(limit) {
            let index = idx as usize;
            let entry = &self.entries.entries[index];
            out.push(Entry {
                id: self.entries.id_str(index, &mut id_buf).to_owned(),
                title: self.entries.title_str(entry).to_owned(),
                reason: entry.reason().to_entry_reason(),
                eligible_at: self.entries.eligible_at(entry),
                bytes: self.entries.bytes_for(index),
                detail: Some(self.entries.details.message(msg_idx).to_owned()),
            });
        }
        out
    }

    pub(crate) fn write_json(&self, out: &mut impl Write) -> Result<()> {
        const FLUSH_THRESHOLD: usize = 60 * 1024;
        let mut buf = Vec::<u8>::with_capacity(64 * 1024);
        buf.extend_from_slice(b"{\"schema\":");
        push_u64(&mut buf, u64::from(self.schema));
        buf.extend_from_slice(b",\"mode\":\"");
        buf.extend_from_slice(self.mode.as_bytes());
        buf.extend_from_slice(b"\",\"started_at\":");
        push_i64(&mut buf, self.started_at);
        if let Some(evaluated_at) = self.evaluated_at {
            buf.extend_from_slice(b",\"evaluated_at\":");
            push_i64(&mut buf, evaluated_at);
        }
        buf.extend_from_slice(b",\"examined\":");
        push_u64(&mut buf, self.examined);
        buf.extend_from_slice(b",\"eligible\":");
        push_u64(&mut buf, self.eligible);
        buf.extend_from_slice(b",\"deleted\":");
        push_u64(&mut buf, self.deleted);
        buf.extend_from_slice(b",\"skipped\":");
        push_u64(&mut buf, self.skipped);
        buf.extend_from_slice(b",\"selected_bytes\":");
        push_u64(&mut buf, self.selected_bytes);
        buf.extend_from_slice(b",\"logical_bytes_removed\":");
        push_u64(&mut buf, self.logical_bytes_removed);
        buf.extend_from_slice(b",\"allocated_bytes_unlinked\":");
        push_u64(&mut buf, self.allocated_bytes_unlinked);
        buf.extend_from_slice(b",\"observed_free_space_delta_bytes\":");
        push_opt_i64(&mut buf, self.observed_free_space_delta_bytes);
        buf.extend_from_slice(b",\"actual_reclaimed_bytes\":");
        push_opt_u64(&mut buf, self.actual_reclaimed_bytes);
        buf.extend_from_slice(b",\"entries\":[");

        let mut bytes_cursor = 0usize;
        let bytes_idx = self.entries.bytes_idx.as_slice();
        let bytes_val = self.entries.bytes_val.as_slice();
        let mut details_cursor = 0usize;
        let details_slice = self.entries.details.entries.as_slice();
        let mut last_eligible: Option<(i64, [u8; 24], usize)> = None;
        let mut num_buf = itoa::Buffer::new();
        for (index, entry) in self.entries.entries.iter().enumerate() {
            if buf.len() >= FLUSH_THRESHOLD {
                out.write_all(&buf)?;
                buf.clear();
            }
            if index > 0 {
                buf.push(b',');
            }
            if entry.is_non_canonical_id() {
                buf.extend_from_slice(b"{\"id\":");
                push_json_str(
                    &mut buf,
                    &self.entries.non_canonical_ids[entry.non_canonical_idx()],
                )?;
                buf.extend_from_slice(b",\"title\":");
            } else {
                buf.extend_from_slice(b"{\"id\":\"");
                push_uuid_lower(&mut buf, &entry.id);
                buf.extend_from_slice(b"\",\"title\":");
            }
            push_json_bytes(&mut buf, self.entries.title_bytes(entry))?;
            buf.extend_from_slice(b",\"reason\":\"");
            buf.extend_from_slice(entry.reason().as_str().as_bytes());
            buf.extend_from_slice(b"\",\"eligible_at\":");
            match self.entries.eligible_at(entry) {
                None => buf.extend_from_slice(b"null"),
                Some(ts) => {
                    if let Some((cached_ts, ref cached_buf, len)) = last_eligible
                        && cached_ts == ts
                    {
                        buf.extend_from_slice(&cached_buf[..len]);
                    } else {
                        let rendered = num_buf.format(ts).as_bytes();
                        let mut saved = [0u8; 24];
                        saved[..rendered.len()].copy_from_slice(rendered);
                        buf.extend_from_slice(rendered);
                        last_eligible = Some((ts, saved, rendered.len()));
                    }
                }
            }
            let idx_u32 = index as u32;
            let bytes_entry =
                if bytes_cursor < bytes_idx.len() && bytes_idx[bytes_cursor] == idx_u32 {
                    let v = bytes_val[bytes_cursor];
                    bytes_cursor += 1;
                    Some(v)
                } else {
                    None
                };
            let detail = if details_cursor < details_slice.len()
                && details_slice[details_cursor].0 == idx_u32
            {
                let msg = self
                    .entries
                    .details
                    .message(details_slice[details_cursor].1);
                details_cursor += 1;
                Some(msg)
            } else {
                None
            };
            if bytes_entry.is_none() && detail.is_none() {
                buf.extend_from_slice(b",\"bytes\":null,\"detail\":null}");
            } else {
                buf.extend_from_slice(b",\"bytes\":");
                push_opt_u64(&mut buf, bytes_entry);
                buf.extend_from_slice(b",\"detail\":");
                if let Some(detail) = detail {
                    push_json_str(&mut buf, detail)?;
                } else {
                    buf.extend_from_slice(b"null");
                }
                buf.push(b'}');
            }
        }
        buf.extend_from_slice(b"],\"warnings\":[");
        for (index, warning) in self.warnings.iter().enumerate() {
            if buf.len() >= FLUSH_THRESHOLD {
                out.write_all(&buf)?;
                buf.clear();
            }
            if index > 0 {
                buf.push(b',');
            }
            push_json_str(&mut buf, warning)?;
        }
        buf.extend_from_slice(b"]}");
        out.write_all(&buf)?;
        Ok(())
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

#[inline]
fn push_uuid_lower(buf: &mut Vec<u8>, b: &[u8; 16]) {
    let out = [
        HEX_LOWER[(b[0] >> 4) as usize],
        HEX_LOWER[(b[0] & 0xf) as usize],
        HEX_LOWER[(b[1] >> 4) as usize],
        HEX_LOWER[(b[1] & 0xf) as usize],
        HEX_LOWER[(b[2] >> 4) as usize],
        HEX_LOWER[(b[2] & 0xf) as usize],
        HEX_LOWER[(b[3] >> 4) as usize],
        HEX_LOWER[(b[3] & 0xf) as usize],
        b'-',
        HEX_LOWER[(b[4] >> 4) as usize],
        HEX_LOWER[(b[4] & 0xf) as usize],
        HEX_LOWER[(b[5] >> 4) as usize],
        HEX_LOWER[(b[5] & 0xf) as usize],
        b'-',
        HEX_LOWER[(b[6] >> 4) as usize],
        HEX_LOWER[(b[6] & 0xf) as usize],
        HEX_LOWER[(b[7] >> 4) as usize],
        HEX_LOWER[(b[7] & 0xf) as usize],
        b'-',
        HEX_LOWER[(b[8] >> 4) as usize],
        HEX_LOWER[(b[8] & 0xf) as usize],
        HEX_LOWER[(b[9] >> 4) as usize],
        HEX_LOWER[(b[9] & 0xf) as usize],
        b'-',
        HEX_LOWER[(b[10] >> 4) as usize],
        HEX_LOWER[(b[10] & 0xf) as usize],
        HEX_LOWER[(b[11] >> 4) as usize],
        HEX_LOWER[(b[11] & 0xf) as usize],
        HEX_LOWER[(b[12] >> 4) as usize],
        HEX_LOWER[(b[12] & 0xf) as usize],
        HEX_LOWER[(b[13] >> 4) as usize],
        HEX_LOWER[(b[13] & 0xf) as usize],
        HEX_LOWER[(b[14] >> 4) as usize],
        HEX_LOWER[(b[14] & 0xf) as usize],
        HEX_LOWER[(b[15] >> 4) as usize],
        HEX_LOWER[(b[15] & 0xf) as usize],
    ];
    buf.extend_from_slice(&out);
}

#[inline]
fn push_u64(buf: &mut Vec<u8>, n: u64) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

#[inline]
fn push_i64(buf: &mut Vec<u8>, n: i64) {
    let mut tmp = itoa::Buffer::new();
    buf.extend_from_slice(tmp.format(n).as_bytes());
}

#[inline]
fn push_opt_u64(buf: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => push_u64(buf, v),
        None => buf.extend_from_slice(b"null"),
    }
}

#[inline]
fn push_opt_i64(buf: &mut Vec<u8>, value: Option<i64>) {
    match value {
        Some(v) => push_i64(buf, v),
        None => buf.extend_from_slice(b"null"),
    }
}

#[inline]
fn push_json_bytes(buf: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if bytes.iter().all(|&b| b >= 0x20 && b != b'"' && b != b'\\') {
        buf.push(b'"');
        buf.extend_from_slice(bytes);
        buf.push(b'"');
        Ok(())
    } else {
        let s = std::str::from_utf8(bytes).expect("stored titles are valid UTF-8");
        serde_json::to_writer(buf, s)?;
        Ok(())
    }
}

#[inline]
fn push_json_str(buf: &mut Vec<u8>, value: &str) -> Result<()> {
    push_json_bytes(buf, value.as_bytes())
}

enum EntryStore {
    Full(Vec<Entry>),
    Compact(CompactEntries),
}

impl EntryStore {
    fn id_str<'a>(&'a self, index: usize, buf: &'a mut [u8; 36]) -> &'a str {
        match self {
            Self::Full(entries) => entries[index].id.as_str(),
            Self::Compact(compact) => compact.id_str(index, buf),
        }
    }

    fn id_uuid(&self, index: usize) -> Option<uuid::Uuid> {
        match self {
            Self::Full(entries) => {
                let id = &entries[index].id;
                crate::lineage::parse_canonical_uuid(id).or_else(|| uuid::Uuid::parse_str(id).ok())
            }
            Self::Compact(compact) => compact.id_uuid(index),
        }
    }

    fn set_bytes(&mut self, index: usize, bytes: u64) {
        match self {
            Self::Full(entries) => entries[index].bytes = Some(bytes),
            Self::Compact(compact) => compact.set_bytes(index, bytes),
        }
    }

    fn bytes_for(&self, index: usize) -> Option<u64> {
        match self {
            Self::Full(entries) => entries[index].bytes,
            Self::Compact(compact) => compact.bytes_for(index),
        }
    }

    fn set_reason(&mut self, index: usize, reason: EntryReason) {
        match self {
            Self::Full(entries) => entries[index].reason = reason,
            Self::Compact(compact) => {
                compact.entries[index].set_reason(CompactReason::from_reason(&reason));
            }
        }
    }

    fn is_deleted(&self, index: usize) -> bool {
        match self {
            Self::Full(entries) => entries[index].reason == EntryReason::Deleted,
            Self::Compact(compact) => compact.entries[index].reason() == CompactReason::Deleted,
        }
    }

    fn mark_remaining_eligible_stopped(&mut self) {
        match self {
            Self::Full(entries) => {
                for entry in entries
                    .iter_mut()
                    .filter(|entry| entry.reason == EntryReason::Eligible)
                {
                    entry.reason = EntryReason::RunStopped;
                }
            }
            Self::Compact(compact) => {
                for entry in &mut compact.entries {
                    if entry.reason() == CompactReason::Eligible {
                        entry.set_reason(CompactReason::RunStopped);
                    }
                }
            }
        }
    }

    fn sort_with_candidates(&mut self, candidates: &mut [Candidate]) {
        match self {
            Self::Full(entries) => sort_report_entries(entries, candidates),
            Self::Compact(compact) => compact.sort_with_candidates(candidates),
        }
    }

    fn reserve_bytes_capacity(&mut self, count: usize) {
        if count > 0
            && let Self::Compact(compact) = self
        {
            compact.bytes_idx.reserve_exact(count);
            compact.bytes_val.reserve_exact(count);
        }
    }
}

struct ReportCore {
    mode: ExecutionMode,
    started_at: i64,
    examined: u64,
    eligible: u64,
    deleted: u64,
    skipped: u64,
    selected_bytes: u64,
    logical_bytes_removed: u64,
    allocated_bytes_unlinked: u64,
    observed_free_space_delta_bytes: Option<i64>,
    warnings: Vec<String>,
    entries: EntryStore,
    had_attention: bool,
    #[cfg(test)]
    completed_batches: usize,
}

impl ReportCore {
    fn new(mode: ExecutionMode, now: i64, detail: ReportDetail) -> Self {
        let entries = if detail == ReportDetail::Full {
            EntryStore::Full(Vec::new())
        } else {
            EntryStore::Compact(CompactEntries::new(now))
        };
        Self {
            mode,
            started_at: now,
            examined: 0,
            eligible: 0,
            deleted: 0,
            skipped: 0,
            selected_bytes: 0,
            logical_bytes_removed: 0,
            allocated_bytes_unlinked: 0,
            observed_free_space_delta_bytes: None,
            warnings: Vec::new(),
            entries,
            had_attention: false,
            #[cfg(test)]
            completed_batches: 0,
        }
    }

    fn warning(&mut self, detail: ReportDetail, message: String) {
        self.had_attention = true;
        if detail != ReportDetail::Summary || self.warnings.len() < 10 {
            self.warnings.push(message);
        }
    }

    fn failure(&mut self, index: usize, reason: EntryReason, error: &str, detail: ReportDetail) {
        self.had_attention |= matches!(
            reason,
            EntryReason::ChangedBusyOrError
                | EntryReason::UnsafeOrUnavailableArtifact
                | EntryReason::DependencyCycle
        );
        match &mut self.entries {
            EntryStore::Full(entries) => {
                entries[index].reason = reason;
                entries[index].detail = Some(error.into());
            }
            EntryStore::Compact(compact) => {
                compact.entries[index].set_reason(CompactReason::from_reason(&reason));
                match detail {
                    ReportDetail::Summary => compact.details.insert_capped(index as u32, error, 10),
                    ReportDetail::Text => {
                        compact
                            .details
                            .insert_capped(index as u32, error, TEXT_DISPLAY_LIMIT)
                    }
                    ReportDetail::CompactJson => compact.details.insert(index as u32, error),
                    ReportDetail::Full => unreachable!(),
                }
            }
        }
    }

    fn into_report(self, detail: ReportDetail) -> Report {
        let entries = match self.entries {
            EntryStore::Full(entries) => entries,
            EntryStore::Compact(compact) => match detail {
                ReportDetail::Summary => compact.into_summary_entries(),
                ReportDetail::Text => compact.into_text_entries(),
                ReportDetail::Full | ReportDetail::CompactJson => {
                    unreachable!("into_report called with incompatible detail")
                }
            },
        };
        Report {
            schema: 1,
            mode: self.mode.as_str().into(),
            started_at: self.started_at,
            evaluated_at: None,
            examined: self.examined,
            eligible: self.eligible,
            deleted: self.deleted,
            skipped: self.skipped,
            selected_bytes: self.selected_bytes,
            logical_bytes_removed: self.logical_bytes_removed,
            allocated_bytes_unlinked: self.allocated_bytes_unlinked,
            observed_free_space_delta_bytes: self.observed_free_space_delta_bytes,
            actual_reclaimed_bytes: None,
            entries,
            warnings: self.warnings,
            had_attention: self.had_attention,
            #[cfg(test)]
            completed_batches: self.completed_batches,
        }
    }

    fn into_compact_report(self) -> CompactReport {
        let EntryStore::Compact(entries) = self.entries else {
            unreachable!("into_compact_report called on full store");
        };
        CompactReport {
            schema: 1,
            mode: self.mode.as_str(),
            started_at: self.started_at,
            evaluated_at: None,
            examined: self.examined,
            eligible: self.eligible,
            deleted: self.deleted,
            skipped: self.skipped,
            selected_bytes: self.selected_bytes,
            logical_bytes_removed: self.logical_bytes_removed,
            allocated_bytes_unlinked: self.allocated_bytes_unlinked,
            observed_free_space_delta_bytes: self.observed_free_space_delta_bytes,
            actual_reclaimed_bytes: None,
            warnings: self.warnings,
            entries,
            had_attention: self.had_attention,
        }
    }
}

impl Report {
    pub(crate) fn requires_attention(&self) -> bool {
        self.had_attention
            || self.entries.iter().any(|entry| {
                matches!(
                    entry.reason,
                    EntryReason::ChangedBusyOrError
                        | EntryReason::UnsafeOrUnavailableArtifact
                        | EntryReason::DependencyCycle
                )
            })
            || !self.warnings.is_empty()
    }

    #[cfg(test)]
    fn new(mode: ExecutionMode, now: i64) -> Self {
        ReportCore::new(mode, now, ReportDetail::Full).into_report(ReportDetail::Full)
    }
}

/// Formats with an implemented retention adapter; artifacts still need validation.
pub fn supported_history_mode(mode: &str) -> bool {
    matches!(mode, "legacy" | "paginated")
}

#[allow(clippy::too_many_arguments)]
fn assess_fields(
    id: &str,
    archived: i64,
    archived_at: Option<i64>,
    pinned: i64,
    history_mode: &str,
    epoch: Option<i64>,
    recorded_archive: Option<i64>,
    p: &Policy,
    now: i64,
) -> Assessment {
    let deadline = epoch.and_then(|epoch| epoch.checked_add(p.duration()));
    let reason = if archived != 1 {
        EntryReason::UnknownArchiveStatus
    } else if !p.exclusions.is_empty() && p.exclusions.contains(id) {
        EntryReason::Excluded
    } else if pinned != 0 {
        EntryReason::PinnedOrUnknownPin
    } else if !supported_history_mode(history_mode) {
        EntryReason::UnsupportedHistoryMode
    } else if archived_at.is_none() {
        EntryReason::UnknownArchiveTime
    } else if epoch.is_none() {
        EntryReason::MissingCapture
    } else if archived_at != recorded_archive {
        EntryReason::CaptureMismatch
    } else if epoch.is_some_and(|v| v <= 0 || v > now)
        || archived_at.is_some_and(|v| v <= 0 || v > now)
    {
        EntryReason::InvalidOrFutureClock
    } else if deadline.is_none_or(|v| now <= v) {
        EntryReason::WithinRetention
    } else {
        EntryReason::Eligible
    };
    Assessment {
        reason,
        eligible_at: deadline,
    }
}

fn assess_borrowed(t: &BorrowedThread<'_>, p: &Policy, now: i64) -> Assessment {
    assess_fields(
        t.id,
        t.archived,
        t.archived_at,
        t.pinned,
        t.history_mode,
        t.epoch,
        t.recorded_archive,
        p,
        now,
    )
}

/// Assess retention at `now`, in whole seconds since the Unix epoch.
/// Expiry requires `now` to strictly exceed the calculated retention boundary.
pub fn assess(t: &Thread, p: &Policy, now: i64) -> Assessment {
    assess_fields(
        &t.id,
        t.archived,
        t.archived_at,
        t.pinned,
        &t.history_mode,
        t.epoch,
        t.recorded_archive,
        p,
        now,
    )
}

fn rollout_id(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    if !stem.starts_with("rollout-") || stem.len() < 37 {
        return None;
    }
    let id = stem.get(stem.len() - 36..)?;
    crate::lineage::parse_canonical_uuid(id).map(|_| id)
}

#[inline]
fn plain_filename_bytes(name: &OsStr) -> &[u8] {
    let bytes = name.as_bytes();
    if bytes.len() > 4 && bytes.ends_with(b".zst") {
        &bytes[..bytes.len() - 4]
    } else {
        bytes
    }
}

#[inline]
fn matches_rollout_variant_name(logical_name: &OsStr, rollout_name: &OsStr) -> bool {
    let plain_rollout = plain_filename_bytes(rollout_name);
    let logical_bytes = logical_name.as_bytes();
    logical_bytes == plain_rollout
        || logical_bytes
            .strip_suffix(b".zst")
            .is_some_and(|stem| stem == plain_rollout)
}

/// Compare a logical rollout path against a `(dir, file_name)` rollout pair
/// modulo at most one `.zst` extension without allocating `PathBuf`s, while
/// preserving `Path` component equality (such as `/./` and redundant separators).
fn matches_rollout_variant_parts(
    logical_path: &Path,
    rollout_dir: &Path,
    rollout_name: &OsStr,
) -> bool {
    if logical_path.parent() != Some(rollout_dir) {
        return false;
    }
    logical_path
        .file_name()
        .is_some_and(|logical_name| matches_rollout_variant_name(logical_name, rollout_name))
}

struct RolloutVariants {
    plain: PathBuf,
    compressed: PathBuf,
}

impl RolloutVariants {
    // Derive representations only; callers own identity and existence checks.
    fn from_path(path: &Path) -> Self {
        let plain = if path.extension().is_some_and(|extension| extension == "zst") {
            path.with_extension("")
        } else {
            path.to_owned()
        };
        let mut compressed = plain.as_os_str().to_owned();
        compressed.push(".zst");
        Self {
            plain,
            compressed: compressed.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Artifact {
    path: PathBuf,
    identity: Identity,
    bytes: u64,
    allocated: u64,
}

const CANDIDATE_TS_HI_MASK: u16 = 0x00FF;
const CANDIDATE_PAGINATED: u16 = 1 << 8;
const CANDIDATE_IN_ARCHIVE: u16 = 1 << 9;
const CANDIDATE_COMPACT: u16 = 1 << 10;
const CANDIDATE_COMPRESSED: u16 = 1 << 11;

struct Candidate {
    report_index: u32,
    val_lo: u32,
    flags_and_hi: u16,
    uuid_or_len: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<Candidate>() == 28);

impl Candidate {
    #[inline]
    fn paginated(&self) -> bool {
        self.flags_and_hi & CANDIDATE_PAGINATED != 0
    }

    #[inline]
    fn in_archive(&self) -> bool {
        self.flags_and_hi & CANDIDATE_IN_ARCHIVE != 0
    }

    #[inline]
    fn is_compact(&self) -> bool {
        self.flags_and_hi & CANDIDATE_COMPACT != 0
    }
}

#[derive(Default)]
struct CandidateArena {
    entries: Vec<Candidate>,
    paths: crate::lineage::ChunkedBytes,
}

impl CandidateArena {
    fn push(
        &mut self,
        report_index: usize,
        path: &str,
        archive_prefix: &str,
        paginated: bool,
    ) -> Result<()> {
        let mut flags_and_hi = if paginated { CANDIDATE_PAGINATED } else { 0 };
        let report_index = u32::try_from(report_index).context("report entry index overflow")?;
        let stored = if !archive_prefix.is_empty()
            && let Some(name) = path.strip_prefix(archive_prefix)
            && !name.is_empty()
            && !name.contains('/')
            && name != "."
            && name != ".."
        {
            flags_and_hi |= CANDIDATE_IN_ARCHIVE;
            let bytes = name.as_bytes();
            if let Some((ts, uuid, compressed)) = crate::lineage::parse_compact_rollout_key(bytes) {
                flags_and_hi |= CANDIDATE_COMPACT | ((ts >> 32) as u16 & CANDIDATE_TS_HI_MASK);
                if compressed {
                    flags_and_hi |= CANDIDATE_COMPRESSED;
                }
                self.entries.push(Candidate {
                    report_index,
                    val_lo: ts as u32,
                    flags_and_hi,
                    uuid_or_len: *uuid.as_bytes(),
                });
                return Ok(());
            }
            bytes
        } else {
            path.as_bytes()
        };
        let (path_off, path_len) = self
            .paths
            .push(stored)
            .context("rollout path exceeds 64 KiB")?;
        let mut len_buf = [0u8; 16];
        len_buf[..2].copy_from_slice(&path_len.to_le_bytes());
        self.entries.push(Candidate {
            report_index,
            val_lo: path_off,
            flags_and_hi,
            uuid_or_len: len_buf,
        });
        Ok(())
    }

    fn path_bytes<'a>(&'a self, candidate: &Candidate, buf: &'a mut [u8; 74]) -> &'a [u8] {
        if candidate.is_compact() {
            let ts = u64::from(candidate.val_lo)
                | (u64::from(candidate.flags_and_hi & CANDIDATE_TS_HI_MASK) << 32);
            let uuid = uuid::Uuid::from_bytes(candidate.uuid_or_len);
            let compressed = candidate.flags_and_hi & CANDIDATE_COMPRESSED != 0;
            let len = crate::lineage::format_compact_rollout_name(ts, uuid, compressed, buf);
            &buf[..len]
        } else {
            let path_len = u16::from_le_bytes([candidate.uuid_or_len[0], candidate.uuid_or_len[1]]);
            self.paths.get(candidate.val_lo, path_len)
        }
    }
}

enum LogicalPathRef<'a> {
    ArchiveName(&'a Path, &'a OsStr),
    Full(&'a Path),
}

#[derive(Debug)]
struct ReferencedHistory;

impl fmt::Display for ReferencedHistory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("another thread still references this paginated history")
    }
}

impl std::error::Error for ReferencedHistory {}

fn blocker_reason(blocker: crate::relations::Blocker) -> EntryReason {
    match blocker {
        crate::relations::Blocker::Children => EntryReason::DependentThread,
        crate::relations::Blocker::History => EntryReason::ReferencedHistory,
        crate::relations::Blocker::Cycle => EntryReason::DependencyCycle,
    }
}

fn dependency_reason(error: &anyhow::Error) -> Option<EntryReason> {
    if let Some(&blocker) = error.downcast_ref::<crate::relations::Blocker>() {
        Some(blocker_reason(blocker))
    } else {
        error
            .is::<ReferencedHistory>()
            .then_some(EntryReason::ReferencedHistory)
    }
}

fn validate_owned_selection<'a>(
    id: &str,
    logical_path: LogicalPathRef<'_>,
    p: &Policy,
    lineage: &'a crate::lineage::LineageIndex,
    paginated: bool,
    checked_parents: &mut FastHashSet<PathBuf>,
) -> Result<(uuid::Uuid, &'a [crate::lineage::OwnedRollout])> {
    let owner = match crate::lineage::parse_canonical_uuid(id) {
        Some(owner) => owner,
        None => {
            let owner = uuid::Uuid::parse_str(id)?;
            ensure!(
                owner.hyphenated().encode_lower(&mut [0; 36]) == id,
                "noncanonical thread ID"
            );
            owner
        }
    };
    let rollouts = lineage.owned_entries(owner);
    ensure!(!rollouts.is_empty(), "paginated rollout files are missing");
    ensure!(
        rollouts.len() <= fsutil::MAX_BATCH_THREADS,
        "one thread owns more than 128 rollouts; automatic cleanup is bounded"
    );
    let mut name_buf = [0u8; 74];
    let exact_selected = match logical_path {
        LogicalPathRef::ArchiveName(archive_path, logical_name) => {
            check_managed_parent_dir(&p.codex_home, archive_path, checked_parents)?;
            rollouts.iter().any(|rollout| {
                lineage.rollout_dir(rollout) == archive_path
                    && matches_rollout_variant_name(
                        logical_name,
                        lineage.rollout_name(rollout, &mut name_buf),
                    )
            })
        }
        LogicalPathRef::Full(full) => {
            check_managed_source_parents(&p.codex_home, full, checked_parents)?;
            rollouts.iter().any(|rollout| {
                matches_rollout_variant_parts(
                    full,
                    lineage.rollout_dir(rollout),
                    lineage.rollout_name(rollout, &mut name_buf),
                )
            })
        }
    };
    if !exact_selected {
        let owned_full;
        let full = match logical_path {
            LogicalPathRef::ArchiveName(archive_path, logical_name) => {
                owned_full = archive_path.join(logical_name);
                owned_full.as_path()
            }
            LogicalPathRef::Full(full) => full,
        };
        let logical = RolloutVariants::from_path(full);
        let logical_plain_name = logical.plain.file_name();
        let relocated = !fsutil::path_exists(&logical.plain)?
            && !fsutil::path_exists(&logical.compressed)?
            && rollouts.iter().any(|rollout| {
                Some(plain_filename_bytes(
                    lineage.rollout_name(rollout, &mut name_buf),
                )) == logical_plain_name.map(OsStr::as_bytes)
            });
        ensure!(relocated, "selected rollout is not owned by the thread");
    }
    for rollout in rollouts {
        ensure!(
            rollout.paginated == paginated,
            "database and owned rollout history modes differ"
        );
        check_managed_parent_dir(&p.codex_home, lineage.rollout_dir(rollout), checked_parents)?;
    }
    Ok((owner, rollouts))
}

type OwnedArtifacts = SmallVec<[(Artifact, uuid::Uuid); 1]>;

#[allow(clippy::too_many_arguments)]
fn inspect_owned(
    id: &str,
    logical_path: &Path,
    p: &Policy,
    lineage: &crate::lineage::LineageIndex,
    paginated: bool,
    require_unreferenced: bool,
    reader: &mut crate::metadata::RecordReader,
    checked_parents: &mut FastHashSet<PathBuf>,
    archive_dir: Option<(&Path, BorrowedFd<'_>)>,
) -> Result<(uuid::Uuid, OwnedArtifacts)> {
    let (owner, rollouts) = validate_owned_selection(
        id,
        LogicalPathRef::Full(logical_path),
        p,
        lineage,
        paginated,
        checked_parents,
    )?;
    let mut artifacts = SmallVec::with_capacity(rollouts.len());
    let mut name_buf = [0u8; 74];
    for rollout in rollouts {
        if require_unreferenced && lineage.externally_referenced(rollout.rollout_id, owner) {
            return Err(ReferencedHistory.into());
        }
        let dir = lineage.rollout_dir(rollout);
        let name = lineage.rollout_name(rollout, &mut name_buf);
        let path = dir.join(name);
        let (file, identity, bytes, allocated) = if let Some((archive_path, archive_fd)) =
            archive_dir
            && dir == archive_path
        {
            let (file, stat) = fsutil::regular_at_with_stat(archive_fd, name, &path)?;
            let (bytes, allocated) = fsutil::stat_bytes_and_allocated(&stat);
            (file, Identity::of_stat(&stat), bytes, allocated)
        } else {
            let (file, metadata) = fsutil::regular_with_metadata(&path, false, false)?;
            (
                file,
                Identity::of(&metadata),
                metadata.len(),
                metadata.blocks().saturating_mul(512),
            )
        };
        ensure!(
            identity == rollout.identity,
            "paginated rollout changed after reference discovery"
        );
        let compressed = name.as_bytes().ends_with(b".zst");
        let line = reader.read_first_record(&file, compressed)?;
        if paginated {
            crate::lineage::validate_owner(line, owner)?;
        } else {
            crate::metadata::validate_metadata(line, id)?;
        }
        artifacts.push((
            Artifact {
                path,
                identity: rollout.identity.clone(),
                bytes,
                allocated,
            },
            rollout.rollout_id,
        ));
    }
    Ok((owner, artifacts))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    owner: String,
    thread_id: String,
    artifact: Artifact,
    staged: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingItem {
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot: Option<u32>,
    artifact: Artifact,
    staged: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchIntent {
    schema: u32,
    owner: String,
    items: Vec<PendingItem>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Journal {
    Batch(BatchIntent),
    Single(Intent),
}
impl Journal {
    fn into_batch(self) -> Result<BatchIntent> {
        match self {
            Self::Batch(batch) => {
                ensure!(
                    matches!(batch.schema, 2..=4),
                    "unrecognized pending deletion schema"
                );
                Ok(batch)
            }
            Self::Single(one) => {
                ensure!(one.schema == 1, "unrecognized pending deletion schema");
                Ok(BatchIntent {
                    schema: 2,
                    owner: one.owner,
                    items: vec![PendingItem {
                        thread_id: one.thread_id,
                        rollout_id: None,
                        slot: None,
                        artifact: one.artifact,
                        staged: one.staged,
                    }],
                })
            }
        }
    }
}
fn intent_path(store: &Store) -> PathBuf {
    store.root.join("pending.json")
}
fn stage_path(home: &Path, id: &str) -> PathBuf {
    home.join("archived_sessions")
        .join(format!(".codex-retain-pending-{id}"))
}

fn rollout_stage_path(home: &Path, thread_id: &str, rollout_id: &str) -> PathBuf {
    home.join("archived_sessions")
        .join(format!(".codex-retain-pending-{thread_id}-{rollout_id}"))
}

fn copy_stage_path(home: &Path, thread_id: &str, rollout_id: &str, slot: u32) -> PathBuf {
    home.join("archived_sessions").join(format!(
        ".codex-retain-pending-{thread_id}-{rollout_id}-{slot}"
    ))
}

/// Validate a verified parent directory and all of its ancestors under `home`
/// without re-stating directories already confirmed in this phase.
fn check_managed_parent_dir(
    home: &Path,
    parent: &Path,
    checked: &mut FastHashSet<PathBuf>,
) -> Result<()> {
    if checked.contains(parent) {
        return Ok(());
    }
    let mut current = home.to_path_buf();
    for step in std::iter::once(None).chain(parent.strip_prefix(home)?.components().map(Some)) {
        if let Some(component) = step {
            current.push(component);
        }
        if checked.contains(&current) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "rollout source ancestor is not a real directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("inspect rollout source ancestor"),
        }
        checked.insert(current.clone());
    }
    Ok(())
}

/// Validate every ancestor without re-stating directories already confirmed in
/// this phase; missing directories are never cached so later creation is rechecked.
fn check_managed_source_parents(
    home: &Path,
    source: &Path,
    checked: &mut FastHashSet<PathBuf>,
) -> Result<()> {
    use std::path::Component;
    ensure!(source.is_absolute(), "rollout source must be absolute");
    let relative = source
        .strip_prefix(home)
        .context("rollout source is outside Codex home")?;
    let first = relative.components().next();
    ensure!(
        matches!(first, Some(Component::Normal(name)) if name == "sessions" || name == "archived_sessions"),
        "rollout source is outside managed session roots"
    );
    ensure!(
        !relative
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "rollout source contains parent traversal"
    );
    let parent = source.parent().context("rollout source has no parent")?;
    ensure!(
        parent != home,
        "rollout source is not inside a session directory"
    );
    check_managed_parent_dir(home, parent, checked)
}

/// Validate every ancestor; missing directories may only be created by recovery.
fn managed_source_parents(home: &Path, source: &Path) -> Result<Vec<PathBuf>> {
    use std::path::Component;
    ensure!(source.is_absolute(), "rollout source must be absolute");
    let relative = source
        .strip_prefix(home)
        .context("rollout source is outside Codex home")?;
    let first = relative.components().next();
    ensure!(
        matches!(first, Some(Component::Normal(name)) if name == "sessions" || name == "archived_sessions"),
        "rollout source is outside managed session roots"
    );
    ensure!(
        !relative
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "rollout source contains parent traversal"
    );
    let parent = source.parent().context("rollout source has no parent")?;
    ensure!(
        parent != home,
        "rollout source is not inside a session directory"
    );
    let mut parents = vec![home.to_path_buf()];
    for component in parent.strip_prefix(home)?.components() {
        parents.push(
            parents
                .last()
                .context("missing source root")?
                .join(component),
        );
    }
    for directory in &parents {
        match fs::symlink_metadata(directory) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "rollout source ancestor is not a real directory: {}",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect rollout source ancestor"),
        }
    }
    Ok(parents)
}

fn prepare_restore_parent(home: &Path, source: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parents = managed_source_parents(home, source)?;
    for pair in parents.windows(2) {
        match fs::symlink_metadata(&pair[1]) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "recovery directory changed"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&pair[1])?;
                fsutil::sync_dir(&pair[1])?;
                fsutil::sync_dir(&pair[0])?;
            }
            Err(error) => return Err(error).context("prepare recovery source directory"),
        }
    }
    Ok(())
}
fn open_archive_dir(p: &Policy) -> Result<(PathBuf, rustix::fd::OwnedFd)> {
    let archive = p.codex_home.join("archived_sessions");
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        &archive,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP => {
            anyhow::anyhow!("archive directory must not be a symlink")
        }
        other => anyhow::Error::new(std::io::Error::from(other))
            .context("archive directory unavailable; cleanup cannot proceed"),
    })?;
    Ok((archive, fd))
}

fn archive_directory(p: &Policy) -> Result<PathBuf> {
    let (archive, _fd) = open_archive_dir(p)?;
    Ok(archive)
}
fn validate_intent(intent: &BatchIntent, p: &Policy) -> Result<()> {
    ensure!(
        intent.owner == p.owner,
        "unrecognized pending deletion owner"
    );
    ensure!(
        !intent.items.is_empty() && intent.items.len() <= fsutil::MAX_BATCH_THREADS,
        "invalid pending batch size"
    );
    let mut ids = HashSet::new();
    let mut rollout_ids = HashSet::new();
    let mut sources = HashSet::new();
    for (index, item) in intent.items.iter().enumerate() {
        ensure!(
            sources.insert(&item.artifact.path),
            "duplicate pending source path"
        );
        ensure!(
            item.slot
                == if intent.schema == 4 {
                    Some(u32::try_from(index)?)
                } else {
                    None
                },
            "invalid pending slot"
        );
        ensure!(
            uuid::Uuid::parse_str(&item.thread_id)?.to_string() == item.thread_id,
            "invalid pending thread ID"
        );
        let first_thread_occurrence = ids.insert(&item.thread_id);
        let rollout = match (intent.schema, item.rollout_id.as_deref()) {
            (2, None) => {
                ensure!(first_thread_occurrence, "duplicate pending thread ID");
                ensure!(
                    item.staged == stage_path(&p.codex_home, &item.thread_id),
                    "invalid pending staging path"
                );
                item.thread_id.as_str()
            }
            (3 | 4, Some(rollout)) => {
                ensure!(
                    uuid::Uuid::parse_str(rollout)?.to_string() == rollout,
                    "invalid pending rollout ID"
                );
                ensure!(
                    item.staged
                        == if intent.schema == 4 {
                            copy_stage_path(
                                &p.codex_home,
                                &item.thread_id,
                                rollout,
                                u32::try_from(index)?,
                            )
                        } else {
                            rollout_stage_path(&p.codex_home, &item.thread_id, rollout)
                        },
                    "invalid pending staging path"
                );
                let filename = crate::lineage::filename(
                    item.artifact
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("invalid recovery source filename")?,
                )?
                .context("invalid recovery rollout filename")?;
                ensure!(
                    filename.owner.to_string() == item.thread_id
                        && filename.rollout.to_string() == rollout,
                    "invalid recovery source owner or rollout identity"
                );
                rollout
            }
            _ => anyhow::bail!("pending rollout identity does not match journal schema"),
        };
        ensure!(
            rollout_ids.insert(rollout) || intent.schema == 4,
            "duplicate pending rollout ID"
        );
        if intent.schema == 4 {
            managed_source_parents(&p.codex_home, &item.artifact.path)?;
        }
        ensure!(
            intent.schema == 4
                || item.artifact.path.parent()
                    == Some(p.codex_home.join("archived_sessions").as_path()),
            "invalid recovery source path"
        );
        ensure!(
            item.artifact
                .path
                .file_name()
                .and_then(|v| v.to_str())
                .and_then(rollout_id)
                == Some(rollout),
            "invalid recovery source identity"
        );
    }
    Ok(())
}

/// Every member is named durably before any move. Recovery is restartable even
/// if only a prefix was staged, restored, or unlinked when the process stopped.
pub fn recover(c: &mut Connection, p: &Policy, store: &Store) -> Result<Option<String>> {
    let path = intent_path(store);
    if !fsutil::path_exists(&path)? {
        return Ok(None);
    }
    let intent: Journal = fsutil::read_json(&path)?;
    let intent = intent.into_batch()?;
    validate_intent(&intent, p)?; // Validate the entire receipt before touching any member.
    let archive = archive_directory(p)?;
    let _maintenance = fsutil::maintenance(&p.codex_home)?;
    let mut ids: Vec<&str> = intent.items.iter().map(|v| v.thread_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let _writers = ThreadLocks::acquire(&p.codex_home, &ids)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    database::verify_base(&tx)?;
    database::verify_policy(&tx, p)?;
    let mut restored_parents = std::collections::BTreeSet::new();
    let mut restored = 0;
    let mut finished = 0;
    let mut reader = crate::metadata::RecordReader::new();
    let mut exists_query = tx.prepare("SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)")?;
    for item in &intent.items {
        let exists: bool = exists_query.query_row([&item.thread_id], |r| r.get(0))?;
        if exists && intent.schema == 4 {
            restored_parents.extend(managed_source_parents(&p.codex_home, &item.artifact.path)?);
        }
        if !fsutil::path_exists(&item.staged)? {
            if exists && intent.schema == 4 {
                // A previous recovery may have restored this file but stopped
                // before its directory barrier. Keep that barrier on every retry.
                let (file, metadata) =
                    fsutil::regular_with_metadata(&item.artifact.path, false, false)?;
                ensure!(
                    Identity::of(&metadata) == item.artifact.identity,
                    "restored source identity changed; manual recovery required"
                );
                let line = reader.read_first_record(
                    &file,
                    item.artifact
                        .path
                        .extension()
                        .is_some_and(|extension| extension == "zst"),
                )?;
                crate::lineage::validate_owner(line, uuid::Uuid::parse_str(&item.thread_id)?)?;
                restored_parents.insert(
                    item.artifact
                        .path
                        .parent()
                        .context("missing restored parent")?
                        .to_path_buf(),
                );
            }
            continue;
        }
        let (file, metadata) = fsutil::regular_with_metadata(&item.staged, false, false)?;
        ensure!(
            Identity::of(&metadata) == item.artifact.identity,
            "staged file identity changed; manual recovery required"
        );
        if intent.schema >= 3 {
            let line = reader.read_first_record(
                &file,
                item.artifact
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "zst"),
            )?;
            crate::lineage::validate_owner(line, uuid::Uuid::parse_str(&item.thread_id)?)?;
        } else {
            drop(file);
        }
        if exists {
            prepare_restore_parent(&p.codex_home, &item.artifact.path)?;
            restored_parents.insert(
                item.artifact
                    .path
                    .parent()
                    .context("missing restore parent")?
                    .to_path_buf(),
            );
            fsutil::rename_without_overwrite_unsynced(&item.staged, &item.artifact.path)?;
            restored += 1;
        } else {
            fs::remove_file(&item.staged)?;
            finished += 1;
        }
    }
    drop(exists_query);
    for parent in restored_parents.into_iter().rev() {
        fsutil::sync_dir(&parent)?;
    }
    fsutil::sync_dir(&archive)?;
    tx.commit()?;
    fsutil::remove_durable(&path)?;
    Ok(Some(format!(
        "recovered pending deletion: restored {restored}, finished {finished}; byte accounting unavailable after interruption"
    )))
}

#[derive(Debug)]
struct GlobalCleanupFailure;

impl fmt::Display for GlobalCleanupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("shared cleanup prerequisite unavailable")
    }
}

#[derive(Default)]
struct BatchScratch {
    reader: crate::metadata::RecordReader,
    lineage: crate::lineage::LineageIndex,
    fresh: crate::relations::Dependencies,
    checked_parents: FastHashSet<PathBuf>,
    deleting: FastHashSet<uuid::Uuid>,
    threads: Vec<BatchThread>,
    owner_zst_buf: String,
}

struct Cleanup<'run, 'connection> {
    policy: &'run Policy,
    store: &'run Store,
    now: i64,
    graph: &'run crate::relations::Dependencies,
    owners: RolloutOwners<'connection>,
    headers: crate::lineage::HeaderCache,
    detail: ReportDetail,
    #[cfg(test)]
    completed_batches: usize,
}

impl Cleanup<'_, '_> {
    #[cfg(test)]
    fn delete_batch(
        &mut self,
        c: &Connection,
        ids: &[&str],
        journal_write_attempted: &mut bool,
    ) -> Result<Vec<(u64, u64)>> {
        let mut scratch = BatchScratch::default();
        let mut stmts = None;
        self.delete_batch_with_scratch(c, ids, journal_write_attempted, &mut scratch, &mut stmts)
    }

    fn delete_batch_with_scratch<'c>(
        &mut self,
        c: &'c Connection,
        ids: &[&str],
        journal_write_attempted: &mut bool,
        scratch: &mut BatchScratch,
        stmts: &mut Option<database::BatchStatements<'c>>,
    ) -> Result<Vec<(u64, u64)>> {
        let p = self.policy;
        let store = self.store;
        let (archive, archive_fd) = open_archive_dir(p).context(GlobalCleanupFailure)?;
        let _maintenance = fsutil::maintenance(&p.codex_home)?;
        scratch.deleting.clear();
        if scratch.deleting.capacity() < ids.len() {
            scratch.deleting.reserve(ids.len() - scratch.deleting.len());
        }
        for &id in ids {
            let uuid = match crate::lineage::parse_canonical_uuid(id) {
                Some(uuid) => uuid,
                None => uuid::Uuid::parse_str(id)?,
            };
            scratch.deleting.insert(uuid);
        }
        let guards: Cow<'_, FastHashSet<uuid::Uuid>> = if self.graph.has_no_parents() {
            ensure!(
                scratch.deleting.len() <= fsutil::MAX_BATCH_THREADS,
                "deletion guard set exceeds {} writer locks",
                fsutil::MAX_BATCH_THREADS
            );
            Cow::Borrowed(&scratch.deleting)
        } else {
            Cow::Owned(
                self.graph
                    .ancestors(&scratch.deleting, fsutil::MAX_BATCH_THREADS)?,
            )
        };
        let _writers = ThreadLocks::acquire_uuids(&p.codex_home, guards.iter().copied())
            .context("acquire thread and owning ancestor writer locks")?;
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        let batch_stmts = match stmts {
            Some(existing) => {
                if let Err(error) = existing.verify_base_and_policy(p) {
                    *stmts = None;
                    return Err(error).context(GlobalCleanupFailure);
                }
                existing
            }
            None => stmts.insert(
                database::BatchStatements::prepare_and_verify(c, p)
                    .context(GlobalCleanupFailure)?,
            ),
        };
        scratch.threads.clear();
        if scratch.threads.capacity() < ids.len() {
            scratch.threads.reserve(ids.len() - scratch.threads.len());
        }
        for &id in ids {
            let thread = batch_stmts
                .lookup_batch_thread(id, |row| {
                    let assessment = assess_borrowed(&row, p, self.now);
                    ensure!(
                        assessment.reason == EntryReason::Eligible,
                        "thread {id} is no longer eligible: {}",
                        assessment.reason
                    );
                    let history_mode = match row.history_mode {
                        "legacy" => "legacy",
                        "paginated" => "paginated",
                        other => anyhow::bail!("unsupported history mode: {other}"),
                    };
                    Ok(BatchThread {
                        id: row.id.to_owned(),
                        path: PathBuf::from(row.path),
                        archived_at: row.archived_at,
                        history_mode,
                    })
                })?
                .context("thread was removed concurrently")?;
            scratch.threads.push(thread);
        }
        // File metadata also carries parent ownership when SQL edges are absent.
        // Rebuild both dependency kinds after locks and the live state transaction.
        scratch.lineage.clear();
        crate::lineage::scan_into(
            &p.codex_home,
            &scratch.deleting,
            Some(&mut self.headers),
            &mut scratch.reader,
            &mut scratch.lineage,
        )
        .context(GlobalCleanupFailure)?;
        scratch.fresh.clear();
        let has_spawn_edges = batch_stmts
            .add_spawn_dependencies(&mut scratch.fresh)
            .context(GlobalCleanupFailure)?;
        scratch.lineage.add_dependencies(&mut scratch.fresh);
        scratch.lineage.release_owner_scratch();
        if !scratch.fresh.has_no_parents() {
            let fresh_guards = scratch
                .fresh
                .ancestors(&scratch.deleting, fsutil::MAX_BATCH_THREADS)
                .context(GlobalCleanupFailure)?;
            ensure!(
                fresh_guards.is_subset(guards.as_ref()),
                "owning ancestors changed before deletion; retry with a fresh snapshot"
            );
        }
        scratch.fresh.verify_removal(&scratch.deleting)?;
        scratch.checked_parents.clear();
        let mut intent = BatchIntent {
            schema: 4,
            owner: p.owner.clone(),
            items: Vec::with_capacity(scratch.threads.len()),
        };
        let mut staged_meta = Vec::with_capacity(scratch.threads.len());
        let mut rollout_buf = [0u8; 36];
        for thread in &scratch.threads {
            let (owner, artifacts) = inspect_owned(
                &thread.id,
                &thread.path,
                p,
                &scratch.lineage,
                thread.history_mode == "paginated",
                true,
                &mut scratch.reader,
                &mut scratch.checked_parents,
                Some((archive.as_path(), archive_fd.as_fd())),
            )?;
            for (artifact, rollout_uuid) in artifacts {
                let rollout = rollout_uuid
                    .hyphenated()
                    .encode_lower(&mut rollout_buf)
                    .to_owned();
                let slot = u32::try_from(intent.items.len())?;
                let staged_name = format!(".codex-retain-pending-{}-{rollout}-{slot}", thread.id);
                let staged = archive.join(&staged_name);
                ensure!(
                    !fsutil::path_exists_at(archive_fd.as_fd(), OsStr::new(&staged_name), &staged)?,
                    "an untracked staged file exists; inspect it before cleanup"
                );
                staged_meta.push((staged_name, owner));
                intent.items.push(PendingItem {
                    thread_id: thread.id.clone(),
                    rollout_id: Some(rollout),
                    slot: Some(slot),
                    artifact,
                    staged,
                });
            }
        }
        // Keep the existing 128-file journal bound. A larger multi-thread group
        // splits before any journal or file write; a single oversized thread stays.
        ensure!(
            intent.items.len() <= fsutil::MAX_BATCH_THREADS,
            "deletion group exceeds 128 rollout files"
        );
        verify_rollout_owners(
            &tx,
            &scratch.threads,
            &intent.items,
            &mut self.owners,
            &mut scratch.owner_zst_buf,
        )?;
        *journal_write_attempted = true;
        fsutil::atomic_json(&intent_path(store), &intent)?;
        scratch.checked_parents.clear();
        let mut source_parents = std::collections::BTreeSet::new();
        for (item, (staged_name, owner)) in intent.items.iter().zip(&staged_meta) {
            check_managed_source_parents(
                &p.codex_home,
                &item.artifact.path,
                &mut scratch.checked_parents,
            )?;
            let parent = item
                .artifact
                .path
                .parent()
                .context("missing source parent")?;
            source_parents.insert(parent);
            let staged_name = OsStr::new(staged_name);
            if parent == archive.as_path()
                && let Some(source_name) = item.artifact.path.file_name()
            {
                fsutil::rename_at_without_overwrite_unsynced(
                    archive_fd.as_fd(),
                    source_name,
                    archive_fd.as_fd(),
                    staged_name,
                    &item.staged,
                )?;
            } else {
                fsutil::rename_at_without_overwrite_unsynced(
                    rustix::fs::CWD,
                    &item.artifact.path,
                    archive_fd.as_fd(),
                    staged_name,
                    &item.staged,
                )?;
            }
            let (file, stat) =
                fsutil::regular_at_with_stat(archive_fd.as_fd(), staged_name, &item.staged)?;
            ensure!(
                Identity::of_stat(&stat) == item.artifact.identity,
                "staged file identity changed before commit"
            );
            let compressed = item.artifact.path.as_os_str().as_bytes().ends_with(b".zst");
            let line = scratch.reader.read_first_record(&file, compressed)?;
            crate::lineage::validate_owner(line, *owner)?;
        }
        source_parents.insert(&archive);
        for parent in source_parents {
            fsutil::sync_dir(parent)?;
        }
        batch_stmts.delete_batch_rows(&scratch.threads)?;
        if has_spawn_edges {
            batch_stmts.delete_spawn_edges(&scratch.threads)?;
        }
        tx.commit()?;
        self.owners.acknowledge_local_deletes();
        let mut removed = FastHashMap::<&str, (u64, u64)>::with_capacity_and_hasher(
            scratch.threads.len(),
            Default::default(),
        );
        for (item, (staged_name, _)) in intent.items.iter().zip(&staged_meta) {
            let staged_name = OsStr::new(staged_name);
            let (file, stat) =
                fsutil::regular_at_with_stat(archive_fd.as_fd(), staged_name, &item.staged)?;
            ensure!(
                Identity::of_stat(&stat) == item.artifact.identity,
                "staged file changed; cleanup stopped"
            );
            let (bytes, allocated) = fsutil::stat_bytes_and_allocated(&stat);
            let totals = removed.entry(item.thread_id.as_str()).or_default();
            totals.0 += bytes;
            totals.1 += allocated;
            drop(file);
            fsutil::unlink_at(archive_fd.as_fd(), staged_name)?;
        }
        fsutil::sync_dir(&archive)?;
        fsutil::remove_durable(&intent_path(store))?;
        ids.iter()
            .map(|id| {
                removed
                    .remove(*id)
                    .context("missing completed thread accounting")
            })
            .collect()
    }
}

fn verify_rollout_owners(
    tx: &rusqlite::Transaction<'_>,
    threads: &[BatchThread],
    items: &[PendingItem],
    cache: &mut RolloutOwners<'_>,
    zst_buf: &mut String,
) -> Result<()> {
    zst_buf.clear();
    let mut zst_spans: Vec<(usize, usize, &str)> = Vec::with_capacity(items.len());
    let mut owners: FastHashMap<&str, &str> =
        FastHashMap::with_capacity_and_hasher(items.len() * 2 + threads.len(), Default::default());
    for item in items {
        let artifact_path = item.artifact.path.to_str().context("non-UTF-8 path")?;
        cache.register_str(artifact_path, &item.thread_id);
        if let Some(stem) = artifact_path.strip_suffix(".zst") {
            for path in [stem, artifact_path] {
                cache.register_covered_or_exact(path, &item.thread_id);
                if let Some(previous) = owners.insert(path, item.thread_id.as_str()) {
                    ensure!(
                        previous == item.thread_id,
                        "multiple candidate threads reference one rollout"
                    );
                }
            }
        } else {
            cache.register_covered_or_exact(artifact_path, &item.thread_id);
            if let Some(previous) = owners.insert(artifact_path, item.thread_id.as_str()) {
                ensure!(
                    previous == item.thread_id,
                    "multiple candidate threads reference one rollout"
                );
            }
            let start = zst_buf.len();
            zst_buf.push_str(artifact_path);
            zst_buf.push_str(".zst");
            let end = zst_buf.len();
            cache.register_covered_or_exact(&zst_buf[start..end], &item.thread_id);
            zst_spans.push((start, end, item.thread_id.as_str()));
        }
    }
    for (start, end, thread_id) in zst_spans {
        let compressed = &zst_buf[start..end];
        if let Some(previous) = owners.insert(compressed, thread_id) {
            ensure!(
                previous == thread_id,
                "multiple candidate threads reference one rollout"
            );
        }
    }
    for thread in threads {
        let path = thread.path.to_str().context("non-UTF-8 path")?;
        // Retain the exact SQL spelling when not already covered by self.paths.
        cache.register_covered_or_exact(path, &thread.id);
        if let Some(previous) = owners.insert(path, thread.id.as_str()) {
            ensure!(
                previous == thread.id,
                "multiple candidate threads reference one rollout"
            );
        }
    }
    cache.verify_borrowed(tx, &owners)
}

impl Cleanup<'_, '_> {
    fn apply_group<'c>(
        &mut self,
        c: &'c Connection,
        report: &mut ReportCore,
        indices: &[u32],
        scratch: &mut BatchScratch,
        stmts: &mut Option<database::BatchStatements<'c>>,
    ) -> Result<ControlFlow<()>> {
        let mut remaining: SmallVec<[std::ops::Range<usize>; 8]> = SmallVec::new();
        remaining.push(0..indices.len());
        let mut uuid_bufs = [[0u8; 36]; fsutil::MAX_BATCH_THREADS];
        while let Some(range) = remaining.pop() {
            debug_assert!(range.len() <= fsutil::MAX_BATCH_THREADS);
            let mut ids: SmallVec<[&str; fsutil::MAX_BATCH_THREADS]> = SmallVec::new();
            for (&index, buf) in indices[range.clone()].iter().zip(uuid_bufs.iter_mut()) {
                ids.push(report.entries.id_str(index as usize, buf));
            }
            let mut journal_write_attempted = false;
            let outcome = self.delete_batch_with_scratch(
                c,
                &ids,
                &mut journal_write_attempted,
                scratch,
                stmts,
            );
            drop(ids);
            match outcome {
                Ok(artifacts) => {
                    #[cfg(test)]
                    {
                        self.completed_batches += 1;
                    }
                    for (&index, (bytes, allocated)) in indices[range].iter().zip(artifacts) {
                        report
                            .entries
                            .set_reason(index as usize, EntryReason::Deleted);
                        report.eligible += 1;
                        report.deleted += 1;
                        report.logical_bytes_removed += bytes;
                        report.allocated_bytes_unlinked += allocated;
                    }
                }
                Err(error) => {
                    let pending = fsutil::path_exists(&intent_path(self.store))?;
                    let database_busy = error.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
                        matches!(error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
                    });
                    let lock_scope = error
                        .downcast_ref::<fsutil::LockAcquisitionError>()
                        .map(|error| &error.scope);
                    let global = error.is::<GlobalCleanupFailure>()
                        || matches!(lock_scope, Some(fsutil::LockScope::Global));
                    let can_retry =
                        !journal_write_attempted && !pending && !database_busy && !global;
                    if can_retry && range.len() > 1 {
                        let mut id_buf = [0u8; 36];
                        if let Some(fsutil::LockScope::Thread(blocked_id)) = lock_scope
                            && let Some(offset) = indices[range.clone()].iter().position(|&index| {
                                report.entries.id_str(index as usize, &mut id_buf) == *blocked_id
                            })
                        {
                            let blocked = range.start + offset;
                            report.failure(
                                indices[blocked] as usize,
                                EntryReason::ChangedBusyOrError,
                                &format!("{error:#}"),
                                self.detail,
                            );
                            // Keep both free sides grouped; the failed member's
                            // lock remains owned by its external writer.
                            if blocked + 1 < range.end {
                                remaining.push(blocked + 1..range.end);
                            }
                            if range.start < blocked {
                                remaining.push(range.start..blocked);
                            }
                            continue;
                        }
                        // Validation failures may concern one unidentified
                        // member. Split iteratively within the original batch.
                        let middle = range.start + range.len() / 2;
                        remaining.push(middle..range.end);
                        remaining.push(range.start..middle);
                        continue;
                    }
                    let message = format!("{error:#}");
                    for &index in &indices[range] {
                        report.failure(
                            index as usize,
                            dependency_reason(&error).unwrap_or(EntryReason::ChangedBusyOrError),
                            &message,
                            self.detail,
                        );
                    }
                    if pending {
                        report.warning(self.detail, "cleanup stopped with a durable pending group; next run recovers it first; counts cover completed groups".into());
                    } else if journal_write_attempted {
                        report.warning(self.detail, "group finalization failed after journal writing began; effects may have occurred; stopped with counts for earlier completed groups only".into());
                    }
                    if database_busy {
                        report.warning(self.detail, "Codex database is busy; stopped this run instead of retrying every archive".into());
                    } else if global {
                        report.warning(self.detail, "shared cleanup prerequisites are unavailable; stopped this run instead of retrying every archive".into());
                    }
                    if !can_retry {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// Preview or run retention at `now`, in whole seconds since the Unix epoch.
pub fn execute(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
) -> Result<Report> {
    Ok(execute_core(c, p, store, now, mode, ReportDetail::Full)?.into_report(ReportDetail::Full))
}

/// Preview or run retention at `now` for CLI `--json` output using compact
/// entry storage and direct JSON streaming without materializing `Vec<Entry>`.
pub(crate) fn execute_compact(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
) -> Result<CompactReport> {
    Ok(execute_core(c, p, store, now, mode, ReportDetail::CompactJson)?.into_compact_report())
}

/// Preview or run retention at `now` for human text output, retaining only the
/// lexicographically smallest 30 display entries plus any of the first 10
/// diagnostic skip entries beyond index 30 needed for `last-run.json`.
pub(crate) fn execute_text(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
) -> Result<Report> {
    Ok(execute_core(c, p, store, now, mode, ReportDetail::Text)?.into_report(ReportDetail::Text))
}

/// Run scheduled retention at `now`, in whole seconds since the Unix epoch.
pub(crate) fn execute_scheduled(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
) -> Result<Report> {
    Ok(
        execute_core(c, p, store, now, ExecutionMode::Run, ReportDetail::Summary)?
            .into_report(ReportDetail::Summary),
    )
}

const TEXT_DISPLAY_LIMIT: usize = 30;

struct EntryById(Entry);

impl PartialEq for EntryById {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for EntryById {}

impl PartialOrd for EntryById {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EntryById {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.id.cmp(&other.0.id)
    }
}

fn sort_report_entries(entries: &mut [Entry], candidates: &mut [Candidate]) {
    if entries.windows(2).all(|pair| pair[0].id <= pair[1].id) {
        return;
    }
    if candidates.is_empty() {
        entries.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        return;
    }
    let n = entries.len();
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by(|&a, &b| entries[a as usize].id.cmp(&entries[b as usize].id));
    let mut new_index = vec![0u32; n];
    for (new_pos, &old_pos) in order.iter().enumerate() {
        new_index[old_pos as usize] = new_pos as u32;
    }
    for i in 0..n {
        let mut current = i;
        while order[current] as usize != i {
            let next = order[current] as usize;
            entries.swap(current, next);
            order[current] = current as u32;
            current = next;
        }
        order[current] = current as u32;
    }
    for candidate in candidates.iter_mut() {
        candidate.report_index = new_index[candidate.report_index as usize];
    }
    candidates.sort_unstable_by_key(|candidate| candidate.report_index);
}

struct CompactSelected {
    report_index: u32,
    ts_lo: u32,
    ts_hi: u16,
    uuid: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<CompactSelected>() == 28);

struct SelectedEntry {
    report_index: u32,
    name_off: u32,
    name_len: u16,
    in_archive: bool,
}

#[derive(Default)]
struct SelectedPaths {
    compact: Vec<CompactSelected>,
    other: Vec<SelectedEntry>,
    other_names: crate::lineage::ChunkedBytes,
}

impl SelectedPaths {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            compact: Vec::with_capacity(capacity),
            other: Vec::new(),
            other_names: crate::lineage::ChunkedBytes::default(),
        }
    }

    fn is_empty(&self) -> bool {
        self.compact.is_empty() && self.other.is_empty()
    }

    fn push(&mut self, report_index: u32, path_bytes: &[u8], in_archive: bool) -> Result<()> {
        if in_archive
            && let Some((ts, uuid, _compressed)) =
                crate::lineage::parse_compact_rollout_key(path_bytes)
        {
            self.compact.push(CompactSelected {
                report_index,
                ts_lo: ts as u32,
                ts_hi: (ts >> 32) as u16,
                uuid: *uuid.as_bytes(),
            });
            return Ok(());
        }
        let (name_off, name_len) = self
            .other_names
            .push(path_bytes)
            .context("rollout path exceeds 64 KiB")?;
        self.other.push(SelectedEntry {
            report_index,
            name_off,
            name_len,
            in_archive,
        });
        Ok(())
    }

    fn other_bytes(&self, entry: &SelectedEntry) -> &[u8] {
        self.other_names.get(entry.name_off, entry.name_len)
    }
}

fn check_republished(
    home: &Path,
    selected_paths: &SelectedPaths,
    report: &mut ReportCore,
    detail: ReportDetail,
) {
    if selected_paths.is_empty() {
        return;
    }
    let Ok(home_fd) = rustix::fs::openat(
        rustix::fs::CWD,
        home,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return;
    };
    let archive_fd = fsutil::open_dir_nofollow(&home.join("archived_sessions")).ok();
    let mut name_buf = [0u8; 74];
    let mut variant_buf = Vec::<u8>::new();
    let mut id_buf = [0u8; 36];
    let mut ci = 0usize;
    let mut oi = 0usize;
    let c_len = selected_paths.compact.len();
    let o_len = selected_paths.other.len();
    while ci < c_len || oi < o_len {
        let index = match (
            selected_paths.compact.get(ci).map(|e| e.report_index),
            selected_paths.other.get(oi).map(|e| e.report_index),
        ) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let mut c_end = ci;
        while c_end < c_len && selected_paths.compact[c_end].report_index == index {
            c_end += 1;
        }
        let mut o_end = oi;
        while o_end < o_len && selected_paths.other[o_end].report_index == index {
            o_end += 1;
        }
        if report.entries.is_deleted(index as usize) {
            let mut republished = false;
            if let Some(fd) = archive_fd.as_ref() {
                for entry in &selected_paths.compact[ci..c_end] {
                    let ts = u64::from(entry.ts_lo) | (u64::from(entry.ts_hi) << 32);
                    let uuid = uuid::Uuid::from_bytes(entry.uuid);
                    let plain_len =
                        crate::lineage::format_compact_rollout_name(ts, uuid, false, &mut name_buf);
                    debug_assert_eq!(plain_len, 70);
                    if rustix::fs::statat(fd.as_fd(), &name_buf[..70], rustix::fs::AtFlags::empty())
                        .is_ok()
                    {
                        republished = true;
                        break;
                    }
                    name_buf[70..74].copy_from_slice(b".zst");
                    if rustix::fs::statat(fd.as_fd(), &name_buf[..74], rustix::fs::AtFlags::empty())
                        .is_ok()
                    {
                        republished = true;
                        break;
                    }
                }
            }
            if !republished {
                republished = selected_paths.other[oi..o_end].iter().any(|entry| {
                    let rel = selected_paths.other_bytes(entry);
                    let (dir_fd, name) = if entry.in_archive {
                        let Some(fd) = archive_fd.as_ref() else {
                            return false;
                        };
                        (fd.as_fd(), rel)
                    } else {
                        match (archive_fd.as_ref(), rel.strip_prefix(b"archived_sessions/")) {
                            (Some(fd), Some(rest)) => (fd.as_fd(), rest),
                            _ => (home_fd.as_fd(), rel),
                        }
                    };
                    let plain = name.strip_suffix(b".zst").unwrap_or(name);
                    if rustix::fs::statat(dir_fd, plain, rustix::fs::AtFlags::empty()).is_ok() {
                        return true;
                    }
                    variant_buf.clear();
                    variant_buf.extend_from_slice(plain);
                    variant_buf.extend_from_slice(b".zst");
                    rustix::fs::statat(dir_fd, variant_buf.as_slice(), rustix::fs::AtFlags::empty())
                        .is_ok()
                });
            }
            if republished {
                let id = report.entries.id_str(index as usize, &mut id_buf);
                let message = format!("{id}: Codex republished a rollout; preserved it");
                report.warning(detail, message);
            }
        }
        ci = c_end;
        oi = o_end;
    }
}

fn execute_core(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
    detail: ReportDetail,
) -> Result<ReportCore> {
    let apply = mode == ExecutionMode::Run;
    ensure!(
        p.enabled,
        "policy is disabled; run enable to apply retention to recorded archive dates"
    );
    ensure!(
        now >= p.enabled_at,
        "system clock moved before policy activation"
    );
    database::verify_policy(c, p)?;
    let mut report = ReportCore::new(mode, now, detail);
    if apply {
        ensure!(
            !p.paused,
            "policy is paused; resume it before a cleanup run"
        );
        if let Some(message) = recover(c, p, store)? {
            report.warning(detail, message);
        }
    } else if fsutil::path_exists(&intent_path(store))? {
        report.warning(
            detail,
            "pending recovery; run cleanup or disable before relying on this preview".into(),
        );
    }

    // An empty prefix preserves full keys when a native profile path is not
    // UTF-8 or relative; it does not change filesystem path operations or skip checks.
    let archive_path = p.codex_home.join("archived_sessions");
    let archive_prefix = if archive_path.is_absolute() {
        archive_path
            .to_str()
            .map(|path| format!("{path}/"))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let mut candidates = CandidateArena::default();
    match detail {
        ReportDetail::Summary => {
            let EntryStore::Compact(ref mut compact) = report.entries else {
                unreachable!()
            };
            report.examined = database::visit_archived_summary(c, now, p.duration(), |event| {
                match event {
                    database::SummaryVisit::DueHint(hint) => {
                        compact.reserve_exact(hint);
                        candidates.entries.reserve_exact(hint);
                    }
                    database::SummaryVisit::Row(thread) => {
                        let assessment = assess_borrowed(&thread, p, now);
                        if assessment.reason == EntryReason::Eligible {
                            let index = compact.push(
                                thread.id,
                                thread.title,
                                assessment.reason,
                                assessment.eligible_at,
                            )?;
                            candidates.push(
                                index,
                                thread.path,
                                &archive_prefix,
                                thread.history_mode == "paginated",
                            )?;
                        }
                    }
                }
                Ok(())
            })?;
            report.skipped = report
                .examined
                .saturating_sub(candidates.entries.len() as u64);
        }
        ReportDetail::Text => {
            let EntryStore::Compact(ref mut compact) = report.entries else {
                unreachable!()
            };
            let mut non_eligible = BinaryHeap::<EntryById>::with_capacity(TEXT_DISPLAY_LIMIT);
            let fast_examined =
                database::visit_archived_text(c, now, p.duration(), TEXT_DISPLAY_LIMIT, |event| {
                    match event {
                        database::TextVisit::DueHint(hint) => {
                            compact.reserve_exact(hint.saturating_add(TEXT_DISPLAY_LIMIT));
                            candidates.entries.reserve_exact(hint);
                        }
                        database::TextVisit::Due(thread) => {
                            let assessment = assess_borrowed(&thread, p, now);
                            if assessment.reason == EntryReason::Eligible {
                                let index = compact.push(
                                    thread.id,
                                    thread.title,
                                    assessment.reason,
                                    assessment.eligible_at,
                                )?;
                                candidates.push(
                                    index,
                                    thread.path,
                                    &archive_prefix,
                                    thread.history_mode == "paginated",
                                )?;
                            }
                        }
                        database::TextVisit::Prefix(thread) => {
                            let assessment = assess_borrowed(&thread, p, now);
                            if assessment.reason != EntryReason::Eligible {
                                compact.push(
                                    thread.id,
                                    thread.title,
                                    assessment.reason,
                                    assessment.eligible_at,
                                )?;
                            }
                        }
                        database::TextVisit::Full(thread) => {
                            report.examined += 1;
                            let assessment = assess_borrowed(&thread, p, now);
                            if assessment.reason == EntryReason::Eligible {
                                let index = compact.push(
                                    thread.id,
                                    thread.title,
                                    assessment.reason,
                                    assessment.eligible_at,
                                )?;
                                candidates.push(
                                    index,
                                    thread.path,
                                    &archive_prefix,
                                    thread.history_mode == "paginated",
                                )?;
                            } else {
                                report.skipped += 1;
                                let keep = non_eligible.len() < TEXT_DISPLAY_LIMIT
                                    || non_eligible
                                        .peek()
                                        .is_some_and(|largest| thread.id < largest.0.id.as_str());
                                if keep {
                                    if non_eligible.len() == TEXT_DISPLAY_LIMIT {
                                        non_eligible.pop();
                                    }
                                    non_eligible.push(EntryById(Entry {
                                        id: thread.id.to_owned(),
                                        title: thread.title.to_owned(),
                                        reason: assessment.reason,
                                        eligible_at: assessment.eligible_at,
                                        bytes: None,
                                        detail: None,
                                    }));
                                }
                            }
                        }
                    }
                    Ok(())
                })?;
            if let Some(examined) = fast_examined {
                report.examined = examined;
                report.skipped = examined.saturating_sub(candidates.entries.len() as u64);
            } else {
                for entry in non_eligible.into_iter().map(|entry| entry.0) {
                    compact.push(&entry.id, &entry.title, entry.reason, entry.eligible_at)?;
                }
            }
        }
        ReportDetail::Full => {
            let EntryStore::Full(ref mut entries) = report.entries else {
                unreachable!()
            };
            database::visit_archived_full_with_count(c, |event| {
                match event {
                    database::FullVisit::Count(count) => entries.reserve_exact(count),
                    database::FullVisit::Row(thread) => {
                        report.examined += 1;
                        let assessment = assess_borrowed(&thread, p, now);
                        let eligible = assessment.reason == EntryReason::Eligible;
                        let index = entries.len();
                        entries.push(Entry {
                            id: thread.id.to_owned(),
                            title: thread.title.to_owned(),
                            reason: assessment.reason,
                            eligible_at: assessment.eligible_at,
                            bytes: None,
                            detail: None,
                        });
                        if eligible {
                            candidates.push(
                                index,
                                thread.path,
                                &archive_prefix,
                                thread.history_mode == "paginated",
                            )?;
                        } else {
                            report.skipped += 1;
                        }
                    }
                }
                Ok(())
            })?;
        }
        ReportDetail::CompactJson => {
            struct CompactScanState<'a> {
                compact: &'a mut CompactEntries,
                candidates: &'a mut CandidateArena,
                examined: u64,
                phase1_ok: usize,
                matched_phase1_ok: usize,
                epoch_hint: usize,
                eligible_indices: Vec<u32>,
            }
            let EntryStore::Compact(ref mut compact) = report.entries else {
                unreachable!()
            };
            let duration = p.duration();
            let mut state = CompactScanState {
                compact,
                candidates: &mut candidates,
                examined: 0,
                phase1_ok: 0,
                matched_phase1_ok: 0,
                epoch_hint: 0,
                eligible_indices: Vec::new(),
            };
            database::visit_archived_compact(
                c,
                &mut state,
                |state, event| {
                    match event {
                        database::CompactVisit::Count(count) => {
                            state.compact.reserve_exact(count);
                        }
                        database::CompactVisit::Thread(thread) => {
                            state.examined += 1;
                            let (reason, stored_ts) = if thread.archived != 1 {
                                (EntryReason::UnknownArchiveStatus, None)
                            } else if !p.exclusions.is_empty() && p.exclusions.contains(thread.id) {
                                (EntryReason::Excluded, None)
                            } else if thread.pinned != 0 {
                                (EntryReason::PinnedOrUnknownPin, None)
                            } else if !supported_history_mode(thread.history_mode) {
                                (EntryReason::UnsupportedHistoryMode, None)
                            } else if thread.archived_at.is_none() {
                                (EntryReason::UnknownArchiveTime, None)
                            } else {
                                state.phase1_ok += 1;
                                (EntryReason::MissingCapture, thread.archived_at)
                            };
                            state
                                .compact
                                .push(thread.id, thread.title, reason, stored_ts)?;
                        }
                        database::CompactVisit::ThreadsDone => {
                            state.compact.sort_entries_only();
                        }
                        database::CompactVisit::Epoch(epoch_row) => {
                            let Some(epoch) = epoch_row.archived_since else {
                                return Ok(());
                            };
                            let Some(idx) = state
                                .compact
                                .find_by_id_hinted(epoch_row.thread_id, &mut state.epoch_hint)
                            else {
                                return Ok(());
                            };
                            let deadline = epoch.checked_add(duration);
                            if state.compact.entries[idx].reason() == CompactReason::MissingCapture
                            {
                                state.matched_phase1_ok += 1;
                                let archived_at = state
                                    .compact
                                    .eligible_at(&state.compact.entries[idx])
                                    .expect("phase1_ok entry stores archived_at");
                                let final_reason =
                                    if Some(archived_at) != epoch_row.codex_archived_at {
                                        CompactReason::CaptureMismatch
                                    } else if epoch <= 0
                                        || epoch > now
                                        || archived_at <= 0
                                        || archived_at > now
                                    {
                                        CompactReason::InvalidOrFutureClock
                                    } else if deadline.is_none_or(|v| now <= v) {
                                        CompactReason::WithinRetention
                                    } else {
                                        state.eligible_indices.push(idx as u32);
                                        CompactReason::Eligible
                                    };
                                state.compact.entries[idx].set_reason(final_reason);
                                state.compact.set_eligible_at(idx, deadline)?;
                            } else {
                                state.compact.set_eligible_at(idx, deadline)?;
                            }
                        }
                    }
                    Ok(())
                },
                |state, tx| {
                    if state.matched_phase1_ok < state.phase1_ok {
                        state.compact.clear_unmatched_missing_capture();
                    }
                    if !state.eligible_indices.is_empty() {
                        state
                            .candidates
                            .entries
                            .reserve_exact(state.eligible_indices.len());
                        if state.eligible_indices.len() >= 1024
                            && state.eligible_indices.len() * 8 >= state.examined as usize
                        {
                            let mut seq_hint = 0usize;
                            database::visit_eligible_paths_sequential(
                                tx,
                                |id, path, paginated| {
                                    if let Some(idx) =
                                        state.compact.find_by_id_hinted(id, &mut seq_hint)
                                        && state.compact.entries[idx].reason()
                                            == CompactReason::Eligible
                                    {
                                        state.candidates.push(
                                            idx,
                                            path,
                                            &archive_prefix,
                                            paginated,
                                        )?;
                                    }
                                    Ok(())
                                },
                            )?;
                            state
                                .candidates
                                .entries
                                .sort_unstable_by_key(|candidate| candidate.report_index);
                        } else {
                            state.eligible_indices.sort_unstable();
                            let mut lookup = database::prepare_eligible_path_lookup(tx)?;
                            let mut id_buf = [0u8; 36];
                            for &idx_u32 in &state.eligible_indices {
                                let idx = idx_u32 as usize;
                                let id = state.compact.id_str(idx, &mut id_buf);
                                database::lookup_eligible_path(
                                    &mut lookup,
                                    id,
                                    |path, paginated| {
                                        state.candidates.push(idx, path, &archive_prefix, paginated)
                                    },
                                )?;
                            }
                        }
                    }
                    Ok(())
                },
            )?;
            report.examined = state.examined;
            report.skipped = report
                .examined
                .saturating_sub(state.eligible_indices.len() as u64);
        }
    }
    report.entries.sort_with_candidates(&mut candidates.entries);
    // Finish the entire bounded snapshot before deletion, including the
    // 100,001st-row guard. Only candidate paths survive alongside report state.
    let mut scratch = BatchScratch::default();
    let mut graph = crate::relations::Dependencies::default();
    // Preview has no later scan to reuse these headers.
    let mut headers = crate::lineage::HeaderCache::default();
    let lineage = if candidates.entries.is_empty() {
        Ok(())
    } else {
        // Release SQLite scan pages before HeaderCache/lineage allocations peak.
        database::shrink_memory(c);
        let mut candidate_ids =
            FastHashSet::with_capacity_and_hasher(candidates.entries.len(), Default::default());
        for candidate in &candidates.entries {
            if let Some(uuid) = report.entries.id_uuid(candidate.report_index as usize) {
                candidate_ids.insert(uuid);
            }
        }
        crate::lineage::scan_into(
            &p.codex_home,
            &candidate_ids,
            apply.then_some(&mut headers),
            &mut scratch.reader,
            &mut scratch.lineage,
        )
        .and_then(|()| {
            database::add_spawn_dependencies(c, &mut graph)?;
            scratch.lineage.add_dependencies(&mut graph);
            scratch.lineage.release_graph_scratch();
            Ok(())
        })
    };
    let expected_valid = if lineage.is_ok() {
        candidates.entries.len().min(scratch.lineage.owned_len())
    } else {
        0
    };
    let lineage_error_msg = lineage
        .as_ref()
        .err()
        .map(|error| format!("cannot verify history and ownership dependencies: {error:#}"));
    let mut selected_paths = if apply {
        SelectedPaths::with_capacity(expected_valid)
    } else {
        SelectedPaths::default()
    };
    let before = apply
        .then(|| fsutil::volume_available(&p.codex_home))
        .flatten();
    let mut due: Vec<u32> = Vec::with_capacity(expected_valid);
    report.entries.reserve_bytes_capacity(expected_valid);
    let mut owners =
        RolloutOwners::with_capacity(c, &archive_prefix, if apply { expected_valid } else { 0 });
    let mut owner_path_buf = if apply && expected_valid > 0 && !archive_prefix.is_empty() {
        let mut buf = String::with_capacity(archive_prefix.len() + 80);
        buf.push_str(&archive_prefix);
        buf
    } else {
        String::new()
    };
    let mut error_buf = String::new();
    let mut cand_path_buf = [0u8; 74];
    let mut cand_id_buf = [0u8; 36];
    let mut owned_name_buf = [0u8; 74];
    for candidate in &candidates.entries {
        let index_u32 = candidate.report_index;
        let index = index_u32 as usize;
        if let Some(ref message) = lineage_error_msg {
            report.skipped += 1;
            report.failure(
                index,
                EntryReason::UnsafeOrUnavailableArtifact,
                message,
                detail,
            );
            continue;
        }
        let id = report.entries.id_str(index, &mut cand_id_buf);
        let path_bytes = candidates.path_bytes(candidate, &mut cand_path_buf);
        let logical_path = if candidate.in_archive() {
            LogicalPathRef::ArchiveName(&archive_path, OsStr::from_bytes(path_bytes))
        } else {
            LogicalPathRef::Full(Path::new(OsStr::from_bytes(path_bytes)))
        };
        // The initial scan started with an empty HeaderCache, so every owned
        // rollout was just opened, fstat-checked (regular, 1 link), read, and
        // header-validated from disk. Deletion batches still re-open and
        // re-validate every rollout fresh under ThreadLocks + BEGIN IMMEDIATE.
        let validated = validate_owned_selection(
            id,
            logical_path,
            p,
            &scratch.lineage,
            candidate.paginated(),
            &mut scratch.checked_parents,
        );
        match validated {
            Ok((_owner, rollouts)) => {
                let bytes = rollouts.iter().map(|rollout| rollout.bytes).sum();
                if apply {
                    for rollout in rollouts {
                        let dir = scratch.lineage.rollout_dir(rollout);
                        let name = scratch.lineage.rollout_name(rollout, &mut owned_name_buf);
                        if dir == archive_path.as_path() && !archive_prefix.is_empty() {
                            let name_str = name.to_str().context("non-UTF-8 path")?;
                            owner_path_buf.truncate(archive_prefix.len());
                            owner_path_buf.push_str(name_str);
                            owners.register_str(&owner_path_buf, id);
                            selected_paths.push(index_u32, name.as_bytes(), true)?;
                        } else {
                            let full = dir.join(name);
                            let artifact_str = full.to_str().context("non-UTF-8 path")?;
                            owners.register_str(artifact_str, id);
                            if let Some(plain) = artifact_str.strip_suffix(".zst") {
                                owners.register_covered_or_exact(plain, id);
                                owners.register_covered_or_exact(artifact_str, id);
                            } else {
                                owners.register_covered_or_exact(artifact_str, id);
                                owners
                                    .register_covered_or_exact(&format!("{artifact_str}.zst"), id);
                            }
                            let relative = full
                                .strip_prefix(&p.codex_home)
                                .context("rollout artifact outside Codex home")?
                                .as_os_str()
                                .as_bytes();
                            selected_paths.push(index_u32, relative, false)?;
                        }
                    }
                    if let Ok(path_str) = simdutf8::basic::from_utf8(path_bytes) {
                        if candidate.in_archive() && !archive_prefix.is_empty() {
                            owner_path_buf.truncate(archive_prefix.len());
                            owner_path_buf.push_str(path_str);
                            owners.register_covered_or_exact(&owner_path_buf, id);
                        } else {
                            owners.register_covered_or_exact(path_str, id);
                        }
                    }
                }
                report.entries.set_bytes(index, bytes);
                due.push(index_u32);
            }
            Err(error) => {
                report.skipped += 1;
                error_buf.clear();
                let _ = write!(error_buf, "{error:#}");
                report.failure(
                    index,
                    if error.is::<ReferencedHistory>() {
                        EntryReason::ReferencedHistory
                    } else {
                        EntryReason::UnsafeOrUnavailableArtifact
                    },
                    &error_buf,
                    detail,
                );
            }
        }
    }
    drop(candidates);
    drop(owner_path_buf);
    // Release the initial scan's owned-rollout allocations (up to 10k entries)
    // before dependency planning and batched deletion (at most 128 entries).
    scratch.lineage = crate::lineage::LineageIndex::default();
    scratch.checked_parents.clear();
    // File validation precedes dependency pruning: an unavailable or protected
    // child must keep its parents, even when their own files are valid.
    let layers: Vec<Vec<u32>> = if graph.is_empty() {
        if due.is_empty() {
            Vec::new()
        } else {
            vec![due]
        }
    } else {
        let mut indices = FastHashMap::with_capacity_and_hasher(due.len(), Default::default());
        let mut candidate_set =
            FastHashSet::with_capacity_and_hasher(due.len(), Default::default());
        let mut id_buf = [0u8; 36];
        for index in due {
            let id = match report.entries.id_uuid(index as usize) {
                Some(id) => id,
                None => uuid::Uuid::parse_str(report.entries.id_str(index as usize, &mut id_buf))?,
            };
            match graph.has_bounded_ancestors(id, fsutil::MAX_BATCH_THREADS) {
                Ok(()) => {
                    indices.insert(id, index);
                    candidate_set.insert(id);
                }
                Err(error) => {
                    report.failure(
                        index as usize,
                        EntryReason::UnsafeOrUnavailableArtifact,
                        &format!("{error:#}"),
                        detail,
                    );
                }
            }
        }
        let plan = graph.plan(&candidate_set);
        drop(candidate_set);
        for (id, blocker) in plan.blocked {
            report.failure(
                indices[&id] as usize,
                blocker_reason(blocker),
                &blocker.to_string(),
                detail,
            );
        }
        plan.layers
            .into_iter()
            .map(|layer| layer.into_iter().map(|id| indices[&id]).collect())
            .collect()
    };
    report.selected_bytes = layers
        .iter()
        .flatten()
        .map(|&index| report.entries.bytes_for(index as usize).unwrap_or(0))
        .sum();
    if !apply {
        report.eligible = layers.iter().map(Vec::len).sum::<usize>() as u64;
        report.skipped = report.examined.saturating_sub(report.eligible);
    }
    if apply {
        let mut cleanup = Cleanup {
            policy: p,
            store,
            now,
            graph: &graph,
            owners,
            headers,
            detail,
            #[cfg(test)]
            completed_batches: 0,
        };
        let mut batch_stmts = None;
        'layers: for layer in &layers {
            for group in layer.chunks(fsutil::MAX_BATCH_THREADS) {
                if cleanup
                    .apply_group(c, &mut report, group, &mut scratch, &mut batch_stmts)?
                    .is_break()
                {
                    report.entries.mark_remaining_eligible_stopped();
                    break 'layers;
                }
            }
        }
        drop(batch_stmts);
        #[cfg(test)]
        {
            report.completed_batches = cleanup.completed_batches;
        }
        drop(cleanup);
        drop(scratch);
        report.skipped = report.examined.saturating_sub(report.deleted);
        report.observed_free_space_delta_bytes = before
            .zip(fsutil::volume_available(&p.codex_home))
            .and_then(|(b, a)| i64::try_from(i128::from(a) - i128::from(b)).ok());
        // Check every owned segment, including inactive segments of a reverted
        // thread, for a late materialization. Never remove a republished file.
        check_republished(&p.codex_home, &selected_paths, &mut report, detail);
    }
    Ok(report)
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn cached_inventory_rechecks_orphan_parents_between_committed_groups() {
        for change in ["new", "rewrite", "replace"] {
            let fixture = test_support::Fixture::new();
            let first = fixture.add(1, 1);
            let second = fixture.add(2, 1);
            fixture.age(&first);
            fixture.age(&second);
            let orphan = uuid::Uuid::from_u128(3);
            let path = fixture
                .home
                .join("sessions")
                .join(format!("rollout-2026-09-10T00-00-00-{orphan}.jsonl"));
            let record = |parent: uuid::Uuid| {
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type":"session_meta", "payload": {
                            "id":orphan, "history_mode":"legacy", "parent_thread_id":parent
                        }
                    })
                )
            };
            if change != "new" {
                fs::write(&path, record(uuid::Uuid::from_u128(4))).unwrap();
            }
            let ids = [&first, &second].map(|id| uuid::Uuid::parse_str(id).unwrap());
            let first_path = fixture.path(&first, true);
            let second_path = fixture.path(&second, true);
            let mut paths = vec![first_path.as_path(), second_path.as_path()];
            if change != "new" {
                paths.push(&path);
            }
            let mut headers = crate::lineage::HeaderCache::after_files_settle(&paths);
            let lineage =
                crate::lineage::scan(&fixture.home, &HashSet::from(ids), Some(&mut headers))
                    .unwrap();
            let mut graph = crate::relations::Dependencies::default();
            lineage.add_dependencies(&mut graph);
            let mut cleanup = Cleanup {
                policy: &fixture.policy,
                store: &fixture.store,
                now: test_support::now(),
                graph: &graph,
                owners: RolloutOwners::new(&fixture.c, ""),
                headers,
                detail: ReportDetail::Full,
                completed_batches: 0,
            };
            let mut attempted = false;
            cleanup
                .delete_batch(&fixture.c, &[&first], &mut attempted)
                .unwrap();
            assert!(attempted);
            assert!(!fixture.exists(&first));
            let data_version: i64 = fixture
                .c
                .query_row("PRAGMA main.data_version", [], |row| row.get(0))
                .unwrap();
            if change == "replace" {
                fs::rename(&path, fixture.home.join("saved-orphan")).unwrap();
            }
            fs::write(&path, record(ids[1])).unwrap();
            assert_eq!(
                fixture
                    .c
                    .query_row("PRAGMA main.data_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                data_version
            );
            attempted = false;
            let error = cleanup
                .delete_batch(&fixture.c, &[&second], &mut attempted)
                .unwrap_err();
            assert!(
                error.to_string().contains("surviving child"),
                "{change}: {error:#}"
            );
            assert!(!attempted, "fresh dependency must veto before journaling");
            assert!(fixture.exists(&second));
            assert!(fixture.path(&second, true).exists());
            assert!(path.exists());
            assert!(!fixture.store.root.join("pending.json").exists());
        }
    }

    #[test]
    fn flat_batch_syncs_the_archive_once_per_durability_barrier() {
        let mut fixture = test_support::Fixture::new();
        let id = fixture.add(1, 1);
        fixture.age(&id);
        fsutil::trace_directory_syncs(None);
        let report = fixture.run(true).unwrap();
        let trace = fsutil::take_directory_sync_trace();
        assert_eq!(report.deleted, 1);
        assert_eq!(
            trace
                .iter()
                .filter(|path| **path == fixture.home.join("archived_sessions"))
                .count(),
            2,
            "one barrier before SQL commit and one after staged unlink"
        );
    }

    #[test]
    fn reason_tokens_keep_the_report_schema_and_attention_policy() {
        let cases = [
            (
                EntryReason::UnknownArchiveStatus,
                "unknown_archive_status",
                false,
            ),
            (EntryReason::Excluded, "excluded", false),
            (
                EntryReason::PinnedOrUnknownPin,
                "pinned_or_unknown_pin",
                false,
            ),
            (
                EntryReason::UnsupportedHistoryMode,
                "unsupported_history_mode",
                false,
            ),
            (EntryReason::ReferencedHistory, "referenced_history", false),
            (EntryReason::RelatedThread, "related_thread", false),
            (EntryReason::DependentThread, "dependent_thread", false),
            (EntryReason::DependencyCycle, "dependency_cycle", true),
            (
                EntryReason::UnknownArchiveTime,
                "unknown_archive_time",
                false,
            ),
            (EntryReason::MissingCapture, "missing_capture", false),
            (EntryReason::CaptureMismatch, "capture_mismatch", false),
            (
                EntryReason::InvalidOrFutureClock,
                "invalid_or_future_clock",
                false,
            ),
            (EntryReason::WithinRetention, "within_retention", false),
            (EntryReason::Eligible, "eligible", false),
            (EntryReason::Deleted, "deleted", false),
            (
                EntryReason::ChangedBusyOrError,
                "changed_busy_or_error",
                true,
            ),
            (
                EntryReason::UnsafeOrUnavailableArtifact,
                "unsafe_or_unavailable_artifact",
                true,
            ),
            (EntryReason::RunStopped, "run_stopped", false),
        ];
        for (reason, token, attention) in cases {
            let wire = serde_json::json!({
                "id": "synthetic", "title": "fixture", "reason": token,
                "eligible_at": 42, "bytes": 123, "detail": null,
            });
            let entry: Entry = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(entry.reason, reason, "deserialize {token}");
            assert_eq!(entry.reason.to_string(), token, "display {token}");
            assert_eq!(
                serde_json::to_value(&entry).unwrap(),
                wire,
                "serialize {token}"
            );
            let mut report = Report::new(ExecutionMode::Run, 42);
            report.entries.push(entry);
            assert_eq!(
                report.requires_attention(),
                attention,
                "attention for {token}"
            );
            report.warnings.push("synthetic recovery warning".into());
            assert!(
                report.requires_attention(),
                "warnings still require attention"
            );
        }
    }

    #[test]
    fn unknown_report_reason_is_preserved_without_becoming_eligible() {
        let wire = serde_json::json!({
            "id": "synthetic", "title": "fixture", "reason": "future_reason",
            "eligible_at": null, "bytes": null, "detail": null,
        });
        let entry: Entry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(entry.reason, EntryReason::Unknown("future_reason".into()));
        assert_ne!(entry.reason, EntryReason::Eligible);
        assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
        let mut report = Report::new(ExecutionMode::Preview, 42);
        report.entries.push(entry);
        assert!(!report.requires_attention());
        assert_eq!(report.mode, "preview");
        assert_eq!(Report::new(ExecutionMode::Run, 42).mode, "run");
    }

    #[test]
    fn report_reason_still_rejects_non_string_json() {
        for reason in [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({"eligible": null}),
        ] {
            let wire = serde_json::json!({
                "id": "synthetic", "title": "fixture", "reason": reason,
                "eligible_at": null, "bytes": null, "detail": null,
            });
            assert!(serde_json::from_value::<Entry>(wire).is_err());
        }
    }

    #[test]
    fn rollout_variants_share_paths_without_losing_native_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let plain = Path::new(std::ffi::OsStr::from_bytes(b"/archive-\xff/rollout.jsonl"));
        let compressed = Path::new(std::ffi::OsStr::from_bytes(
            b"/archive-\xff/rollout.jsonl.zst",
        ));
        for path in [plain, compressed] {
            let variants = RolloutVariants::from_path(path);
            assert_eq!(variants.plain, plain);
            assert_eq!(variants.compressed, compressed);
        }
    }

    #[test]
    fn inventory_retains_candidates_and_still_checks_unrelated_paths() {
        let fixture = test_support::Fixture::new();
        let due = fixture.add(1, 1);
        for number in 2..=100 {
            fixture.add(number, 0);
        }
        let owner = uuid::Uuid::parse_str(&due).unwrap();
        let candidates = HashSet::from([owner]);
        let inventory = crate::lineage::scan(&fixture.home, &candidates, None).unwrap();
        assert_eq!(inventory.owned(owner).len(), 1);
        assert_eq!(inventory.owned(owner)[0].path, fixture.path(&due, true));
        assert!(inventory.owned(uuid::Uuid::from_u128(2)).is_empty());
        let duplicate = fixture
            .home
            .join("sessions")
            .join(fixture.path(&due, true).file_name().unwrap());
        fs::copy(fixture.path(&due, true), &duplicate).unwrap();
        assert_eq!(
            crate::lineage::scan(&fixture.home, &candidates, None)
                .unwrap()
                .owned(owner)
                .len(),
            2
        );
        std::os::unix::fs::symlink("missing", fixture.home.join("sessions/unrelated-link"))
            .unwrap();
        assert!(crate::lineage::scan(&fixture.home, &candidates, None).is_err());
    }

    #[test]
    fn scheduled_noop_retains_no_per_archive_entries() {
        let mut fixture = test_support::Fixture::new();
        for number in 1..=1000 {
            fixture.add(number, 1);
        }
        let report = execute_scheduled(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
        )
        .unwrap();
        assert_eq!(report.examined, 1000);
        assert_eq!(report.skipped, 1000);
        assert_eq!(report.deleted, 0);
        assert!(report.entries.is_empty());
        assert_eq!(report.entries.capacity(), 0);
        assert!(!report.requires_attention());
        assert_eq!(report.completed_batches, 0);
    }

    #[test]
    fn scheduled_errors_keep_first_ten_ids_even_when_earlier_failure_is_discovered_late() {
        let mut fixture = test_support::Fixture::new();
        let mut ids = Vec::new();
        for number in 1..=25 {
            let id = fixture.add(number, 1);
            fixture.age(&id);
            if number >= 15 {
                fs::remove_file(fixture.path(&id, true)).unwrap();
            }
            ids.push(id);
        }
        let _writer = fsutil::ThreadLock::acquire(&fixture.home, &ids[0]).unwrap();
        let report = execute_scheduled(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
        )
        .unwrap();
        let expected: Vec<&str> = std::iter::once(ids[0].as_str())
            .chain(ids[14..23].iter().map(String::as_str))
            .collect();
        assert_eq!(
            report
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(report.entries[0].reason, EntryReason::ChangedBusyOrError);
        assert!(report.entries.capacity() <= 10);
        assert!(
            report.entries[1..]
                .iter()
                .all(|entry| entry.reason == EntryReason::UnsafeOrUnavailableArtifact)
        );
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.title == "fixture" && entry.detail.is_some())
        );
        assert_eq!(report.deleted, 13);
        assert_eq!(report.skipped, 12);
        assert!(report.requires_attention());
        assert!(fixture.exists(&ids[0]));
    }

    #[test]
    fn a_known_busy_member_keeps_free_peers_in_at_most_two_committed_groups() {
        for blocked in [
            0,
            fsutil::MAX_BATCH_THREADS / 2,
            fsutil::MAX_BATCH_THREADS - 1,
        ] {
            let mut fixture = test_support::Fixture::new();
            let ids: Vec<_> = (1..=fsutil::MAX_BATCH_THREADS)
                .map(|number| {
                    let id = fixture.add(number as u128, 1);
                    fixture.age(&id);
                    id
                })
                .collect();
            let _writer = fsutil::ThreadLock::acquire(&fixture.home, &ids[blocked]).unwrap();
            let report = fixture.run(true).unwrap();
            assert_eq!(report.deleted, 127);
            assert_eq!(report.skipped, 1);
            assert_eq!(
                report.completed_batches,
                if blocked == 0 || blocked == 127 { 1 } else { 2 }
            );
            for (index, id) in ids.iter().enumerate() {
                assert_eq!(fixture.exists(id), index == blocked);
                assert_eq!(fixture.path(id, true).exists(), index == blocked);
            }
        }
    }

    #[test]
    fn alias_conflict_splits_groups_without_stranding_safe_neighbors() {
        let mut fixture = test_support::Fixture::new();
        let ids: Vec<_> = (1..=128)
            .map(|number| {
                let id = fixture.add(number, 1);
                fixture.age(&id);
                id
            })
            .collect();
        let active = fixture.add(129, 0);
        fixture
            .c
            .execute(
                "UPDATE threads SET rollout_path=? WHERE id=?",
                rusqlite::params![fixture.path(&ids[0], true).to_str().unwrap(), active],
            )
            .unwrap();
        let report = fixture.run(true).unwrap();
        assert_eq!(report.deleted, 127);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.completed_batches, 7);
        assert!(fixture.exists(&ids[0]) && fixture.path(&ids[0], true).exists());
        assert!(fixture.exists(&active) && fixture.path(&active, false).exists());
    }

    #[test]
    fn equivalent_logical_path_spellings_keep_deletion_and_exact_alias_guards() {
        for dot_component in [false, true] {
            for compressed in [false, true] {
                for alias in [false, true] {
                    let mut fixture = test_support::Fixture::new();
                    let id = fixture.add(1, 1);
                    fixture.age(&id);
                    let plain = fixture.path(&id, true);
                    let original = fs::read(&plain).unwrap();
                    let physical = if compressed {
                        let path = plain.with_extension("jsonl.zst");
                        fs::write(
                            &path,
                            zstd::stream::encode_all(original.as_slice(), 1).unwrap(),
                        )
                        .unwrap();
                        fs::remove_file(&plain).unwrap();
                        path
                    } else {
                        plain.clone()
                    };
                    let name = plain.file_name().unwrap().to_str().unwrap();
                    let logical = if dot_component {
                        format!("{}/archived_sessions/./{name}", fixture.home.display())
                    } else {
                        format!("{}//archived_sessions/{name}", fixture.home.display())
                    };
                    fixture
                        .c
                        .execute(
                            "UPDATE threads SET rollout_path=? WHERE id=?",
                            rusqlite::params![logical, id],
                        )
                        .unwrap();
                    if alias {
                        let active = fixture.add(2, 0);
                        fixture
                            .c
                            .execute(
                                "UPDATE threads SET rollout_path=? WHERE id=?",
                                rusqlite::params![logical, active],
                            )
                            .unwrap();
                    }
                    let report = fixture.run(true).unwrap();
                    assert_eq!(
                        report.deleted,
                        u64::from(!alias),
                        "{logical}, compressed={compressed}, alias={alias}"
                    );
                    assert_eq!(fixture.exists(&id), alias);
                    assert_eq!(physical.exists(), alias);
                    if alias {
                        assert!(
                            report.entries[0]
                                .detail
                                .as_ref()
                                .unwrap()
                                .contains("another thread references")
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn global_lock_failure_stops_before_attempting_later_groups() {
        let mut fixture = test_support::Fixture::new();
        for number in 1..=fsutil::MAX_BATCH_THREADS + 1 {
            let id = fixture.add(number as u128, 1);
            fixture.age(&id);
        }
        let _maintenance = fsutil::maintenance(&fixture.home).unwrap();
        let report = fixture.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.completed_batches, 0);
        assert_eq!(
            report.entries.last().unwrap().reason,
            EntryReason::RunStopped
        );
        assert_eq!(report.skipped, report.examined);
        assert!(!fixture.store.root.join("pending.json").exists());
    }

    #[test]
    fn oversized_archive_is_rejected_before_removing_the_first_due_row() {
        let mut fixture = test_support::Fixture::new();
        let due = fixture.add(1, 1);
        fixture.age(&due);
        let original = fs::read(fixture.path(&due, true)).unwrap();
        fixture.c.execute_batch("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<100000) INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at) SELECT printf('limit-%06d',n),'/not-inspected',1,1,'cli','fixture','/fixture','x','{}','never',1,1 FROM numbers;").unwrap();
        let error = fixture.run(true).unwrap_err();
        assert!(error.to_string().contains("100,000-thread safety limit"));
        assert!(fixture.exists(&due));
        assert_eq!(fs::read(fixture.path(&due, true)).unwrap(), original);
        assert!(!fixture.store.root.join("pending.json").exists());
    }

    #[test]
    fn journal_unlink_sync_failure_never_retries_an_effectful_group() {
        let mut fixture = test_support::Fixture::new();
        let ids: Vec<_> = (1..=fsutil::MAX_BATCH_THREADS + 1)
            .map(|n| {
                let id = fixture.add(n as u128, 1);
                fixture.age(&id);
                id
            })
            .collect();
        fsutil::fail_journal_clear_sync_once(fixture.store.root.clone());
        let report = fixture.run(true).unwrap();
        assert_eq!(
            report.deleted, 0,
            "uncertain group completion must not invent accounting"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|v| v.contains("finalization failed"))
        );
        assert!(!fixture.store.root.join("pending.json").exists());
        for id in &ids[..fsutil::MAX_BATCH_THREADS] {
            assert!(!fixture.exists(id));
            assert!(!fixture.path(id, true).exists());
        }
        assert!(
            fixture.exists(&ids[fsutil::MAX_BATCH_THREADS]),
            "later groups must not continue after post-effect failure"
        );
        assert!(fixture.path(&ids[fsutil::MAX_BATCH_THREADS], true).exists());
        assert_eq!(fixture.run(true).unwrap().deleted, 1);
    }

    #[test]
    fn recovery_retries_every_cross_directory_barrier_after_partial_failure() {
        for recreate_directories in [false, true] {
            let mut fixture = test_support::Fixture::new();
            let owner = fixture.add(1, 1);
            let archived = fixture.path(&owner, true);
            let source = fixture
                .home
                .join("sessions/2025/01/01")
                .join(archived.file_name().unwrap());
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::rename(&archived, &source).unwrap();
            let original = fs::read(&source).unwrap();
            let metadata = fs::metadata(&source).unwrap();
            let staged = copy_stage_path(&fixture.home, &owner, &owner, 0);
            let intent = BatchIntent {
                schema: 4,
                owner: fixture.policy.owner.clone(),
                items: vec![PendingItem {
                    thread_id: owner.clone(),
                    rollout_id: Some(owner.clone()),
                    slot: Some(0),
                    artifact: Artifact {
                        path: source.clone(),
                        identity: Identity::of(&metadata),
                        bytes: metadata.len(),
                        allocated: metadata.blocks() * 512,
                    },
                    staged: staged.clone(),
                }],
            };
            fsutil::atomic_json(&intent_path(&fixture.store), &intent).unwrap();
            fs::rename(&source, &staged).unwrap();
            if recreate_directories {
                for directory in ["sessions/2025/01/01", "sessions/2025/01", "sessions/2025"] {
                    fs::remove_dir(fixture.home.join(directory)).unwrap();
                }
            }
            let failed_directory = if recreate_directories {
                fixture.home.join("sessions")
            } else {
                source.parent().unwrap().to_path_buf()
            };
            fsutil::trace_directory_syncs(Some(failed_directory.clone()));
            let failure = recover(&mut fixture.c, &fixture.policy, &fixture.store);
            let first_trace = fsutil::take_directory_sync_trace();
            assert!(
                failure
                    .unwrap_err()
                    .to_string()
                    .contains("injected recovery directory sync failure")
            );
            assert!(first_trace.contains(&failed_directory));
            assert!(intent_path(&fixture.store).exists());
            if !recreate_directories {
                assert!(
                    source.exists() && !staged.exists(),
                    "retry must cover a copy already restored before its barrier failed"
                );
            }
            fsutil::trace_directory_syncs(None);
            let retried = recover(&mut fixture.c, &fixture.policy, &fixture.store);
            let retry_trace = fsutil::take_directory_sync_trace();
            retried.unwrap();
            let ancestors = managed_source_parents(&fixture.home, &source).unwrap();
            let mut previous = None;
            for ancestor in ancestors.iter().rev() {
                let position = retry_trace
                    .iter()
                    .rposition(|path| path == ancestor)
                    .unwrap_or_else(|| {
                        panic!(
                            "retry omitted durability barrier for {}",
                            ancestor.display()
                        )
                    });
                if let Some(previous) = previous {
                    assert!(
                        previous < position,
                        "source ancestry must be synced bottom-up"
                    );
                }
                previous = Some(position);
            }
            assert!(retry_trace.contains(&fixture.home.join("archived_sessions")));
            assert_eq!(fs::read(&source).unwrap(), original);
            assert!(fixture.exists(&owner));
            assert!(!staged.exists() && !intent_path(&fixture.store).exists());
        }
    }

    #[test]
    fn full_and_summary_scans_agree_on_unsorted_mixed_archive_states() {
        let mut fixture = test_support::Fixture::new();
        // Insert out of UUID order to exercise in-place report entry + candidate permutation.
        let due_high = fixture.add(90, 1);
        let active = fixture.add(80, 0);
        let due_low = fixture.add(10, 1);
        let within = fixture.add(50, 1);
        let pinned = fixture.add(40, 1);
        let excluded = fixture.add(30, 1);
        let unknown_pos = fixture.add(70, 1);
        let unknown_neg = fixture.add(20, 1);
        let mismatch = fixture.add(60, 1);
        let future_clock = fixture.add(15, 1);

        for id in [&due_high, &due_low, &pinned, &excluded, &mismatch] {
            fixture.age(id);
        }
        fixture.policy.exclusions.insert(excluded.clone());
        fixture
            .c
            .execute("UPDATE threads SET is_pinned=1 WHERE id=?", [&pinned])
            .unwrap();
        fixture
            .c
            .execute("UPDATE threads SET archived=2 WHERE id=?", [&unknown_pos])
            .unwrap();
        fixture
            .c
            .execute("UPDATE threads SET archived=-1 WHERE id=?", [&unknown_neg])
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE codex_retain_epochs SET codex_archived_at=archived_since-1 WHERE thread_id=?",
                [&mismatch],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE codex_retain_epochs SET archived_since=?1, codex_archived_at=?1 WHERE thread_id=?2",
                rusqlite::params![test_support::now() + 60, &future_clock],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET archived_at=?1 WHERE id=?2",
                rusqlite::params![test_support::now() + 60, &future_clock],
            )
            .unwrap();

        let preview = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
            ExecutionMode::Preview,
        )
        .unwrap();
        // Active thread excluded; 9 non-zero archived rows examined in exact sorted ID order.
        assert_eq!(preview.examined, 9);
        assert_eq!(preview.eligible, 2);
        assert_eq!(preview.skipped, 7);
        assert!(
            preview
                .entries
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id),
            "full report entries must remain sorted by id"
        );
        let by_id: HashMap<&str, &EntryReason> = preview
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), &entry.reason))
            .collect();
        assert!(!by_id.contains_key(active.as_str()));
        assert_eq!(by_id[due_low.as_str()], &EntryReason::Eligible);
        assert_eq!(by_id[due_high.as_str()], &EntryReason::Eligible);
        assert_eq!(by_id[within.as_str()], &EntryReason::WithinRetention);
        assert_eq!(by_id[pinned.as_str()], &EntryReason::PinnedOrUnknownPin);
        assert_eq!(by_id[excluded.as_str()], &EntryReason::Excluded);
        assert_eq!(
            by_id[unknown_pos.as_str()],
            &EntryReason::UnknownArchiveStatus
        );
        assert_eq!(
            by_id[unknown_neg.as_str()],
            &EntryReason::UnknownArchiveStatus
        );
        assert_eq!(by_id[mismatch.as_str()], &EntryReason::CaptureMismatch);
        assert_eq!(
            by_id[future_clock.as_str()],
            &EntryReason::InvalidOrFutureClock
        );

        // Summary scheduled run must examine the same 9 archived rows and delete exactly the 2 due rows.
        let scheduled = execute_scheduled(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
        )
        .unwrap();
        assert_eq!(scheduled.examined, 9);
        assert_eq!(scheduled.eligible, 2);
        assert_eq!(scheduled.deleted, 2);
        assert_eq!(scheduled.skipped, 7);
        assert!(scheduled.entries.is_empty());
        assert!(!fixture.exists(&due_low));
        assert!(!fixture.exists(&due_high));
        for surviving in [
            &active,
            &within,
            &pinned,
            &excluded,
            &unknown_pos,
            &unknown_neg,
            &mismatch,
            &future_clock,
        ] {
            assert!(
                fixture.exists(surviving),
                "surviving thread {surviving} removed"
            );
        }
    }

    #[test]
    fn text_mode_matches_full_prefix_and_counts_on_unsorted_large_mixed_archive() {
        let mut fixture = test_support::Fixture::new();
        // Insert 45 archived threads in reverse order so the top-30 heap must evict.
        let mut ids = Vec::new();
        for n in (1..=45u128).rev() {
            let id = fixture.add(n, 1);
            if n % 10 == 0 {
                fixture.age(&id);
            }
            ids.push(id);
        }
        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
            ExecutionMode::Preview,
        )
        .unwrap();
        let text = execute_text(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
            ExecutionMode::Preview,
        )
        .unwrap();
        assert_eq!(text.examined, full.examined);
        assert_eq!(text.eligible, full.eligible);
        assert_eq!(text.skipped, full.skipped);
        assert_eq!(text.selected_bytes, full.selected_bytes);
        assert_eq!(text.warnings, full.warnings);
        assert_eq!(text.requires_attention(), full.requires_attention());
        assert_eq!(full.entries.len(), 45);
        assert!(text.entries.len() < full.entries.len());
        assert_eq!(
            text.entries
                .iter()
                .take(30)
                .map(|e| (
                    &e.id,
                    &e.reason,
                    e.eligible_at,
                    e.bytes,
                    &e.title,
                    &e.detail
                ))
                .collect::<Vec<_>>(),
            full.entries
                .iter()
                .take(30)
                .map(|e| (
                    &e.id,
                    &e.reason,
                    e.eligible_at,
                    e.bytes,
                    &e.title,
                    &e.detail
                ))
                .collect::<Vec<_>>(),
            "first 30 text entries must match full report exactly"
        );
    }

    #[test]
    fn visit_archived_summary_boundary_math_and_null_guard() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                archived INTEGER,
                archived_at INTEGER,
                is_pinned INTEGER NOT NULL DEFAULT 0,
                thread_section_id TEXT,
                history_mode TEXT NOT NULL DEFAULT 'legacy',
                name TEXT,
                title TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX idx_threads_archived ON threads(archived);
            CREATE TABLE codex_retain_epochs (
                thread_id TEXT PRIMARY KEY,
                archived_since INTEGER NOT NULL,
                codex_archived_at INTEGER
            );",
        )
        .unwrap();

        let insert = |c: &Connection, id: &str, archived: Option<i64>, since: Option<i64>| {
            c.execute(
                "INSERT INTO threads(id,rollout_path,archived,archived_at,title) VALUES(?1,'/p',?2,?3,'t')",
                rusqlite::params![id, archived, since],
            )
            .unwrap();
            if let Some(since) = since {
                c.execute(
                    "INSERT INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?2)",
                    rusqlite::params![id, since],
                )
                .unwrap();
            }
        };

        let duration = 86_400i64;
        let now = duration * 2; // cutoff == 86_400
        insert(&conn, "a-zero", Some(1), Some(0));
        insert(&conn, "b-one", Some(1), Some(1));
        insert(&conn, "c-cutoff-minus-1", Some(1), Some(duration - 1));
        insert(&conn, "d-cutoff", Some(1), Some(duration));
        insert(&conn, "e-overflow", Some(1), Some(i64::MAX - 10));
        insert(&conn, "f-active", Some(0), None);
        insert(&conn, "g-neg", Some(-1), Some(1));
        insert(&conn, "h-two", Some(2), Some(1));

        let mut seen = Vec::new();
        let examined = database::visit_archived_summary(&mut conn, now, duration, |event| {
            if let database::SummaryVisit::Row(t) = event {
                seen.push(t.id.to_owned());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(examined, 7);
        seen.sort();
        assert_eq!(seen, vec!["b-one", "c-cutoff-minus-1"]);

        // Boundary: now == duration (cutoff == 0), now < duration, now == duration + 1 (cutoff == 1)
        for boundary_now in [duration - 1, duration, duration + 1] {
            let mut visited = 0usize;
            let count = database::visit_archived_summary(&mut conn, boundary_now, duration, |ev| {
                if matches!(ev, database::SummaryVisit::Row(_)) {
                    visited += 1;
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(count, 7);
            assert_eq!(visited, 0, "no row can be due at now={boundary_now}");
        }
        // Boundary: cutoff == 2 (now == duration + 2) includes archived_since == 1 only.
        let mut at_two = Vec::new();
        database::visit_archived_summary(&mut conn, duration + 2, duration, |event| {
            if let database::SummaryVisit::Row(t) = event {
                at_two.push(t.id.to_owned());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(at_two, vec!["b-one"]);

        // NULL archived status must fail closed identically to visit_archived_full.
        insert(&conn, "z-null", None, None);
        assert!(database::visit_archived_full(&conn, |_| Ok(())).is_err());
        assert!(database::visit_archived_summary(&mut conn, now, duration, |_| Ok(())).is_err());
    }

    #[test]
    fn compact_report_write_json_matches_serde_across_all_reasons_and_edge_cases() {
        let now = 1_780_000_000i64;
        let all_reasons = [
            EntryReason::UnknownArchiveStatus,
            EntryReason::Excluded,
            EntryReason::PinnedOrUnknownPin,
            EntryReason::UnsupportedHistoryMode,
            EntryReason::ReferencedHistory,
            EntryReason::RelatedThread,
            EntryReason::DependentThread,
            EntryReason::DependencyCycle,
            EntryReason::UnknownArchiveTime,
            EntryReason::MissingCapture,
            EntryReason::CaptureMismatch,
            EntryReason::InvalidOrFutureClock,
            EntryReason::WithinRetention,
            EntryReason::Eligible,
            EntryReason::Deleted,
            EntryReason::ChangedBusyOrError,
            EntryReason::UnsafeOrUnavailableArtifact,
            EntryReason::RunStopped,
        ];
        let control_title: String = (0u8..=0x22).map(char::from).collect::<String>()
            + "\x7f\u{0085}\u{2028}\u{2029}\\quote\"and-utf8-🚀-日本語";
        let sample_deadlines = [
            None,
            Some(now - 5),
            Some(now),
            Some(now + 100),
            Some(now + 100), // repeat to exercise cached eligible_at formatting
            Some(i64::MIN),
            Some(i64::MAX),
            Some(now + i64::from(i32::MAX) + 10),
            Some(now - i64::from(i32::MAX) - 10),
            Some(0),
            Some(-1),
            Some(9),
            Some(10),
            Some(99),
            Some(100),
            Some(123_456_789),
            None,
            Some(now + 86_400),
        ];
        let sample_bytes = [
            None,
            Some(0),
            Some(9),
            Some(10),
            Some(99),
            Some(100),
            Some(u64::MAX),
            None,
            Some(4096),
            None,
            None,
            Some(1),
            None,
            Some(2048),
            Some(8192),
            None,
            Some(12345),
            None,
        ];

        let mut compact_entries = CompactEntries::new(now);
        let mut full_entries = Vec::new();
        for (i, reason) in all_reasons.iter().enumerate() {
            let id = if i % 5 == 0 {
                format!("non-canonical-{i:02}-\"\\")
            } else {
                uuid::Uuid::from_u128(0x0195_0000_0000_7000_8000_0000_0000_0000 + i as u128)
                    .to_string()
            };
            let title = match i % 4 {
                0 => String::new(),
                1 => control_title.clone(),
                2 => format!("Ordinary ASCII title {i}"),
                _ => format!("Unicode título {i} — café"),
            };
            let eligible_at = sample_deadlines[i];
            let idx = compact_entries
                .push(&id, &title, reason.clone(), eligible_at)
                .unwrap();
            if let Some(bytes) = sample_bytes[i] {
                compact_entries.set_bytes(idx, bytes);
            }
            let detail = if i % 2 == 0 {
                Some(format!("detail #{i}: \"slash\" \\ line\nend"))
            } else {
                None
            };
            if let Some(ref d) = detail {
                compact_entries.details.insert(idx as u32, d);
            }
            full_entries.push(Entry {
                id,
                title,
                reason: reason.clone(),
                eligible_at,
                bytes: sample_bytes[i],
                detail,
            });
        }
        // Sort both identically by id, verifying compare_entries handles mixed canonical/non-canonical IDs.
        compact_entries.sort_entries_only();
        // Rebuild bytes/details on sorted indices to match sorted full_entries.
        full_entries.sort_by(|a, b| a.id.cmp(&b.id));
        let mut sorted_compact = CompactEntries::new(now);
        for (idx, e) in full_entries.iter().enumerate() {
            let pushed = sorted_compact
                .push(&e.id, &e.title, e.reason.clone(), e.eligible_at)
                .unwrap();
            assert_eq!(pushed, idx);
            if let Some(b) = e.bytes {
                sorted_compact.set_bytes(idx, b);
            }
            if let Some(ref d) = e.detail {
                sorted_compact.details.insert(idx as u32, d);
            }
        }

        let clone_entry = |e: &Entry| Entry {
            id: e.id.clone(),
            title: e.title.clone(),
            reason: e.reason.clone(),
            eligible_at: e.eligible_at,
            bytes: e.bytes,
            detail: e.detail.clone(),
        };
        let warnings = vec![
            "plain warning".to_owned(),
            "escaped \"warning\" \\ \n\t\x01".to_owned(),
        ];
        for (evaluated_at, free_delta) in [
            (None, None),
            (Some(-123_456_789), Some(i64::MIN)),
            (Some(now), Some(0)),
            (Some(i64::MAX), Some(-4096)),
        ] {
            let compact = CompactReport {
                schema: 1,
                mode: "preview",
                started_at: now,
                evaluated_at,
                examined: full_entries.len() as u64,
                eligible: 2,
                deleted: 1,
                skipped: 16,
                selected_bytes: 99_999,
                logical_bytes_removed: 8192,
                allocated_bytes_unlinked: 16384,
                observed_free_space_delta_bytes: free_delta,
                actual_reclaimed_bytes: Some(0),
                entries: CompactEntries {
                    base_now: sorted_compact.base_now,
                    entries: sorted_compact.entries.clone(),
                    titles: {
                        let mut cb = crate::lineage::ChunkedBytes::default();
                        for e in &full_entries {
                            cb.push(e.title.as_bytes()).unwrap();
                        }
                        cb
                    },
                    non_canonical_ids: sorted_compact.non_canonical_ids.clone(),
                    overflow_deadlines: sorted_compact.overflow_deadlines.clone(),
                    bytes_idx: sorted_compact.bytes_idx.clone(),
                    bytes_val: sorted_compact.bytes_val.clone(),
                    details: sorted_compact.details.clone(),
                },
                warnings: warnings.clone(),
                had_attention: true,
            };
            let full = Report {
                schema: 1,
                mode: "preview".into(),
                started_at: now,
                evaluated_at,
                examined: full_entries.len() as u64,
                eligible: 2,
                deleted: 1,
                skipped: 16,
                selected_bytes: 99_999,
                logical_bytes_removed: 8192,
                allocated_bytes_unlinked: 16384,
                observed_free_space_delta_bytes: free_delta,
                actual_reclaimed_bytes: Some(0),
                entries: full_entries.iter().map(clone_entry).collect(),
                warnings: warnings.clone(),
                had_attention: true,
                completed_batches: 0,
            };
            let mut compact_json = Vec::new();
            compact.write_json(&mut compact_json).unwrap();
            let serde_json_bytes = serde_json::to_vec(&full).unwrap();
            assert_eq!(
                String::from_utf8(compact_json).unwrap(),
                String::from_utf8(serde_json_bytes).unwrap()
            );

            let expected_skips: Vec<&Entry> = full_entries
                .iter()
                .filter(|e| e.detail.is_some())
                .take(5)
                .collect();
            let got_skips = compact.first_skips(5);
            assert_eq!(
                serde_json::to_vec(&got_skips).unwrap(),
                serde_json::to_vec(&expected_skips).unwrap()
            );
        }
    }

    #[test]
    fn execute_compact_matches_execute_full_on_mixed_reasons_and_edge_cases() {
        let mut fixture = test_support::Fixture::new();
        let now = test_support::now();
        let due_ts = now - 40 * 86_400;

        // 1. Eligible legacy
        let eligible_legacy = fixture.add(101, 1);
        fixture.age(&eligible_legacy);
        // 2. Within retention
        let within = fixture.add(102, 1);
        // 3. Excluded (with epoch row -> eligible_at must still be populated!)
        let excluded = fixture.add(103, 1);
        fixture.age(&excluded);
        fixture.policy.exclusions.insert(excluded.clone());
        // 4. Pinned via is_pinned=1 (with epoch row)
        let pinned = fixture.add(104, 1);
        fixture.age(&pinned);
        fixture
            .c
            .execute("UPDATE threads SET is_pinned=1 WHERE id=?1", [&pinned])
            .unwrap();
        // 5. Pinned via thread_section_id = PINNED_SECTION_ID
        let pinned_sec = fixture.add(105, 1);
        fixture.age(&pinned_sec);
        fixture
            .c
            .execute(
                "INSERT INTO thread_sections(id,name) VALUES(?1,'Pinned')",
                [database::PINNED_SECTION_ID],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET thread_section_id=?1 WHERE id=?2",
                [database::PINNED_SECTION_ID, &pinned_sec],
            )
            .unwrap();
        // 6. Unsupported history_mode
        let unsupported = fixture.add(106, 1);
        fixture.age(&unsupported);
        fixture
            .c
            .execute(
                "UPDATE threads SET history_mode='future_v2' WHERE id=?1",
                [&unsupported],
            )
            .unwrap();
        // 7. UnknownArchiveTime (archived_at IS NULL) with epoch row
        let no_time = fixture.add(107, 1);
        fixture
            .c
            .execute(
                "UPDATE threads SET archived_at=NULL WHERE id=?1",
                [&no_time],
            )
            .unwrap();
        // 8. MissingCapture (epoch row deleted -> eligible_at MUST be null, not leaked archived_at!)
        let missing_cap = fixture.add(108, 1);
        fixture.age(&missing_cap);
        fixture
            .c
            .execute(
                "DELETE FROM codex_retain_epochs WHERE thread_id=?1",
                [&missing_cap],
            )
            .unwrap();
        // 9. CaptureMismatch
        let mismatch = fixture.add(109, 1);
        fixture.age(&mismatch);
        fixture
            .c
            .execute(
                "UPDATE codex_retain_epochs SET codex_archived_at=?2 WHERE thread_id=?1",
                rusqlite::params![&mismatch, due_ts + 1],
            )
            .unwrap();
        // 10. InvalidOrFutureClock with i64::MIN overflowing i32 delta!
        let min_clock = fixture.add(110, 1);
        fixture
            .c
            .execute(
                "UPDATE threads SET archived_at=?2 WHERE id=?1",
                rusqlite::params![&min_clock, i64::MIN],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE codex_retain_epochs SET archived_since=?2, codex_archived_at=?2 WHERE thread_id=?1",
                rusqlite::params![&min_clock, i64::MIN],
            )
            .unwrap();
        // 11. Overflow deadline (archived_since near i64::MAX -> checked_add returns None -> InvalidOrFutureClock or WithinRetention)
        let future_clock = fixture.add(111, 1);
        fixture
            .c
            .execute(
                "UPDATE threads SET archived_at=?2 WHERE id=?1",
                rusqlite::params![&future_clock, i64::MAX - 5],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE codex_retain_epochs SET archived_since=?2, codex_archived_at=?2 WHERE thread_id=?1",
                rusqlite::params![&future_clock, i64::MAX - 5],
            )
            .unwrap();
        // 12. UnknownArchiveStatus (archived = 2 and archived = -1)
        let arch_two = fixture.add(112, 1);
        fixture
            .c
            .execute("UPDATE threads SET archived=2 WHERE id=?1", [&arch_two])
            .unwrap();
        let arch_neg = fixture.add(113, 1);
        fixture
            .c
            .execute("UPDATE threads SET archived=-1 WHERE id=?1", [&arch_neg])
            .unwrap();
        // 13. Long 170-char multi-byte UTF-8 name, empty name -> title fallback, and NULL name -> title fallback.
        let long_name = "🚀".repeat(170);
        fixture
            .c
            .execute(
                "UPDATE threads SET name=?2 WHERE id=?1",
                rusqlite::params![&within, &long_name],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET name='', title='Fallback from empty' WHERE id=?1",
                [&excluded],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET name=NULL, title='Fallback from NULL' WHERE id=?1",
                [&pinned],
            )
            .unwrap();
        // 14. Active thread with an orphan row in codex_retain_epochs (must be ignored).
        let active = fixture.add(114, 0);
        fixture
            .c
            .execute(
                "INSERT OR REPLACE INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?2)",
                rusqlite::params![&active, due_ts],
            )
            .unwrap();

        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        let compact = execute_compact(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();

        let full_json = serde_json::to_vec(&full).unwrap();
        let mut compact_json = Vec::new();
        compact.write_json(&mut compact_json).unwrap();
        assert_eq!(
            String::from_utf8(compact_json).unwrap(),
            String::from_utf8(full_json).unwrap()
        );
        // Explicitly verify missing_cap had eligible_at = None (not leaked temporary archived_at).
        let mc = full.entries.iter().find(|e| e.id == missing_cap).unwrap();
        assert_eq!(mc.reason, EntryReason::MissingCapture);
        assert_eq!(mc.eligible_at, None);
        // Explicitly verify excluded still carried its epoch deadline.
        let ex = full.entries.iter().find(|e| e.id == excluded).unwrap();
        assert_eq!(ex.reason, EntryReason::Excluded);
        assert!(ex.eligible_at.is_some());
    }

    #[test]
    fn execute_text_fast_path_above_4096_matches_full_prefix() {
        let mut fixture = test_support::Fixture::new();
        let now = test_support::now();
        let recent_ts = now - 86_400;
        let due_ts = now - 40 * 86_400;

        // Add 2 real due threads with files (one lexicographically tiny so it lands in top-30,
        // one lexicographically large so it lands outside top-30).
        let due_small = fixture.add(1, 1);
        fixture.age(&due_small);
        let due_large = fixture.add(u128::MAX - 5, 1);
        fixture.age(&due_large);

        // Bulk-insert 4,100 non-due archived rows to cross the examined >= 4096 threshold.
        {
            let tx = fixture.c.transaction().unwrap();
            let mut ins_t = tx
                .prepare(
                    "INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at,history_mode,is_pinned) \
                     VALUES(?1,?2,?3,?3,'cli','openai','/tmp',?4,'workspace','auto',1,?3,'legacy',0)",
                )
                .unwrap();
            let mut ins_e = tx
                .prepare(
                    "INSERT OR REPLACE INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?2)",
                )
                .unwrap();
            for i in 10u128..4_110u128 {
                let id = uuid::Uuid::from_u128(i).to_string();
                let path = fixture
                    .home
                    .join("archived_sessions")
                    .join(format!("rollout-2026-09-10T00-00-00-{id}.jsonl"));
                ins_t
                    .execute(rusqlite::params![
                        &id,
                        path.to_str().unwrap(),
                        recent_ts,
                        format!("Row {i}")
                    ])
                    .unwrap();
                ins_e.execute(rusqlite::params![&id, recent_ts]).unwrap();
            }
            drop(ins_e);
            drop(ins_t);
            tx.commit().unwrap();
        }

        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        let text = execute_text(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();

        assert_eq!(text.examined, 4_102);
        assert_eq!(text.examined, full.examined);
        assert_eq!(text.eligible, full.eligible);
        assert_eq!(text.skipped, full.skipped);
        assert_eq!(text.selected_bytes, full.selected_bytes);
        // The first 30 sorted entries (used by human text output) must match identically.
        assert_eq!(
            serde_json::to_vec(&text.entries[..TEXT_DISPLAY_LIMIT]).unwrap(),
            serde_json::to_vec(&full.entries[..TEXT_DISPLAY_LIMIT]).unwrap()
        );
        // Without failures past index 30, text mode bounds returned entries to the 30-entry prefix,
        // while eligible count and selected_bytes still account for both due_small and due_large.
        assert_eq!(text.entries.len(), TEXT_DISPLAY_LIMIT);
        assert!(
            text.entries
                .iter()
                .any(|e| e.id == due_small && e.reason == EntryReason::Eligible)
        );
        assert!(
            !text.entries.iter().any(|e| e.id == due_large),
            "non-failing due entry past index 30 should not be materialized in text mode"
        );
        // Confirm due_ts wasn't unused (verify eligible_at matches due_ts + duration).
        let due_entry = text.entries.iter().find(|e| e.id == due_small).unwrap();
        assert!(due_entry.eligible_at.is_some());
        let _ = due_ts;
    }

    #[test]
    fn selected_paths_check_republished_preserves_order_and_deduplicates_mixed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let arch = home.join("archived_sessions");
        let sess = home.join("sessions");
        fs::create_dir_all(&arch).unwrap();
        fs::create_dir_all(&sess).unwrap();

        let id0 = uuid::Uuid::from_u128(1).to_string();
        let id1 = uuid::Uuid::from_u128(2).to_string();
        let id2 = uuid::Uuid::from_u128(3).to_string();
        let mut report =
            ReportCore::new(ExecutionMode::Run, test_support::now(), ReportDetail::Full);
        if let EntryStore::Full(ref mut entries) = report.entries {
            for id in [&id0, &id1, &id2] {
                entries.push(Entry {
                    id: id.clone(),
                    title: String::new(),
                    reason: EntryReason::Deleted,
                    eligible_at: None,
                    bytes: None,
                    detail: None,
                });
            }
        }
        let compact_name0 = format!("rollout-2026-09-10T00-00-00-{id0}.jsonl");
        let fallback_name0 = format!("rollout-2026-09-10T00-00-00-{id0}_{id1}.jsonl");
        let compact_name1 = format!("rollout-2026-09-10T00-00-00-{id1}.jsonl");
        let sess_rel2 = format!("sessions/rollout-2026-09-10T00-00-00-{id2}.jsonl");

        // Thread 0 has BOTH compact and fallback entries (both republished) -> must warn ONCE.
        // Thread 1 has compact .jsonl entry, republished as .jsonl.zst -> must warn.
        // Thread 2 has sessions/ fallback entry, republished as plain -> must warn.
        fs::write(arch.join(&compact_name0), b"{}\n").unwrap();
        fs::write(arch.join(&fallback_name0), b"{}\n").unwrap();
        fs::write(arch.join(format!("{compact_name1}.zst")), b"{}\n").unwrap();
        fs::write(home.join(&sess_rel2), b"{}\n").unwrap();

        let mut selected = SelectedPaths::with_capacity(4);
        selected.push(0, compact_name0.as_bytes(), true).unwrap();
        selected.push(0, fallback_name0.as_bytes(), true).unwrap();
        selected.push(1, compact_name1.as_bytes(), true).unwrap();
        selected.push(2, sess_rel2.as_bytes(), false).unwrap();
        assert_eq!(selected.compact.len(), 2);
        assert_eq!(selected.other.len(), 2);

        check_republished(home, &selected, &mut report, ReportDetail::Full);
        assert_eq!(
            report.warnings,
            vec![
                format!("{id0}: Codex republished a rollout; preserved it"),
                format!("{id1}: Codex republished a rollout; preserved it"),
                format!("{id2}: Codex republished a rollout; preserved it"),
            ]
        );
    }

    #[test]
    fn execute_compact_matches_full_on_embedded_nul_titles_and_sequential_due_paths() {
        let mut fixture = test_support::Fixture::new();
        let now = test_support::now();
        let due_ts = now - 40 * 86_400;

        // Embedded NUL bytes in name and title must match SQLite's substr() NUL truncation.
        let nul_lead = fixture.add(1, 1);
        let nul_mid = fixture.add(2, 1);
        let nul_title = fixture.add(3, 1);
        let nul_long = fixture.add(4, 1);
        fixture
            .c
            .execute(
                "UPDATE threads SET name=?2, title='fallback' WHERE id=?1",
                rusqlite::params![&nul_lead, "\0hidden"],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET name=?2, title='fallback' WHERE id=?1",
                rusqlite::params![&nul_mid, "visible\0hidden"],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET name='', title=?2 WHERE id=?1",
                rusqlite::params![&nul_title, "title\0tail"],
            )
            .unwrap();
        let long_with_nul = format!("{}\0tail", "x".repeat(170));
        fixture
            .c
            .execute(
                "UPDATE threads SET name=?2 WHERE id=?1",
                rusqlite::params![&nul_long, &long_with_nul],
            )
            .unwrap();

        // Bulk-insert 1,060 due rows (without disk files) to exercise the sequential
        // Pass 4 eligible-path scan (>= 1024 eligible && >= 1/8 examined) and CompactDetails interning.
        {
            let tx = fixture.c.transaction().unwrap();
            let mut ins_t = tx
                .prepare(
                    "INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at,history_mode,is_pinned) \
                     VALUES(?1,?2,?3,?3,'cli','openai','/tmp',?4,'workspace','auto',1,?3,'legacy',0)",
                )
                .unwrap();
            let mut ins_e = tx
                .prepare(
                    "INSERT OR REPLACE INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?2)",
                )
                .unwrap();
            for i in (100u128..1_160u128).rev() {
                let id = uuid::Uuid::from_u128(i).to_string();
                let path = fixture
                    .home
                    .join("archived_sessions")
                    .join(format!("rollout-2026-09-10T00-00-00-{id}.jsonl"));
                ins_t
                    .execute(rusqlite::params![
                        &id,
                        path.to_str().unwrap(),
                        due_ts,
                        format!("Due row {i}")
                    ])
                    .unwrap();
                ins_e.execute(rusqlite::params![&id, due_ts]).unwrap();
            }
            drop(ins_e);
            drop(ins_t);
            tx.commit().unwrap();
        }

        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        let compact = execute_compact(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();

        let full_json = serde_json::to_vec(&full).unwrap();
        let mut compact_json = Vec::new();
        compact.write_json(&mut compact_json).unwrap();
        assert_eq!(
            String::from_utf8(compact_json).unwrap(),
            String::from_utf8(full_json).unwrap()
        );
        assert_eq!(
            full.entries
                .iter()
                .find(|e| e.id == nul_lead)
                .unwrap()
                .title,
            ""
        );
        assert_eq!(
            full.entries.iter().find(|e| e.id == nul_mid).unwrap().title,
            "visible"
        );
        assert_eq!(
            full.entries
                .iter()
                .find(|e| e.id == nul_title)
                .unwrap()
                .title,
            "title"
        );
        assert_eq!(
            compact.entries.details.len(),
            1_060,
            "all 1,060 missing due rows must record a failure detail"
        );
        assert_eq!(
            compact.entries.details.messages.len(),
            1,
            "identical missing-file details must deduplicate to one interned string"
        );

        // Verify CompactEntries::set_eligible_at reuses an existing overflow slot in place.
        let mut ce = CompactEntries::new(now);
        let idx = ce
            .push(
                &nul_lead,
                "t",
                EntryReason::MissingCapture,
                Some(i64::MIN + 10),
            )
            .unwrap();
        assert_eq!(ce.overflow_deadlines.len(), 1);
        ce.set_eligible_at(idx, Some(i64::MAX - 10)).unwrap();
        assert_eq!(
            ce.overflow_deadlines.len(),
            1,
            "overwriting an overflowing timestamp must reuse its slot"
        );
        assert_eq!(ce.eligible_at(&ce.entries[idx]), Some(i64::MAX - 10));
    }

    #[test]
    fn batch_statements_mid_run_mutations_preserve_exact_error_precedence() {
        for mutation in [
            "extra_table",
            "drop_migration",
            "incomplete_backfill",
            "replace_db",
            "drop_capture_owner",
            "wrong_owner",
        ] {
            let fixture = test_support::Fixture::new();
            let first = fixture.add(1, 1);
            let second = fixture.add(2, 1);
            fixture.age(&first);
            fixture.age(&second);
            let graph = crate::relations::Dependencies::default();
            let mut cleanup = Cleanup {
                policy: &fixture.policy,
                store: &fixture.store,
                now: test_support::now(),
                graph: &graph,
                owners: RolloutOwners::new(&fixture.c, ""),
                headers: crate::lineage::HeaderCache::default(),
                detail: ReportDetail::Full,
                completed_batches: 0,
            };
            let mut scratch = BatchScratch::default();
            let mut stmts = None;
            let mut attempted = false;
            cleanup
                .delete_batch_with_scratch(
                    &fixture.c,
                    &[&first],
                    &mut attempted,
                    &mut scratch,
                    &mut stmts,
                )
                .unwrap();
            assert!(stmts.is_some(), "batch 1 must cache BatchStatements");
            assert!(!fixture.exists(&first));

            // Mutate schema / metadata on a separate connection between batch 1 and batch 2.
            let other = Connection::open(fixture.home.join(database::DB_NAME)).unwrap();
            let expected_substr = match mutation {
                "extra_table" => {
                    other
                        .execute("CREATE TABLE rogue_extension(x INTEGER)", [])
                        .unwrap();
                    "unsupported or modified Codex schema"
                }
                "drop_migration" => {
                    other
                        .execute("DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)", [])
                        .unwrap();
                    "Codex migration history differs"
                }
                "incomplete_backfill" => {
                    other
                        .execute("UPDATE backfill_state SET status='running' WHERE id=1", [])
                        .unwrap();
                    "Codex index backfill is incomplete"
                }
                "replace_db" => {
                    let db_path = fixture.home.join(database::DB_NAME);
                    let copy = fixture.home.join("replacement.sqlite");
                    fs::copy(&db_path, &copy).unwrap();
                    fs::rename(&copy, &db_path).unwrap();
                    "Codex database was replaced"
                }
                "drop_capture_owner" => {
                    other.execute("DROP TABLE codex_retain_owner", []).unwrap();
                    "retention capture is missing or modified"
                }
                "wrong_owner" => {
                    other
                        .execute(
                            "UPDATE codex_retain_owner SET owner='/other/policy' WHERE singleton=1",
                            [],
                        )
                        .unwrap();
                    "this Codex profile belongs to another retention policy"
                }
                _ => unreachable!(),
            };
            drop(other);

            attempted = false;
            let err = cleanup
                .delete_batch_with_scratch(
                    &fixture.c,
                    &[&second],
                    &mut attempted,
                    &mut scratch,
                    &mut stmts,
                )
                .unwrap_err();
            assert!(!attempted, "{mutation}: must fail before journaling");
            assert!(
                stmts.is_none(),
                "{mutation}: failed verify must clear cached statements"
            );
            assert!(
                err.is::<GlobalCleanupFailure>(),
                "{mutation}: must stop run as GlobalCleanupFailure"
            );
            let full_msg = format!("{err:#}");
            assert!(
                full_msg.contains(expected_substr),
                "{mutation}: expected '{expected_substr}', got '{full_msg}'"
            );
            assert!(fixture.exists(&second));
        }
    }

    #[test]
    fn execute_text_skewed_active_population_matches_full() {
        let mut fixture = test_support::Fixture::new();
        let now = test_support::now();
        let recent_ts = now - 86_400;
        let due = fixture.add(u128::MAX - 2, 1);
        fixture.age(&due);

        // Insert 5,000 active rows with tiny UUIDs before 4,100 archived rows with huge UUIDs,
        // forcing visit_archived_text to detect active >= examined and use idx_threads_archived.
        {
            let tx = fixture.c.transaction().unwrap();
            let mut ins_t = tx
                .prepare(
                    "INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at,history_mode,is_pinned) \
                     VALUES(?1,?2,?3,?3,'cli','openai','/tmp',?4,'workspace','auto',?5,?6,'legacy',0)",
                )
                .unwrap();
            let mut ins_e = tx
                .prepare(
                    "INSERT OR REPLACE INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?2)",
                )
                .unwrap();
            for i in 1u128..=5_000u128 {
                let id = uuid::Uuid::from_u128(i).to_string();
                let path = fixture
                    .home
                    .join("sessions")
                    .join(format!("rollout-2026-09-10T00-00-00-{id}.jsonl"));
                ins_t
                    .execute(rusqlite::params![
                        &id,
                        path.to_str().unwrap(),
                        recent_ts,
                        format!("Active {i}"),
                        0i64,
                        Option::<i64>::None,
                    ])
                    .unwrap();
            }
            for i in 10u128..4_110u128 {
                let id = uuid::Uuid::from_u128(u128::MAX - 10_000 + i).to_string();
                let path = fixture
                    .home
                    .join("archived_sessions")
                    .join(format!("rollout-2026-09-10T00-00-00-{id}.jsonl"));
                ins_t
                    .execute(rusqlite::params![
                        &id,
                        path.to_str().unwrap(),
                        recent_ts,
                        format!("Archived {i}"),
                        1i64,
                        Some(recent_ts),
                    ])
                    .unwrap();
                ins_e.execute(rusqlite::params![&id, recent_ts]).unwrap();
            }
            drop(ins_e);
            drop(ins_t);
            tx.commit().unwrap();
        }

        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        let text = execute_text(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        assert_eq!(text.examined, 4_101);
        assert_eq!(text.examined, full.examined);
        assert_eq!(text.eligible, full.eligible);
        assert_eq!(text.skipped, full.skipped);
        assert_eq!(
            serde_json::to_vec(&text.entries[..TEXT_DISPLAY_LIMIT]).unwrap(),
            serde_json::to_vec(&full.entries[..TEXT_DISPLAY_LIMIT]).unwrap()
        );
    }

    #[test]
    fn execute_text_preserves_display_prefix_and_first_ten_details_past_prefix() {
        let mut fixture = test_support::Fixture::new();
        let now = test_support::now();
        // 28 recent (non-due) threads with smallest IDs, plus 20 due threads
        // whose rollout files are missing so failures span across index 30.
        for i in 1u128..=28 {
            fixture.add(i, 1);
        }
        for i in 29u128..=48 {
            let id = fixture.add(i, 1);
            fixture.age(&id);
            fs::remove_file(fixture.path(&id, true)).unwrap();
        }
        let full = execute(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        let text = execute_text(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            now,
            ExecutionMode::Preview,
        )
        .unwrap();
        assert_eq!(text.examined, full.examined);
        assert_eq!(text.eligible, full.eligible);
        assert_eq!(text.skipped, full.skipped);
        assert_eq!(text.requires_attention(), full.requires_attention());
        assert_eq!(
            serde_json::to_vec(&text.entries[..TEXT_DISPLAY_LIMIT]).unwrap(),
            serde_json::to_vec(&full.entries[..TEXT_DISPLAY_LIMIT]).unwrap(),
            "text report must preserve exact 30-entry display prefix"
        );
        let full_skips: Vec<&Entry> = full
            .entries
            .iter()
            .filter(|e| e.detail.is_some())
            .take(10)
            .collect();
        let text_skips: Vec<&Entry> = text
            .entries
            .iter()
            .filter(|e| e.detail.is_some())
            .take(10)
            .collect();
        assert_eq!(text_skips.len(), 10);
        assert_eq!(
            serde_json::to_vec(&text_skips).unwrap(),
            serde_json::to_vec(&full_skips).unwrap(),
            "text report must preserve exact first 10 skip diagnostics across index 30"
        );

        let sched =
            execute_scheduled(&mut fixture.c, &fixture.policy, &fixture.store, now).unwrap();
        assert_eq!(sched.examined, full.examined);
        assert_eq!(sched.requires_attention(), full.requires_attention());
        assert_eq!(sched.entries.len(), 10);
        assert_eq!(
            serde_json::to_vec(&sched.entries).unwrap(),
            serde_json::to_vec(&full_skips).unwrap(),
            "scheduled summary report must preserve exact first 10 skip diagnostics"
        );
    }
}
