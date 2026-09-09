//! Bounded reads, durable replacement, and the locks shared with Codex 0.153.4.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}
impl Identity {
    pub fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
        }
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "directory must not be a symlink: {}",
        path.display()
    );
    Ok(())
}

pub fn regular(path: &Path, writable: bool, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "expected a regular file with one link: {}",
        path.display()
    );
    Ok(file)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    regular(path, false, false)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "state file exceeds 1 MiB: {}",
        path.display()
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid state file: {}", path.display()))
}

pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(test)]
    if JOURNAL_CLEAR_SYNC_FAILURE.with(|slot| {
        let mut target = slot.borrow_mut();
        if target.as_deref() == Some(path) && !path.join("pending.json").exists() {
            *target = None;
            true
        } else {
            false
        }
    }) {
        bail!("injected directory sync failure after journal unlink");
    }
    File::open(path)?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

#[cfg(test)]
thread_local! {
    static JOURNAL_CLEAR_SYNC_FAILURE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_journal_clear_sync_once(directory: PathBuf) {
    JOURNAL_CLEAR_SYNC_FAILURE.with(|slot| *slot.borrow_mut() = Some(directory));
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    private_dir(parent)?;
    if path.symlink_metadata().is_ok() {
        regular(path, false, false)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), value)?;
    tmp.write_all(b"\n")?;
    ensure!(
        tmp.as_file().metadata()?.len() <= 1024 * 1024,
        "state would exceed 1 MiB; previous state was preserved"
    );
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync_dir(parent)
}

