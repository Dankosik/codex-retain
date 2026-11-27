//! Fail-closed reader for Codex Desktop pinned-thread state in
//! `$CODEX_HOME/.codex-global-state.json`.
//!
//! Codex Desktop can store pinned thread IDs in `.codex-global-state.json`
//! independently of (or in addition to) SQLite columns. If the file is absent,
//! no Desktop pins are added. If it is present but unreadable, non-regular,
//! oversized, or corrupt JSON, reads fail closed so pinned chats are never
//! deleted on missing Desktop state.

use crate::lineage::FastHashSet;
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{CWD, FileType, Mode, OFlags, Stat, fstat, openat};
use serde_json::Value;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub const GLOBAL_STATE_FILE: &str = ".codex-global-state.json";
const MAX_GLOBAL_STATE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct StateStamp {
    device: u64,
    inode: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
    mode: u32,
}

impl StateStamp {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            len: stat.st_size as u64,
            mtime: stat.st_mtime as i64,
            mtime_nsec: stat.st_mtime_nsec as i64,
            ctime: stat.st_ctime as i64,
            ctime_nsec: stat.st_ctime_nsec as i64,
            mode: stat.st_mode as u32,
        }
    }
}

/// Stamp-validated cache of Codex Desktop pinned thread UUIDs.
pub struct DesktopPins {
    path: PathBuf,
    present: bool,
    stamp: Option<StateStamp>,
    pinned: FastHashSet<String>,
}

impl DesktopPins {
    pub fn load(codex_home: &Path) -> Result<Self> {
        let mut cache = Self {
            path: codex_home.join(GLOBAL_STATE_FILE),
            present: false,
            stamp: None,
            pinned: FastHashSet::default(),
        };
        cache.refresh()?;
        Ok(cache)
    }

    pub fn present(&self) -> bool {
        self.present
    }

    pub fn len(&self) -> usize {
        self.pinned.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pinned.is_empty()
    }

    #[inline]
    pub fn contains(&self, id: &str) -> bool {
        if self.pinned.is_empty() {
            return false;
        }
        if self.pinned.contains(id) {
            return true;
        }
        uuid::Uuid::parse_str(id)
            .ok()
            .is_some_and(|uuid| self.pinned.contains(&uuid.to_string()))
    }

    /// Re-stat `.codex-global-state.json` and re-parse only when the file
    /// appeared, disappeared, or changed since the previous check.
    pub fn refresh(&mut self) -> Result<()> {
        for attempt in 0..2 {
            let fd = match openat(
                CWD,
                &self.path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => {
                    self.present = false;
                    self.stamp = None;
                    self.pinned.clear();
                    return Ok(());
                }
                Err(error) => {
                    return Err(std::io::Error::from(error)).with_context(|| {
                        format!(
                            "cannot inspect Codex Desktop global state {}",
                            self.path.display()
                        )
                    });
                }
            };
            let file = File::from(fd);
            let before = fstat(&file)
                .map_err(std::io::Error::from)
                .with_context(|| {
                    format!(
                        "cannot stat Codex Desktop global state {}",
                        self.path.display()
                    )
                })?;
            ensure!(
                FileType::from_raw_mode(before.st_mode) == FileType::RegularFile,
                "Codex Desktop global state is not a regular file: {}",
                self.path.display()
            );
            let stamp = StateStamp::from_stat(&before);
            ensure!(
                stamp.len <= MAX_GLOBAL_STATE_BYTES,
                "Codex Desktop global state exceeds 16 MiB safety limit: {}",
                self.path.display()
            );
            if self.present && self.stamp == Some(stamp) {
                return Ok(());
            }

            let mut bytes = Vec::with_capacity(stamp.len.min(64 * 1024) as usize);
            (&file)
                .take(MAX_GLOBAL_STATE_BYTES + 1)
                .read_to_end(&mut bytes)
                .with_context(|| {
                    format!(
                        "cannot read Codex Desktop global state {}",
                        self.path.display()
                    )
                })?;
            ensure!(
                bytes.len() as u64 <= MAX_GLOBAL_STATE_BYTES,
                "Codex Desktop global state exceeds 16 MiB safety limit: {}",
                self.path.display()
            );
            let after = fstat(&file)
                .map_err(std::io::Error::from)
                .with_context(|| {
                    format!(
                        "cannot re-stat Codex Desktop global state {}",
                        self.path.display()
                    )
                })?;
            let after_stamp = StateStamp::from_stat(&after);
            if after_stamp != stamp {
                if attempt == 0 {
                    continue;
                }
                bail!(
                    "Codex Desktop global state changed while reading: {}",
                    self.path.display()
                );
            }
            let parsed: Value = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "cannot verify Codex Desktop pins: invalid JSON in {}",
                    self.path.display()
                )
            })?;
            let mut next = FastHashSet::default();
            walk_global_state(&parsed, None, &mut next);
            self.present = true;
            self.stamp = Some(stamp);
            self.pinned = next;
            return Ok(());
        }
        unreachable!("loop returns on attempt 0 or 1")
    }
}

fn is_pin_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.windows(3).any(|w| {
        w[0].eq_ignore_ascii_case(&b'p')
            && w[1].eq_ignore_ascii_case(&b'i')
            && w[2].eq_ignore_ascii_case(&b'n')
    })
}

