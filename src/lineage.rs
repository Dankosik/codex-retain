//! Conservative ownership and incoming-reference inventory for local rollouts.
//!
//! This is a filesystem snapshot, not a cross-process fork reservation. Callers
//! must separately enforce archived eligibility and mutation coordination.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct Rollout {
    pub rollout_id: Uuid,
    pub path: PathBuf,
    pub paginated: bool,
    pub identity: crate::fsutil::Identity,
}

#[derive(Default)]
pub(crate) struct LineageIndex {
    owned: HashMap<Uuid, Vec<Rollout>>,
    incoming: HashMap<Uuid, ReferringOwners>,
    owners: HashMap<Uuid, Uuid>,
    parents: HashSet<(Uuid, Uuid)>,
    references: Vec<(Uuid, Uuid)>,
}

struct ReferringOwners {
    first: Uuid,
    mixed: bool,
}

impl LineageIndex {
    pub(crate) fn owned(&self, owner: Uuid) -> &[Rollout] {
        self.owned.get(&owner).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn externally_referenced(&self, rollout: Uuid, owner: Uuid) -> bool {
        self.incoming
            .get(&rollout)
            .is_some_and(|sources| sources.mixed || sources.first != owner)
    }

    pub(crate) fn add_dependencies(&self, graph: &mut crate::relations::Dependencies) {
        for &(child, parent) in &self.parents {
            graph.add_parent(child, parent);
        }
        for &(dependent, rollout) in &self.references {
            if let Some(&source) = self.owners.get(&rollout)
                && source != dependent
            {
                graph.add_history(dependent, source);
            }
        }
    }
}

/// Run-local header reuse only. Directory discovery, graph construction and
/// candidate validation remain live; SQLite versions cannot describe orphans.
#[derive(Default)]
pub(crate) struct HeaderCache {
    entries: HashMap<PathBuf, CachedHeader>,
    generation: bool,
    #[cfg(test)]
    reads: usize,
}

struct CachedHeader {
    stamp: FileStamp,
    parsed: ParsedHeader,
    generation: bool,
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    identity: crate::fsutil::Identity,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
}

impl FileStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            identity: crate::fsutil::Identity::of(metadata),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            links: metadata.nlink(),
        }
    }
}

fn read_header(
    path: &Path,
    compressed: bool,
    mut cache: Option<&mut HeaderCache>,
) -> Result<(ParsedHeader, crate::fsutil::Identity)> {
    if let Some(cache) = cache.as_deref_mut()
        && let Some(cached) = cache.entries.get_mut(path)
    {
        // lstat on every hit detects replaced paths, symlinks, hardlinks,
        // appends, in-place edits and permission changes. Never use directory
        // timestamps or a database-only freshness token for header reuse.
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect rollout {}", path.display()))?;
        if FileStamp::of(&metadata) == cached.stamp {
            let parsed = cached.parsed;
            let identity = cached.stamp.identity.clone();
            cached.generation = cache.generation;
            return Ok((parsed, identity));
        }
    }
    let (file, metadata) = crate::fsutil::regular_with_metadata(path, false, false)?;
    let stamp = FileStamp::of(&metadata);
    let line = crate::metadata::first_record(&file, compressed)
        .with_context(|| format!("read rollout header {}", path.display()))?;
    let parsed =
        header(&line).with_context(|| format!("invalid rollout header {}", path.display()))?;
    let identity = stamp.identity.clone();
    if let Some(cache) = cache {
        // Bind a successful parse to the same opened object's stable metadata.
        // A changing file fails this scan rather than seeding a stale cache.
        ensure!(
            FileStamp::of(&file.metadata()?) == stamp,
            "rollout changed while reading header: {}",
            path.display()
        );
        cache.entries.insert(
            path.to_path_buf(),
            CachedHeader {
                stamp,
                parsed,
                generation: cache.generation,
            },
        );
        #[cfg(test)]
        {
            cache.reads += 1;
        }
    }
    Ok((parsed, identity))
}