pub fn remove_durable(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(path.parent().context("no parent")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

pub fn try_lock(path: &Path) -> Result<File> {
    let f = regular(path, true, true)?;
    f.try_lock()
        .map_err(|e| anyhow::anyhow!("busy or unavailable lock {}: {e}", path.display()))?;
    Ok(f)
}

pub const MAX_BATCH_THREADS: usize = 32;

/// Own all requested thread locks or none, with one coordination lock.
///
/// Codex removes stale UUID lock paths while holding coordination. Retain that
/// same lock until every owned file has been closed and its path cleaned up.
pub struct ThreadLocks {
    files: Vec<(PathBuf, File)>,
    _coordination: File,
}
impl ThreadLocks {
    pub fn acquire(home: &Path, ids: &[&str]) -> Result<Self> {
        ensure!(
            !ids.is_empty() && ids.len() <= MAX_BATCH_THREADS,
            "thread lock batch must contain 1..={MAX_BATCH_THREADS} IDs"
        );
        for id in ids {
            ensure!(
                uuid::Uuid::parse_str(id)?.to_string() == *id,
                "noncanonical thread ID"
            );
        }
        let mut ordered = ids.to_vec();
        ordered.sort_unstable();
        ensure!(
            ordered.windows(2).all(|pair| pair[0] != pair[1]),
            "thread lock batch contains duplicate IDs"
        );
        let dir = home.join("thread-writer-locks");
        private_dir(&dir)?;
        let coordination = try_lock(&dir.join(".coordination.lock"))?;
        let mut owned = Self {
            files: Vec::with_capacity(ordered.len()),
            _coordination: coordination,
        };
        for id in ordered {
            let path = dir.join(format!("{id}.lock"));
            let file = try_lock(&path)?;
            // Only successfully locked paths enter cleanup ownership. A busy
            // pathname belongs to another writer and must never be unlinked.
            owned.files.push((path, file));
        }
        Ok(owned)
    }
}
impl Drop for ThreadLocks {
    fn drop(&mut self) {
        // Coordination is still held. A leftover empty lock is safe: Codex
        // removes it during its next coordinated stale-lock cleanup.
        while let Some((path, file)) = self.files.pop() {
            drop(file);
            let _ = fs::remove_file(path);
        }
    }
}

pub struct ThreadLock {
    _locks: ThreadLocks,
}
impl ThreadLock {
    pub fn acquire(home: &Path, id: &str) -> Result<Self> {
        Ok(Self {
            _locks: ThreadLocks::acquire(home, &[id])?,
        })
    }
}

pub fn maintenance(home: &Path) -> Result<File> {
    let dir = home.join(".tmp");
    private_dir(&dir)?;
    try_lock(&dir.join("rollout-maintenance.lock"))
}

pub fn checked_absolute(path: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    ensure!(
        !canonical.as_os_str().as_bytes().contains(&0),
        "invalid path"
    );
    Ok(canonical)
}

pub fn volume_available(path: &Path) -> Option<u64> {
    rustix::fs::statvfs(path)
        .ok()
        .and_then(|s| s.f_bavail.checked_mul(s.f_frsize))
}

pub fn rename_without_overwrite(source: &Path, destination: &Path) -> Result<()> {
    rename_without_overwrite_unsynced(source, destination)?;
    sync_dir(source.parent().context("source parent missing")?)
}

/// Rename under the caller's thread lock without syncing the directory.
/// The caller must sync after its bounded group of moves before committing.
pub fn rename_without_overwrite_unsynced(source: &Path, destination: &Path) -> Result<()> {
    if path_exists(destination)? {
        bail!(
            "recovery destination already exists: {}",
            destination.display()
        );
    }
    // Cooperating Codex clients cannot touch this thread while ThreadLock is held.
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(number: u128) -> String {
        uuid::Uuid::from_u128(number).to_string()
    }

    #[test]
    fn partial_batch_failure_releases_owned_locks_and_preserves_foreign_inode() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("thread-writer-locks");
        private_dir(&directory).unwrap();
        let first = id(1);
        let busy_id = id(2);
        let last = id(3);
        let busy_path = directory.join(format!("{busy_id}.lock"));
        // A native writer retains its UUID lock after releasing coordination.
        let foreign = try_lock(&busy_path).unwrap();
        let foreign_identity = Identity::of(&foreign.metadata().unwrap());

        assert!(ThreadLocks::acquire(home.path(), &[&first, &busy_id, &last]).is_err());
        assert_eq!(
            Identity::of(&fs::metadata(&busy_path).unwrap()),
            foreign_identity,
            "failed acquisition must not unlink another writer's lock"
        );
        assert!(try_lock(&busy_path).is_err(), "foreign lock was released");
        assert!(!directory.join(format!("{first}.lock")).exists());
        assert!(!directory.join(format!("{last}.lock")).exists());
        let independently_retried = ThreadLock::acquire(home.path(), &first).unwrap();
        drop(independently_retried);
        assert!(!directory.join(format!("{first}.lock")).exists());

        drop(foreign);
        let retry = ThreadLocks::acquire(home.path(), &[&first, &busy_id, &last]).unwrap();
        drop(retry);
        for thread_id in [&first, &busy_id, &last] {
            assert!(!directory.join(format!("{thread_id}.lock")).exists());
        }
    }

    #[test]
    fn a_batch_keeps_one_coordination_lock_until_all_thread_locks_close() {
        let home = tempfile::tempdir().unwrap();
        let first = id(4);
        let second = id(5);
        let owned = ThreadLocks::acquire(home.path(), &[&second, &first]).unwrap();
        let directory = home.path().join("thread-writer-locks");
        assert!(try_lock(&directory.join(".coordination.lock")).is_err());
        for thread_id in [&first, &second] {
            assert!(try_lock(&directory.join(format!("{thread_id}.lock"))).is_err());
        }
        drop(owned);
        let coordination = try_lock(&directory.join(".coordination.lock")).unwrap();
        for thread_id in [&first, &second] {
            assert!(!directory.join(format!("{thread_id}.lock")).exists());
        }
        drop(coordination);
    }

    #[test]
    fn invalid_duplicate_and_oversized_batches_fail_before_creating_locks() {
        let home = tempfile::tempdir().unwrap();
        let ids = (1..=MAX_BATCH_THREADS + 1)
            .map(|number| id(number as u128))
            .collect::<Vec<_>>();
        let refs = ids.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(ThreadLocks::acquire(home.path(), &[]).is_err());
        assert!(ThreadLocks::acquire(home.path(), &refs).is_err());
        assert!(ThreadLocks::acquire(home.path(), &[&ids[0], &ids[0]]).is_err());
        assert!(ThreadLocks::acquire(home.path(), &[&ids[0], "not-a-uuid"]).is_err());
        let noncanonical = id(0xabcd).to_uppercase();
        assert!(ThreadLocks::acquire(home.path(), &[&noncanonical]).is_err());
        assert!(!home.path().join("thread-writer-locks").exists());

        let maximum = ThreadLocks::acquire(home.path(), &refs[..MAX_BATCH_THREADS]).unwrap();
        drop(maximum);
        for thread_id in &ids[..MAX_BATCH_THREADS] {
            assert!(
                !home
                    .path()
                    .join("thread-writer-locks")
                    .join(format!("{thread_id}.lock"))
                    .exists()
            );
        }
    }

    #[test]
    fn synced_and_grouped_renames_preserve_existing_or_dangling_destinations() {
        type Rename = fn(&Path, &Path) -> Result<()>;
        for rename in [
            rename_without_overwrite as Rename,
            rename_without_overwrite_unsynced as Rename,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let destination = dir.path().join("destination");
            fs::write(&source, b"source bytes").unwrap();
            fs::write(&destination, b"existing bytes").unwrap();
            assert!(rename(&source, &destination).is_err());
            assert_eq!(fs::read(&source).unwrap(), b"source bytes");
            assert_eq!(fs::read(&destination).unwrap(), b"existing bytes");

            fs::remove_file(&destination).unwrap();
            let missing = dir.path().join("missing-target");
            std::os::unix::fs::symlink(&missing, &destination).unwrap();
            assert!(rename(&source, &destination).is_err());
            assert_eq!(fs::read_link(&destination).unwrap(), missing);
            assert_eq!(fs::read(&source).unwrap(), b"source bytes");

            fs::remove_file(&destination).unwrap();
            rename(&source, &destination).unwrap();
            sync_dir(dir.path()).unwrap();
            assert!(!source.exists());
            assert_eq!(fs::read(&destination).unwrap(), b"source bytes");
        }
    }
}
