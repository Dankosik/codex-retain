//! Bounded reads, durable replacement, and the locks shared with Codex 0.153.4.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{BufWriter, Read, Write},
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
    regular_with_metadata(path, writable, create).map(|(file, _)| file)
}

/// Open and validate the file, returning the metadata from that same check.
/// This is a snapshot of the opened inode, not a later path revalidation.
pub fn regular_with_metadata(
    path: &Path,
    writable: bool,
    create: bool,
) -> Result<(File, Metadata)> {
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
    Ok((file, m))
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
    {
        // JSON serialization emits many small writes. Flush the buffer before
        // checking length or syncing so neither errors nor bytes are hidden
        // in BufWriter::drop while publishing the replacement file.
        let mut writer = BufWriter::with_capacity(64 * 1024, tmp.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
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

pub const MAX_BATCH_THREADS: usize = 128;

/// The scope of a failed acquisition determines whether another group can run.
#[derive(Debug, PartialEq, Eq)]
pub enum LockScope {
    Global,
    Thread(String),
}

#[derive(Debug)]
pub struct LockAcquisitionError {
    pub scope: LockScope,
    source: anyhow::Error,
}

impl std::fmt::Display for LockAcquisitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}

impl std::error::Error for LockAcquisitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Display already forwards the wrapped error's first message. Forward
        // its source as well so chained diagnostics do not repeat that message.
        self.source.source()
    }
}

/// Own all requested thread locks or none, with one coordination lock.
///
/// Codex removes stale UUID lock paths while holding coordination. Match its
/// protocol: coordinate acquisition and cleanup, but retain only the UUID locks
/// while doing filesystem and SQLite work so unrelated writers can start.
pub struct ThreadLocks {
    files: Vec<(PathBuf, File)>,
    coordination_path: PathBuf,
    acquisition_guard: Option<File>,
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
        private_dir(&dir).map_err(|source| LockAcquisitionError {
            scope: LockScope::Global,
            source,
        })?;
        let coordination_path = dir.join(".coordination.lock");
        let coordination = try_lock(&coordination_path).map_err(|source| LockAcquisitionError {
            scope: LockScope::Global,
            source,
        })?;
        let mut owned = Self {
            files: Vec::with_capacity(ordered.len()),
            coordination_path,
            acquisition_guard: Some(coordination),
        };
        for id in ordered {
            let path = dir.join(format!("{id}.lock"));
            let file = try_lock(&path).map_err(|source| LockAcquisitionError {
                scope: LockScope::Thread(id.to_owned()),
                source,
            })?;
            // Only successfully locked paths enter cleanup ownership. A busy
            // pathname belongs to another writer and must never be unlinked.
            owned.files.push((path, file));
        }
        drop(owned.acquisition_guard.take());
        Ok(owned)
    }
}
impl Drop for ThreadLocks {
    fn drop(&mut self) {
        // A partial acquisition already owns coordination. Successful guards
        // reacquire it only for cleanup, as Codex does. If it is busy, close
        // without unlinking: a stale empty file is safe, an inode race is not.
        let coordination = self
            .acquisition_guard
            .take()
            .or_else(|| try_lock(&self.coordination_path).ok());
        while let Some((path, file)) = self.files.pop() {
            drop(file);
            if coordination.is_some() {
                let _ = fs::remove_file(path);
            }
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
    private_dir(&dir).map_err(|source| LockAcquisitionError {
        scope: LockScope::Global,
        source,
    })?;
    try_lock(&dir.join("rollout-maintenance.lock")).map_err(|source| {
        LockAcquisitionError {
            scope: LockScope::Global,
            source,
        }
        .into()
    })
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

    #[test]
    fn regular_metadata_describes_the_opened_inode_after_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        let moved = directory.path().join("moved");
        let original = b"the original file contents";
        fs::write(&path, original).unwrap();

        let (mut file, metadata) = regular_with_metadata(&path, true, true).unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement").unwrap();

        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, original);
        assert_eq!(metadata.len(), original.len() as u64);
        assert_eq!(
            Identity::of(&metadata),
            Identity::of(&file.metadata().unwrap())
        );
        assert_eq!(
            Identity::of(&metadata),
            Identity::of(&fs::metadata(&moved).unwrap())
        );
        assert_ne!(
            Identity::of(&metadata),
            Identity::of(&fs::metadata(&path).unwrap())
        );
    }