pub(crate) fn scan(
    home: &Path,
    candidate_owners: &HashSet<Uuid>,
    mut cache: Option<&mut HeaderCache>,
) -> Result<LineageIndex> {
    if let Some(cache) = cache.as_mut() {
        cache.generation = !cache.generation;
    }
    let result = scan_inventory(home, candidate_owners, cache.as_deref_mut());
    if let Some(cache) = cache {
        if result.is_ok() {
            cache
                .entries
                .retain(|_, header| header.generation == cache.generation);
        } else {
            cache.entries.clear();
        }
    }
    result
}

fn scan_inventory(
    home: &Path,
    candidate_owners: &HashSet<Uuid>,
    mut cache: Option<&mut HeaderCache>,
) -> Result<LineageIndex> {
    let mut index = LineageIndex::default();
    let mut entries = 0usize;
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("inspect session directory"),
        };
        ensure!(metadata.is_dir(), "session root is not a real directory");
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .max_open(16)
        {
            let entry = entry.context("cannot inspect all session lineage paths")?;
            entries += 1;
            ensure!(
                entries <= 500_000,
                "lineage inventory exceeds the 500,000-entry safety limit"
            );
            ensure!(
                !entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".codex-retain-pending-"),
                "pending retention artifact prevents lineage verification: {}",
                entry.path().display()
            );
            let kind = entry.file_type();
            ensure!(
                kind.is_dir() || kind.is_file(),
                "lineage inventory contains a symlink or nonregular entry: {}",
                entry.path().display()
            );
            if kind.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(filename) = filename(name)? else {
                continue;
            };
            let rollout_id = filename.rollout;
            let (parsed, identity) =
                read_header(entry.path(), filename.compressed, cache.as_deref_mut())?;
            let ParsedHeader {
                owner,
                paginated,
                base,
                parent,
            } = parsed;
            if let Some(previous) = index.owners.insert(rollout_id, owner) {
                ensure!(
                    previous == owner,
                    "rollout identity {rollout_id} has conflicting owners"
                );
            }
            ensure!(
                owner == filename.owner,
                "rollout filename and metadata owners differ"
            );
            if let Some(base) = base {
                index.references.push((owner, base));
                index
                    .incoming
                    .entry(base)
                    .and_modify(|sources| {
                        sources.mixed |= sources.first != owner;
                    })
                    .or_insert(ReferringOwners {
                        first: owner,
                        mixed: false,
                    });
            }
            if let Some(parent) = parent {
                index.parents.insert((owner, parent));
            }
            if candidate_owners.contains(&owner) {
                index.owned.entry(owner).or_default().push(Rollout {
                    rollout_id,
                    path: entry.into_path(),
                    paginated,
                    identity,
                });
            }
        }
    }
    for rollouts in index.owned.values_mut() {
        rollouts.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(index)
}

fn canonical_uuid(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value).context("invalid rollout UUID")?;
    ensure!(
        id.hyphenated().encode_lower(&mut [0; 36]) == value,
        "noncanonical rollout UUID"
    );
    Ok(id)
}

pub(crate) struct RolloutName {
    pub owner: Uuid,
    pub rollout: Uuid,
    pub compressed: bool,
}

/// Ordinary names end in owner UUID; reverted names end in owner_rollout UUIDs.
pub(crate) fn filename(name: &str) -> Result<Option<RolloutName>> {
    if !name.starts_with("rollout-") {
        return Ok(None);
    }
    let (stem, compressed) = if let Some(stem) = name.strip_suffix(".jsonl.zst") {
        (stem, true)
    } else if let Some(stem) = name.strip_suffix(".jsonl") {
        (stem, false)
    } else {
        return Ok(None);
    };
    let core = stem
        .strip_prefix("rollout-")
        .context("invalid rollout filename")?;
    let timestamp = core.get(..19).context("rollout filename lacks timestamp")?;
    jiff::civil::DateTime::strptime("%Y-%m-%dT%H-%M-%S", timestamp)
        .context("invalid rollout filename timestamp")?;
    ensure!(
        core.get(19..20) == Some("-"),
        "rollout filename lacks UUID separator"
    );
    let ids = core.get(20..).context("rollout filename lacks UUID")?;
    let (owner, rollout) = ids.split_once('_').unwrap_or((ids, ids));
    Ok(Some(RolloutName {
        owner: canonical_uuid(owner)?,
        rollout: canonical_uuid(rollout)?,
        compressed,
    }))
}