fn is_id_key(key: &str) -> bool {
    key == "id"
        || key.eq_ignore_ascii_case("threadId")
        || key.eq_ignore_ascii_case("thread_id")
        || key.eq_ignore_ascii_case("thread-id")
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(n) => {
            n.as_i64() != Some(0) && n.as_u64() != Some(0) && n.as_f64() != Some(0.0)
        }
        Value::String(s) => {
            let trimmed = s.trim();
            !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
        }
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn parse_uuid_string(value: &str) -> Option<String> {
    if let Some(uuid) = crate::lineage::parse_canonical_uuid(value) {
        return Some(uuid.to_string());
    }
    uuid::Uuid::parse_str(value).ok().map(|u| u.to_string())
}

fn collect_uuids_under_pin(value: &Value, out: &mut FastHashSet<String>) {
    match value {
        Value::String(text) => {
            if let Some(id) = parse_uuid_string(text) {
                out.insert(id);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_uuids_under_pin(item, out);
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                if let Some(id) = parse_uuid_string(key) {
                    if is_truthy(val) {
                        out.insert(id);
                    }
                    continue;
                }
                if is_id_key(key)
                    && let Some(text) = val.as_str()
                    && let Some(id) = parse_uuid_string(text)
                {
                    out.insert(id);
                }
                if matches!(val, Value::Array(_) | Value::Object(_)) {
                    collect_uuids_under_pin(val, out);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn walk_global_state(value: &Value, parent_uuid: Option<&str>, out: &mut FastHashSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_global_state(item, None, out);
            }
        }
        Value::Object(map) => {
            let obj_uuid = map
                .iter()
                .find_map(|(k, v)| is_id_key(k).then(|| v.as_str().and_then(parse_uuid_string))?)
                .or_else(|| parent_uuid.map(str::to_owned));
            for (key, val) in map {
                if is_pin_key(key) {
                    match val {
                        Value::Array(_) | Value::Object(_) => collect_uuids_under_pin(val, out),
                        _ if is_truthy(val) => {
                            if let Some(text) = val.as_str()
                                && let Some(id) = parse_uuid_string(text)
                            {
                                out.insert(id);
                            }
                            if let Some(ref owner) = obj_uuid {
                                out.insert(owner.clone());
                            }
                        }
                        _ => {}
                    }
                } else if matches!(val, Value::Array(_) | Value::Object(_)) {
                    let key_uuid = parse_uuid_string(key);
                    walk_global_state(val, key_uuid.as_deref(), out);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn id(n: u128) -> String {
        uuid::Uuid::from_u128(n).to_string()
    }

    #[test]
    fn absent_global_state_returns_empty_without_error() {
        let temp = tempfile::tempdir().unwrap();
        let pins = DesktopPins::load(temp.path()).unwrap();
        assert!(!pins.present());
        assert!(pins.is_empty());
        assert!(!pins.contains(&id(1)));
    }

    #[test]
    fn extracts_desktop_pins_across_known_and_future_json_shapes() {
        let temp = tempfile::tempdir().unwrap();
        let p1 = id(1);
        let p2 = id(2);
        let p3 = id(3);
        let p4 = id(4);
        let p5 = id(5);
        let unpinned1 = id(10);
        let unpinned2 = id(11);
        let payload = serde_json::json!({
            "pinned-thread-ids": [p1.to_uppercase()],
            "sidebar": {
                "pinnedThreadIds": [p2],
                "pinnedMap": {
                    p3.clone(): true,
                    unpinned1.clone(): false
                }
            },
            "threads": {
                p4.clone(): { "isPinned": true },
                unpinned2.clone(): { "isPinned": false }
            },
            "entries": [
                { "threadId": p5, "pinnedAt": 1780000000 }
            ],
            "theme": "dark"
        });
        fs::write(
            temp.path().join(GLOBAL_STATE_FILE),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();

        let pins = DesktopPins::load(temp.path()).unwrap();
        assert!(pins.present());
        for pinned in [&p1, &p2, &p3, &p4, &p5] {
            assert!(pins.contains(pinned), "missing Desktop pin {pinned}");
        }
        assert!(!pins.contains(&unpinned1));
        assert!(!pins.contains(&unpinned2));
    }

    #[test]
    fn corrupt_or_symlink_global_state_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(GLOBAL_STATE_FILE);
        fs::write(&path, b"{corrupt\0json").unwrap();
        assert!(DesktopPins::load(temp.path()).is_err());

        fs::remove_file(&path).unwrap();
        let target = temp.path().join("other.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(DesktopPins::load(temp.path()).is_err());
    }

    #[test]
    fn refresh_detects_live_desktop_pin_updates() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(GLOBAL_STATE_FILE);
        let p1 = id(100);
        let mut pins = DesktopPins::load(temp.path()).unwrap();
        assert!(!pins.contains(&p1));

        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"pinned-thread-ids": [p1]})).unwrap(),
        )
        .unwrap();
        pins.refresh().unwrap();
        assert!(pins.present());
        assert!(pins.contains(&p1));

        fs::remove_file(&path).unwrap();
        pins.refresh().unwrap();
        assert!(!pins.present());
        assert!(!pins.contains(&p1));
    }
}