    #[test]
    fn regular_metadata_keeps_link_and_file_type_guards() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        let hardlink = directory.path().join("hardlink");
        let symlink = directory.path().join("symlink");
        fs::write(&file, b"preserved").unwrap();
        fs::hard_link(&file, &hardlink).unwrap();
        std::os::unix::fs::symlink(&file, &symlink).unwrap();

        for path in [
            file.as_path(),
            hardlink.as_path(),
            symlink.as_path(),
            directory.path(),
        ] {
            let expected = regular(path, false, false).unwrap_err();
            let actual = regular_with_metadata(path, false, false).unwrap_err();
            assert_eq!(format!("{actual:#}"), format!("{expected:#}"));
        }
        assert_eq!(fs::read(&file).unwrap(), b"preserved");
        assert_eq!(fs::read_link(&symlink).unwrap(), file);
    }

    #[test]
    fn atomic_json_publishes_complete_payload_across_buffer_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let value = serde_json::json!({"message": "x".repeat(96 * 1024), "tail": "complete"});
        atomic_json(&path, &value).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            value
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_json_oversize_and_serialization_failure_preserve_previous_state() {
        struct Fails;
        impl Serialize for Fails {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::{Error, SerializeMap};
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("partial", "must not replace previous state")?;
                Err(S::Error::custom("deliberate serialization failure"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = b"{\"kept\":true}\n";
        fs::write(&path, original).unwrap();
        assert!(atomic_json(&path, &"x".repeat(1024 * 1024 + 128)).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(atomic_json(&path, &Fails).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

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

        let original_error = try_lock(&busy_path).unwrap_err();
        let error = ThreadLocks::acquire(home.path(), &[&first, &busy_id, &last])
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<LockAcquisitionError>().unwrap().scope,
            LockScope::Thread(busy_id.clone())
        );
        assert_eq!(format!("{error:#}"), format!("{original_error:#}"));
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
    fn global_lock_contention_is_classified_without_touching_foreign_inodes() {
        for maintenance_lock in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let directory = home.path().join(if maintenance_lock {
                ".tmp"
            } else {
                "thread-writer-locks"
            });
            private_dir(&directory).unwrap();
            let path = directory.join(if maintenance_lock {
                "rollout-maintenance.lock"
            } else {
                ".coordination.lock"
            });
            let foreign = try_lock(&path).unwrap();
            let identity = Identity::of(&foreign.metadata().unwrap());
            let original_error = try_lock(&path).unwrap_err();
            let thread_id = id(8);
            let error = if maintenance_lock {
                maintenance(home.path()).map(drop)
            } else {
                ThreadLocks::acquire(home.path(), &[&thread_id]).map(drop)
            }
            .unwrap_err();

            assert_eq!(
                error.downcast_ref::<LockAcquisitionError>().unwrap().scope,
                LockScope::Global
            );
            assert_eq!(format!("{error:#}"), format!("{original_error:#}"));
            assert_eq!(Identity::of(&fs::metadata(&path).unwrap()), identity);
            assert!(try_lock(&path).is_err());
            assert!(!directory.join(format!("{thread_id}.lock")).exists());
        }
    }

    #[test]
    fn unavailable_lock_directories_are_global_failures() {
        for maintenance_lock in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join(if maintenance_lock {
                ".tmp"
            } else {
                "thread-writer-locks"
            });
            fs::write(&path, b"not a directory").unwrap();
            let original_error = private_dir(&path).unwrap_err();
            let error = if maintenance_lock {
                maintenance(home.path()).map(drop)
            } else {
                ThreadLocks::acquire(home.path(), &[&id(9)]).map(drop)
            }
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<LockAcquisitionError>().unwrap().scope,
                LockScope::Global
            );
            assert_eq!(format!("{error:#}"), format!("{original_error:#}"));
            assert_eq!(fs::read(&path).unwrap(), b"not a directory");
        }
    }

    #[test]
    fn unavailable_member_lock_preserves_cause_and_classification_through_context() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("thread-writer-locks");
        private_dir(&directory).unwrap();
        let thread_id = id(10);
        let path = directory.join(format!("{thread_id}.lock"));
        let target = home.path().join("foreign-file");
        fs::write(&target, b"foreign contents").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let original_error = try_lock(&path).unwrap_err();
        let error = ThreadLocks::acquire(home.path(), &[&thread_id])
            .err()
            .unwrap();
        assert_eq!(format!("{error:#}"), format!("{original_error:#}"));
        let error = error.context("outer operation");
        assert_eq!(
            error.downcast_ref::<LockAcquisitionError>().unwrap().scope,
            LockScope::Thread(thread_id)
        );
        assert_eq!(fs::read_link(&path).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"foreign contents");
        assert!(try_lock(&directory.join(".coordination.lock")).is_ok());
    }

    #[test]
    fn a_batch_releases_coordination_but_keeps_all_thread_locks() {
        let home = tempfile::tempdir().unwrap();
        let first = id(4);
        let second = id(5);
        let owned = ThreadLocks::acquire(home.path(), &[&second, &first]).unwrap();
        let directory = home.path().join("thread-writer-locks");
        let coordination = try_lock(&directory.join(".coordination.lock")).unwrap();
        for thread_id in [&first, &second] {
            assert!(try_lock(&directory.join(format!("{thread_id}.lock"))).is_err());
        }
        // Unrelated thread acquisition can proceed while the batch is live.
        let unrelated = try_lock(&directory.join(format!("{}.lock", id(6)))).unwrap();
        drop(unrelated);
        fs::remove_file(directory.join(format!("{}.lock", id(6)))).unwrap();
        drop(coordination);
        drop(owned);
        let coordination = try_lock(&directory.join(".coordination.lock")).unwrap();
        for thread_id in [&first, &second] {
            assert!(!directory.join(format!("{thread_id}.lock")).exists());
        }
        drop(coordination);
    }

    #[test]
    fn busy_coordination_during_drop_closes_without_unlinking_a_reusable_inode() {
        let home = tempfile::tempdir().unwrap();
        let thread_id = id(7);
        let owned = ThreadLocks::acquire(home.path(), &[&thread_id]).unwrap();
        let directory = home.path().join("thread-writer-locks");
        let path = directory.join(format!("{thread_id}.lock"));
        let original = Identity::of(&fs::metadata(&path).unwrap());
        let coordinator = try_lock(&directory.join(".coordination.lock")).unwrap();
        let successor = regular(&path, true, false).unwrap();
        assert!(successor.try_lock().is_err());
        drop(owned);
        // A native writer holding coordination can now reuse the same inode.
        successor.try_lock().unwrap();
        assert_eq!(Identity::of(&successor.metadata().unwrap()), original);
        assert_eq!(Identity::of(&fs::metadata(&path).unwrap()), original);
        assert!(try_lock(&path).is_err());
        drop(coordinator);
        assert!(ThreadLocks::acquire(home.path(), &[&thread_id]).is_err());
        assert_eq!(Identity::of(&fs::metadata(&path).unwrap()), original);
        let coordinator = try_lock(&directory.join(".coordination.lock")).unwrap();
        drop(successor);
        fs::remove_file(&path).unwrap();
        drop(coordinator);
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