// Derived map deserialization rejects duplicate known fields. Validate the full
// JSON value too, including fields these selective structs do not retain.
#[derive(Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    id: String,
    history_mode: Option<String>,
    history_base: Option<Base>,
    parent_thread_id: Option<String>,
    source: Option<SessionSource>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionSource {
    // Unit source names have no organizational parent; retain their type check.
    #[allow(dead_code)]
    Name(String),
    Object(SourceFields),
}

#[derive(Deserialize)]
struct SourceFields {
    subagent: Option<SubagentSource>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SubagentSource {
    #[allow(dead_code)]
    Name(String),
    Object(SubagentFields),
}

#[derive(Deserialize)]
struct SubagentFields {
    thread_spawn: Option<SpawnSource>,
}

#[derive(Deserialize)]
struct SpawnSource {
    parent_thread_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedHeader {
    owner: Uuid,
    paginated: bool,
    base: Option<Uuid>,
    parent: Option<Uuid>,
}

#[derive(Deserialize)]
struct Base {
    thread_id: String,
    end_ordinal_exclusive: u64,
    end_byte_offset: u64,
}

fn header(line: &[u8]) -> Result<ParsedHeader> {
    let value: Value = serde_json::from_slice(line)?;
    ensure!(
        value.is_object() && value["payload"].is_object(),
        "rollout header and payload must be objects"
    );
    let base = &value["payload"]["history_base"];
    ensure!(
        base.is_null() || base.is_object(),
        "history_base must be an object or null"
    );
    if let Some(spawn) = value.pointer("/payload/source/subagent/thread_spawn") {
        ensure!(spawn.is_object(), "thread_spawn source must be an object");
    }
    drop(value);
    let record: Header = serde_json::from_slice(line)?;
    ensure!(
        record.kind == "session_meta",
        "first record is not session_meta"
    );
    let owner = canonical_uuid(&record.payload.id)?;
    let paginated = match record.payload.history_mode.as_deref() {
        None | Some("legacy") => false,
        Some("paginated") => true,
        Some(_) => bail!("unsupported rollout history mode"),
    };
    let base = record
        .payload
        .history_base
        .map(|base| {
            let _ = (base.end_ordinal_exclusive, base.end_byte_offset);
            canonical_uuid(&base.thread_id)
        })
        .transpose()?;
    ensure!(
        paginated || base.is_none(),
        "legacy rollout contains a history reference"
    );
    let explicit_parent = record
        .payload
        .parent_thread_id
        .as_deref()
        .map(canonical_uuid)
        .transpose()?;
    let source_parent = match record.payload.source {
        Some(SessionSource::Object(SourceFields {
            subagent:
                Some(SubagentSource::Object(SubagentFields {
                    thread_spawn: Some(spawn),
                })),
        })) => Some(canonical_uuid(&spawn.parent_thread_id)?),
        _ => None,
    };
    ensure!(
        explicit_parent.is_none() || source_parent.is_none() || explicit_parent == source_parent,
        "conflicting recorded parent identities"
    );
    Ok(ParsedHeader {
        owner,
        paginated,
        base,
        parent: explicit_parent.or(source_parent),
    })
}

/// Journal schema 3 cannot infer the stable owner from the filename UUID.
pub(crate) fn validate_owner(line: &[u8], expected: Uuid) -> Result<()> {
    // A committed journal already authorized removal. Recovery binds identity,
    // rather than reinterpreting optional metadata under a newer adapter policy.
    #[derive(Deserialize)]
    struct OwnerHeader {
        #[serde(rename = "type")]
        kind: String,
        payload: OwnerPayload,
    }
    #[derive(Deserialize)]
    struct OwnerPayload {
        id: String,
    }
    let _: Value = serde_json::from_slice(line)?;
    let record: OwnerHeader = serde_json::from_slice(line)?;
    ensure!(
        record.kind == "session_meta",
        "staged header is not session_meta"
    );
    let owner = canonical_uuid(&record.payload.id)?;
    ensure!(
        owner == expected,
        "staged rollout belongs to a different thread"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: Uuid = Uuid::from_u128(1);
    const CHILD: Uuid = Uuid::from_u128(2);
    const OLD: Uuid = Uuid::from_u128(3);

    fn write(
        home: &Path,
        directory: &str,
        rollout: Uuid,
        owner: Uuid,
        base: Option<Uuid>,
        compressed: bool,
    ) -> PathBuf {
        let directory = home.join(directory);
        fs::create_dir_all(&directory).unwrap();
        let ids = if owner == rollout {
            owner.to_string()
        } else {
            format!("{owner}_{rollout}")
        };
        let path = directory.join(format!(
            "rollout-2026-09-10T00-00-00-{ids}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        let value = json!({"type":"session_meta", "payload": {
            "id":owner, "history_mode":"paginated", "history_base":base.map(|id| json!({
                "thread_id":id,"end_ordinal_exclusive":10,"end_byte_offset":100
            }))
        }});
        let bytes = format!("{value}\nthis transcript is deliberately not JSON\n").into_bytes();
        fs::write(
            &path,
            if compressed {
                zstd::stream::encode_all(bytes.as_slice(), 0).unwrap()
            } else {
                bytes
            },
        )
        .unwrap();
        path
    }

    #[test]
    fn protects_sources_from_active_archived_and_compressed_orphan_rollouts() {
        for (directory, compressed) in [
            ("sessions/2026/09/10", false),
            ("archived_sessions", false),
            ("sessions/2026/09/10", true),
        ] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            write(
                home.path(),
                directory,
                CHILD,
                CHILD,
                Some(OWNER),
                compressed,
            );
            // No database exists: an orphan rollout still carries a real edge.
            let index = scan(home.path(), &HashSet::from([OWNER]), None).unwrap();
            assert!(index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 1);
            assert!(index.owned(OWNER)[0].paginated);
            assert!(index.owned(CHILD).is_empty());
        }
    }

    #[test]
    fn cached_scans_discover_new_copies_and_forget_removed_references() {
        for compressed in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let source = write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::default();
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert_eq!(cache.reads, 1, "unchanged headers must not be reopened");

            let child = write(
                home.path(),
                "sessions/nested",
                CHILD,
                CHILD,
                Some(OWNER),
                compressed,
            );
            let copy = home
                .path()
                .join("sessions")
                .join(source.file_name().unwrap());
            fs::copy(&source, &copy).unwrap();
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 2);
            assert_eq!(cache.reads, 3);

            fs::remove_file(child).unwrap();
            fs::remove_file(copy).unwrap();
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(!index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 1);
            assert_eq!(cache.entries.len(), 1);
            assert_eq!(cache.reads, 3);
        }
    }

    #[test]
    fn cached_header_edits_cannot_hide_new_dependencies() {
        use std::io::Write;
        for change in [
            "same-size",
            "restored-mtime",
            "replace",
            "append",
            "compressed",
        ] {
            let home = tempfile::tempdir().unwrap();
            let compressed = change == "compressed";
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let child = write(home.path(), "sessions", CHILD, CHILD, Some(OLD), compressed);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::default();
            let initial = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(!initial.externally_referenced(OWNER, OWNER));
            let before = fs::metadata(&child).unwrap();
            if change == "append" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&child)
                    .unwrap()
                    .write_all(b"another record\n")
                    .unwrap();
            } else {
                if change == "replace" {
                    fs::rename(&child, home.path().join("old-inode")).unwrap();
                }
                write(
                    home.path(),
                    "sessions",
                    CHILD,
                    CHILD,
                    Some(OWNER),
                    compressed,
                );
                if change == "restored-mtime" {
                    fs::File::options()
                        .write(true)
                        .open(&child)
                        .unwrap()
                        .set_modified(before.modified().unwrap())
                        .unwrap();
                }
            }
            if matches!(change, "same-size" | "restored-mtime" | "replace") {
                assert_eq!(fs::metadata(&child).unwrap().len(), before.len());
            }
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert_eq!(
                index.externally_referenced(OWNER, OWNER),
                change != "append",
                "{change}"
            );
            assert_eq!(
                cache.reads, 3,
                "changed header must be read again: {change}"
            );
        }
    }

    #[test]
    fn cache_hits_do_not_bypass_filesystem_or_header_failures() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for change in [
            "symlink",
            "hardlink",
            "permissions",
            "malformed",
            "pending",
            "root-symlink",
        ] {
            let home = tempfile::tempdir().unwrap();
            let source = write(home.path(), "sessions", OWNER, OWNER, None, false);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::default();
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            match change {
                "symlink" => {
                    let saved = home.path().join("saved");
                    fs::rename(&source, &saved).unwrap();
                    symlink(saved, &source).unwrap();
                }
                "hardlink" => fs::hard_link(&source, home.path().join("extra-link")).unwrap(),
                "permissions" => {
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap()
                }
                "malformed" => fs::write(&source, b"invalid header\n").unwrap(),
                "pending" => {
                    fs::write(home.path().join("sessions/.codex-retain-pending-test"), b"").unwrap()
                }
                "root-symlink" => {
                    let saved = home.path().join("saved-root");
                    fs::rename(home.path().join("sessions"), &saved).unwrap();
                    symlink(saved, home.path().join("sessions")).unwrap();
                }
                _ => unreachable!(),
            }
            let result = scan(home.path(), &candidates, Some(&mut cache));
            if change == "permissions" {
                fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
                // A privileged test runner may still be able to read mode 000.
                if result.is_ok() {
                    assert_eq!(cache.reads, 2);
                    continue;
                }
            }
            assert!(result.is_err(), "{change}");
            assert!(
                cache.entries.is_empty(),
                "failed scans discard cached headers"
            );
        }
    }

    #[test]
    fn owns_reverted_rollouts_without_treating_internal_edges_as_external() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        write(
            home.path(),
            "archived_sessions",
            OWNER,
            OWNER,
            Some(OLD),
            false,
        );
        let candidates = HashSet::from([OWNER]);
        let index = scan(home.path(), &candidates, None).unwrap();
        assert_eq!(index.owned(OWNER).len(), 2);
        assert!(!index.externally_referenced(OLD, OWNER));
        assert_eq!(index.owned(OWNER)[0].rollout_id, OWNER);
        write(home.path(), "sessions", CHILD, CHILD, Some(OLD), false);
        let index = scan(home.path(), &candidates, None).unwrap();
        assert!(index.externally_referenced(OLD, OWNER));
        assert!(!index.externally_referenced(OWNER, OWNER));
    }

    #[test]
    fn retains_same_owner_copies_across_locations_and_encodings() {
        for (directory, compressed) in [("sessions", false), ("archived_sessions", true)] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            write(home.path(), directory, OWNER, OWNER, None, compressed);
            assert_eq!(
                scan(home.path(), &HashSet::from([OWNER]), None)
                    .unwrap()
                    .owned(OWNER)
                    .len(),
                2
            );
        }
    }

    #[test]
    fn rejects_malformed_unrelated_headers() {
        for invalid in [
            "not json\n",
            "{}\n",
            "{\"type\":\"session_meta\",\"payload\":null}\n",
        ] {
            let home = tempfile::tempdir().unwrap();
            let path = write(home.path(), "sessions", CHILD, CHILD, None, false);
            fs::write(path, invalid).unwrap();
            assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        }
    }

    #[test]
    fn validates_modes_reference_fields_and_duplicate_keys() {
        for payload in [
            json!({"id":OWNER,"history_mode":"future"}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":{}}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":[OLD,1,10]}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":{"thread_id":OLD,"end_ordinal_exclusive":-1,"end_byte_offset":10}}),
            json!({"id":OWNER,"history_base":{"thread_id":OLD,"end_ordinal_exclusive":1,"end_byte_offset":10}}),
            json!({"id":OWNER.to_string().replace('-', "")}),
        ] {
            assert!(
                header(
                    json!({"type":"session_meta","payload":payload})
                        .to_string()
                        .as_bytes()
                )
                .is_err()
            );
        }
        let duplicate = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_base":null,"history_base":null}}}}"#
        );
        assert!(header(duplicate.as_bytes()).is_err());
        let legacy = json!({"type":"session_meta","payload":{"id":OWNER}});
        assert_eq!(
            header(legacy.to_string().as_bytes()).unwrap(),
            ParsedHeader {
                owner: OWNER,
                paginated: false,
                base: None,
                parent: None
            }
        );
    }

    #[test]
    fn parses_native_reverted_names_and_checks_the_filename_owner() {
        for suffix in [".jsonl", ".jsonl.zst"] {
            let name = format!("rollout-2026-09-10T00-00-00-{OWNER}_{OLD}{suffix}");
            let parsed = filename(&name).unwrap().unwrap();
            assert_eq!(parsed.owner, OWNER);
            assert_eq!(parsed.rollout, OLD);
            assert_eq!(parsed.compressed, suffix.ends_with(".zst"));
        }
        let home = tempfile::tempdir().unwrap();
        let original = write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        let wrong =
            original.with_file_name(format!("rollout-2026-09-10T00-00-00-{CHILD}_{OLD}.jsonl"));
        fs::rename(original, wrong).unwrap();
        assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        for name in [
            format!("rollout-2026-02-31T00-00-00-{OWNER}.jsonl"),
            format!("rollout-2026-09-10T00-00-00-{OWNER}_{OLD}_{CHILD}.jsonl"),
        ] {
            assert!(filename(&name).is_err());
        }
    }

    #[test]
    fn rejects_symlinks_hardlinks_and_pending_artifacts() {
        for kind in ["symlink", "hardlink", "pending"] {
            let home = tempfile::tempdir().unwrap();
            let original = write(home.path(), "sessions", OWNER, OWNER, None, false);
            match kind {
                "symlink" => {
                    std::os::unix::fs::symlink(&original, home.path().join("sessions/link"))
                        .unwrap()
                }
                "hardlink" => fs::hard_link(&original, home.path().join("elsewhere")).unwrap(),
                _ => {
                    fs::write(home.path().join("sessions/.codex-retain-pending-test"), b"").unwrap()
                }
            }
            assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        }
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(home.path().join("missing"), home.path().join("sessions"))
            .unwrap();
        assert!(scan(home.path(), &HashSet::new(), None).is_err());
    }

    #[test]
    fn organizational_metadata_survives_missing_sql_edges() {
        for source_only in [false, true] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let child = write(home.path(), "sessions", CHILD, CHILD, None, false);
            let parent = if source_only {
                json!({"source":{"subagent":{"thread_spawn":{"parent_thread_id":OWNER,"depth":1}}}})
            } else {
                json!({"parent_thread_id":OWNER})
            };
            let mut payload = json!({"id":CHILD,"history_mode":"paginated"});
            for (key, value) in parent.as_object().unwrap() {
                payload[key] = value.clone();
            }
            fs::write(
                &child,
                format!("{}\n", json!({"type":"session_meta","payload":payload})),
            )
            .unwrap();
            let index = scan(home.path(), &HashSet::from([OWNER]), None).unwrap();
            let mut graph = crate::relations::Dependencies::default();
            index.add_dependencies(&mut graph);
            assert_eq!(
                graph.plan(&HashSet::from([OWNER])).blocked[&OWNER],
                crate::relations::Blocker::Children
            );
        }
    }

    #[test]
    fn contradictory_or_duplicate_parent_metadata_cannot_hide_an_owner() {
        let invalid = json!({"type":"session_meta","payload":{"id":OWNER,"parent_thread_id":CHILD,
            "source":{"subagent":{"thread_spawn":{"parent_thread_id":OLD,"depth":1}}}}});
        assert!(header(invalid.to_string().as_bytes()).is_err());
        let duplicate = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}","parent_thread_id":"{OLD}"}}}}}}}}}}"#
        );
        assert!(header(duplicate.as_bytes()).is_err());
        // Recovery checks identity, not new optional-metadata admission rules.
        validate_owner(invalid.to_string().as_bytes(), OWNER).unwrap();
    }
}
