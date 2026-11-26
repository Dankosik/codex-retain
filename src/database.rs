//! The adapter deliberately accepts one reviewed schema, not a best-effort guess.
use crate::{
    config::Policy,
    fsutil::{self, Identity},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Statement, TransactionBehavior, params,
    types::{Null, ValueRef},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};

pub const CODEX_VERSION: &str = "codex-cli 0.153.4";
pub const DB_NAME: &str = "state_5.sqlite";
pub const PINNED_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";
pub const CAPTURE_SQL: &str = include_str!("../compatibility/capture.sql");
pub const DROP_CAPTURE: &str = "DROP TRIGGER IF EXISTS codex_retain_insert; DROP TRIGGER IF EXISTS codex_retain_update; DROP TRIGGER IF EXISTS codex_retain_delete; DROP TABLE IF EXISTS codex_retain_epochs; DROP TABLE IF EXISTS codex_retain_owner;";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct SchemaObject {
    r#type: String,
    name: String,
    tbl_name: String,
    sql: Option<String>,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct RawMigration {
    version: i64,
    success: i64,
    checksum: String,
}

struct Migration {
    version: i64,
    success: i64,
    checksum_bytes: Vec<u8>,
}

fn decode_hex_bytes(hex: &str) -> Vec<u8> {
    let bytes = hex.as_bytes();
    assert!(bytes.len().is_multiple_of(2), "odd hex checksum length");
    let nibble = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("invalid hex digit in embedded migrations.json"),
    };
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
        .collect()
}

static EXPECTED_SCHEMA: LazyLock<Vec<SchemaObject>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../compatibility/schema.json"))
        .expect("embedded schema.json must be valid")
});

static EXPECTED_MIGRATIONS: LazyLock<Vec<Migration>> = LazyLock::new(|| {
    let raw: Vec<RawMigration> =
        serde_json::from_str(include_str!("../compatibility/migrations.json"))
            .expect("embedded migrations.json must be valid");
    raw.into_iter()
        .map(|m| Migration {
            version: m.version,
            success: m.success,
            checksum_bytes: decode_hex_bytes(&m.checksum),
        })
        .collect()
});

static EXPECTED_CAPTURE_SCHEMA: LazyLock<Vec<SchemaObject>> = LazyLock::new(|| {
    let reference = Connection::open_in_memory().expect("open in-memory SQLite reference");
    // Trigger SQL is compared as stored by SQLite, avoiding whitespace/normalization guesses.
    reference
        .execute_batch("CREATE TABLE threads(id TEXT,archived INTEGER,archived_at INTEGER);")
        .expect("create reference threads table");
    reference
        .execute_batch(CAPTURE_SQL)
        .expect("create reference capture schema");
    schema(&reference, true).expect("read reference capture schema")
});

pub fn verify_binary(binary: &Path) -> Result<()> {
    // Even `codex --version` installs temporary arg0 helpers. Isolate these
    // startup effects from every real profile and its configuration.
    let environment = tempfile::tempdir()?;
    let codex_home = environment.path().join("codex");
    fs::create_dir(&codex_home)?;
    let output = tempfile::tempfile()?;
    let mut child = Command::new(binary)
        .arg("--version")
        .env("HOME", environment.path())
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()
        .context("start Codex version check")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut poll = Duration::from_micros(500);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Codex version check timed out");
        }
        std::thread::sleep(poll);
        if poll < Duration::from_millis(5) {
            poll = (poll * 2).min(Duration::from_millis(5));
        }
    };
    ensure!(status.success(), "Codex version check failed");
    use std::io::{Read, Seek};
    let mut output = output;
    output.rewind()?;
    let mut version = String::new();
    output.take(1025).read_to_string(&mut version)?;
    ensure!(
        version.trim() == CODEX_VERSION,
        "unsupported Codex version; this adapter requires {CODEX_VERSION}"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseAccess {
    ReadOnly,
    ReadWrite,
}

pub fn open(home: &Path, access: DatabaseAccess) -> Result<Connection> {
    let path = home.join(DB_NAME);
    let _ = fsutil::regular(&path, false, false)?;
    for entry in fs::read_dir(home)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        ensure!(
            !(name.starts_with("state_") && name.ends_with(".sqlite") && name != DB_NAME),
            "another Codex state schema is present; adapter update required"
        );
    }
    let flags = match access {
        DatabaseAccess::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        DatabaseAccess::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
    };
    let c = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(Duration::from_millis(200))?;
    c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")?;
    match access {
        DatabaseAccess::ReadOnly => {
            c.execute_batch("PRAGMA cache_size=-256;")?;
        }
        DatabaseAccess::ReadWrite => {
            c.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-512;")?;
        }
    }
    verify_base(&c)?;
    Ok(c)
}

pub(crate) fn shrink_memory(c: &Connection) {
    let _ = c.execute_batch("PRAGMA shrink_memory;");
}

fn schema(c: &Connection, owned: bool) -> Result<Vec<SchemaObject>> {
    let mut s=c.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")?;
    let rows = s.query_map([], |r| {
        Ok(SchemaObject {
            r#type: r.get(0)?,
            name: r.get(1)?,
            tbl_name: r.get(2)?,
            sql: r.get(3)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        let row = row?;
        if row.name.starts_with("codex_retain_") == owned {
            result.push(row);
        }
    }
    Ok(result)
}

const SCHEMA_SQL: &str = "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name";
const MIGRATIONS_SQL: &str =
    "SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version";
const BACKFILL_SQL: &str = "SELECT status FROM backfill_state WHERE id=1";
const OWNER_SQL: &str = "SELECT owner FROM codex_retain_owner WHERE singleton=1";
const BATCH_THREAD_LOOKUP_SQL: &str = "SELECT t.id,t.rollout_path,t.archived,t.archived_at,\
        CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,\
        t.history_mode,e.archived_since,e.codex_archived_at,'' \
 FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id \
 WHERE t.id=?2";
const BATCH_DELETE_SQL: &str = "DELETE FROM threads WHERE id=? AND archived=1 AND archived_at IS ? AND rollout_path=? AND is_pinned=0 AND history_mode=? AND history_mode IN ('legacy','paginated') AND thread_section_id IS NOT ?";
const SPAWN_SELECT_SQL: &str =
    "SELECT parent_thread_id,child_thread_id FROM thread_spawn_edges LIMIT 500001";
const SPAWN_DELETE_SQL: &str =
    "DELETE FROM thread_spawn_edges WHERE parent_thread_id=? OR child_thread_id=?";

fn schema_matches(c: &Connection, owned: bool, expected: &[SchemaObject]) -> Result<bool> {
    let mut s = c.prepare(SCHEMA_SQL)?;
    let mut rows = s.query([])?;
    let mut index = 0usize;
    while let Some(r) = rows.next()? {
        let name = r.get_ref(1)?.as_str()?;
        if name.starts_with("codex_retain_") != owned {
            continue;
        }
        let Some(exp) = expected.get(index) else {
            return Ok(false);
        };
        let kind = r.get_ref(0)?.as_str()?;
        let tbl_name = r.get_ref(2)?.as_str()?;
        let sql = r.get_ref(3)?.as_str_or_null()?;
        if kind != exp.r#type
            || name != exp.name
            || tbl_name != exp.tbl_name
            || sql != exp.sql.as_deref()
        {
            return Ok(false);
        }
        index += 1;
    }
    Ok(index == expected.len())
}

fn verify_migrations_and_backfill_with(
    migrations_stmt: &mut Statement<'_>,
    backfill_stmt: &mut Statement<'_>,
) -> Result<()> {
    let expected: &[Migration] = &EXPECTED_MIGRATIONS;
    let mut rows = migrations_stmt.query([])?;
    let mut index = 0usize;
    let mut matched = true;
    while let Some(r) = rows.next()? {
        let Some(exp) = expected.get(index) else {
            matched = false;
            break;
        };
        let version: i64 = r.get(0)?;
        let success: i64 = r.get(1)?;
        let checksum_ok = match r.get_ref(2)? {
            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => bytes == exp.checksum_bytes.as_slice(),
            _ => false,
        };
        if version != exp.version || success != exp.success || !checksum_ok {
            matched = false;
            break;
        }
        index += 1;
    }
    ensure!(
        matched && index == expected.len(),
        "Codex migration history differs from the reviewed version"
    );
    let completed = backfill_stmt
        .query_row([], |r| Ok(r.get_ref(0)?.as_str() == Ok("complete")))
        .optional()?;
    ensure!(
        completed == Some(true),
        "Codex index backfill is incomplete; wait for Codex to finish indexing"
    );
    Ok(())
}

fn verify_migrations_and_backfill(c: &Connection) -> Result<()> {
    let mut stmt = c.prepare(MIGRATIONS_SQL)?;
    let mut backfill = c.prepare(BACKFILL_SQL)?;
    verify_migrations_and_backfill_with(&mut stmt, &mut backfill)
}

pub fn verify_base(c: &Connection) -> Result<()> {
    ensure!(
        schema_matches(c, false, &EXPECTED_SCHEMA)?,
        "unsupported or modified Codex schema; nothing will be deleted"
    );
    verify_migrations_and_backfill(c)
}

pub fn identity(home: &Path) -> Result<Identity> {
    let (_file, metadata) = fsutil::regular_with_metadata(&home.join(DB_NAME), false, false)?;
    Ok(Identity::of(&metadata))
}
pub fn verify_policy(c: &Connection, p: &Policy) -> Result<()> {
    ensure!(
        identity(&p.codex_home)? == p.database,
        "Codex database was replaced; disable and explicitly enable the new profile after reviewing its archive dates"
    );
    ensure!(
        schema_matches(c, true, &EXPECTED_CAPTURE_SCHEMA)?,
        "retention capture is missing or modified; deletion is disabled until explicit re-enable"
    );
    let mut owner_stmt = c.prepare(OWNER_SQL)?;
    let matches_owner = owner_stmt.query_row([], |r| Ok(r.get_ref(0)?.as_str()? == p.owner))?;
    ensure!(
        matches_owner,
        "this Codex profile belongs to another retention policy"
    );
    Ok(())
}

/// Reusable SQLite prepared statements bound to one cleanup connection.
/// Reuses SQL compilation only; every guard and query still executes live
/// inside each batch's `BEGIN IMMEDIATE` transaction.
pub(crate) struct BatchStatements<'connection> {
    schema: Statement<'connection>,
    migrations: Statement<'connection>,
    backfill: Statement<'connection>,
    owner: Statement<'connection>,
    thread_lookup: Statement<'connection>,
    spawn_select: Statement<'connection>,
    batch_delete: Statement<'connection>,
    spawn_delete: Statement<'connection>,
}

fn check_schema_statement(stmt: &mut Statement<'_>) -> Result<(bool, bool)> {
    let expected_base: &[SchemaObject] = &EXPECTED_SCHEMA;
    let expected_capture: &[SchemaObject] = &EXPECTED_CAPTURE_SCHEMA;
    let mut rows = stmt.query([])?;
    let mut base_idx = 0usize;
    let mut base_matched = true;
    let mut capture_idx = 0usize;
    let mut capture_matched = true;
    while let Some(r) = rows.next()? {
        let name = r.get_ref(1)?.as_str()?;
        let owned = name.starts_with("codex_retain_");
        let (expected, idx, matched) = if owned {
            (expected_capture, &mut capture_idx, &mut capture_matched)
        } else {
            (expected_base, &mut base_idx, &mut base_matched)
        };
        if !*matched {
            continue;
        }
        let Some(exp) = expected.get(*idx) else {
            *matched = false;
            continue;
        };
        let kind = r.get_ref(0)?.as_str()?;
        let tbl_name = r.get_ref(2)?.as_str()?;
        let sql = r.get_ref(3)?.as_str_or_null()?;
        if kind != exp.r#type
            || name != exp.name
            || tbl_name != exp.tbl_name
            || sql != exp.sql.as_deref()
        {
            *matched = false;
            continue;
        }
        *idx += 1;
    }
    Ok((
        base_matched && base_idx == expected_base.len(),
        capture_matched && capture_idx == expected_capture.len(),
    ))
}

impl<'connection> BatchStatements<'connection> {
    /// Verify base schema, migrations, backfill, database identity, capture
    /// schema, and policy owner while compiling all batch statements once per
    /// connection, preserving exact error precedence when any table is missing
    /// or modified.
    pub(crate) fn prepare_and_verify(c: &'connection Connection, p: &Policy) -> Result<Self> {
        let mut schema = c.prepare(SCHEMA_SQL)?;
        let (base_ok, capture_ok) = check_schema_statement(&mut schema)?;
        ensure!(
            base_ok,
            "unsupported or modified Codex schema; nothing will be deleted"
        );
        let mut migrations = c.prepare(MIGRATIONS_SQL)?;
        let mut backfill = c.prepare(BACKFILL_SQL)?;
        verify_migrations_and_backfill_with(&mut migrations, &mut backfill)?;
        ensure!(
            identity(&p.codex_home)? == p.database,
            "Codex database was replaced; disable and explicitly enable the new profile after reviewing its archive dates"
        );
        ensure!(
            capture_ok,
            "retention capture is missing or modified; deletion is disabled until explicit re-enable"
        );
        let mut owner = c.prepare(OWNER_SQL)?;
        let matches_owner = owner.query_row([], |r| Ok(r.get_ref(0)?.as_str()? == p.owner))?;
        ensure!(
            matches_owner,
            "this Codex profile belongs to another retention policy"
        );
        Ok(Self {
            schema,
            migrations,
            backfill,
            owner,
            thread_lookup: c.prepare(BATCH_THREAD_LOOKUP_SQL)?,
            spawn_select: c.prepare(SPAWN_SELECT_SQL)?,
            batch_delete: c.prepare(BATCH_DELETE_SQL)?,
            spawn_delete: c.prepare(SPAWN_DELETE_SQL)?,
        })
    }

    /// Re-verify base schema, migrations, backfill, database identity, capture
    /// schema, and policy owner in a single `sqlite_schema` scan using already
    /// compiled statements, preserving exact error precedence.
    pub(crate) fn verify_base_and_policy(&mut self, p: &Policy) -> Result<()> {
        let (base_ok, capture_ok) = check_schema_statement(&mut self.schema)?;
        ensure!(
            base_ok,
            "unsupported or modified Codex schema; nothing will be deleted"
        );
        verify_migrations_and_backfill_with(&mut self.migrations, &mut self.backfill)?;
        ensure!(
            identity(&p.codex_home)? == p.database,
            "Codex database was replaced; disable and explicitly enable the new profile after reviewing its archive dates"
        );
        ensure!(
            capture_ok,
            "retention capture is missing or modified; deletion is disabled until explicit re-enable"
        );
        let matches_owner = self
            .owner
            .query_row([], |r| Ok(r.get_ref(0)?.as_str()? == p.owner))?;
        ensure!(
            matches_owner,
            "this Codex profile belongs to another retention policy"
        );
        Ok(())
    }

    pub(crate) fn lookup_batch_thread(
        &mut self,
        id: &str,
        mut on_row: impl FnMut(BorrowedThread<'_>) -> Result<BatchThread>,
    ) -> Result<Option<BatchThread>> {
        let mut rows = self.thread_lookup.query([PINNED_SECTION_ID, id])?;
        let Some(r) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(on_row(borrowed_row(r)?)?))
    }

    pub(crate) fn add_spawn_dependencies(
        &mut self,
        graph: &mut crate::relations::Dependencies,
    ) -> Result<bool> {
        let mut rows = self.spawn_select.query([])?;
        let mut index = 0usize;
        while let Some(row) = rows.next()? {
            ensure!(
                index < 500000,
                "spawn graph exceeds the 500,000-edge safety limit"
            );
            index += 1;
            let parent = row.get_ref(0)?.as_str()?;
            let child = row.get_ref(1)?.as_str()?;
            let parent_id = crate::lineage::parse_canonical_uuid(parent)
                .context("noncanonical spawn relationship identity")?;
            let child_id = crate::lineage::parse_canonical_uuid(child)
                .context("noncanonical spawn relationship identity")?;
            graph.add_parent(child_id, parent_id);
        }
        Ok(index > 0)
    }

    pub(crate) fn delete_batch_rows(&mut self, threads: &[BatchThread]) -> Result<()> {
        for thread in threads {
            let affected = self.batch_delete.execute(params![
                thread.id,
                thread.archived_at,
                thread
                    .path
                    .to_str()
                    .context("non-UTF-8 Codex database path")?,
                thread.history_mode,
                PINNED_SECTION_ID
            ])?;
            ensure!(affected == 1, "thread changed before deletion");
        }
        Ok(())
    }

    pub(crate) fn delete_spawn_edges(&mut self, threads: &[BatchThread]) -> Result<()> {
        for thread in threads {
            self.spawn_delete.execute([&thread.id, &thread.id])?;
        }
        Ok(())
    }
}

pub fn install_capture(c: &mut Connection, owner: &str) -> Result<u64> {
    // Allow a larger page cache for the bulk epoch-seeding insert, then release
    // it before the follow-up cleanup scan allocates report/cache state.
    let _ = c.execute_batch("PRAGMA cache_size=-2000;");
    let result = (|| -> Result<u64> {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        verify_base(&tx)?;
        let owns: Option<String> = if schema(&tx, true)?.is_empty() {
            None
        } else {
            Some(
                tx.query_row(
                    "SELECT owner FROM codex_retain_owner WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )
                .context("unrecognized existing retention extension")?,
            )
        };
        ensure!(
            owns.as_deref().is_none_or(|v| v == owner),
            "another retention policy is already attached to this Codex home"
        );
        tx.execute_batch(DROP_CAPTURE)?;
        tx.execute_batch(CAPTURE_SQL)?;
        tx.execute(
            "INSERT INTO codex_retain_owner(singleton,owner) VALUES(1,?)",
            [owner],
        )?;
        // Adopt Codex's recorded archive age only at explicit activation. Missing
        // dates remain ineligible; do not infer them from creation or file mtime.
        // Subsequent native transitions/repairs still use the live capture triggers.
        let count=tx.execute("INSERT INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) SELECT id,coalesce(archived_at,0),archived_at FROM threads WHERE archived=1",[])?;
        tx.commit()?;
        Ok(count as u64)
    })();
    let _ = c.execute_batch("PRAGMA cache_size=-512; PRAGMA shrink_memory;");
    result
}

pub fn uninstall_capture(c: &mut Connection, p: &Policy) -> Result<()> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !schema(&tx, true)?.is_empty() {
        verify_policy(&tx, p)?;
        tx.execute_batch(DROP_CAPTURE)?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub title: String,
    pub path: PathBuf,
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub pinned: i64,
    pub history_mode: String,
    pub epoch: Option<i64>,
    pub recorded_archive: Option<i64>,
    pub related: bool,
}
const SELECT: &str = "SELECT t.id,t.rollout_path,t.archived,t.archived_at,CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ? THEN 0 ELSE 1 END,t.history_mode,e.archived_since,e.codex_archived_at,EXISTS(SELECT 1 FROM thread_spawn_edges s WHERE s.parent_thread_id=t.id OR s.child_thread_id=t.id),substr(coalesce(nullif(t.name,''),t.title),1,160) FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?,
        title: r.get(9)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        archived: r.get(2)?,
        archived_at: r.get(3)?,
        pinned: r.get(4)?,
        history_mode: r.get(5)?,
        epoch: r.get(6)?,
        recorded_archive: r.get(7)?,
        related: r.get(8)?,
    })
}

pub fn archived(c: &Connection) -> Result<Vec<Thread>> {
    let mut threads = Vec::new();
    visit_archived(c, |thread| {
        threads.push(thread);
        Ok(())
    })?;
    Ok(threads)
}

/// Visit the initial archive snapshot without retaining every owned row.
/// The caller must finish this bounded read before beginning deletion.
pub fn visit_archived(c: &Connection, mut visit: impl FnMut(Thread) -> Result<()>) -> Result<()> {
    let mut s = c.prepare(&format!(
        "{SELECT} WHERE t.archived<>0 OR t.archived IS NULL ORDER BY t.id LIMIT 100001"
    ))?;
    for (index, thread) in s.query_map([PINNED_SECTION_ID], row)?.enumerate() {
        ensure!(
            index < 100000,
            "archive exceeds the 100,000-thread safety limit; no changes made"
        );
        visit(thread?)?;
    }
    Ok(())
}
pub fn prepare_thread_lookup(c: &Connection) -> Result<Statement<'_>> {
    Ok(c.prepare(&format!("{SELECT} WHERE t.id=?"))?)
}
pub fn lookup_thread(statement: &mut Statement<'_>, id: &str) -> Result<Option<Thread>> {
    Ok(statement
        .query_row([PINNED_SECTION_ID, id], row)
        .optional()?)
}
pub fn prepare_delete(c: &Connection) -> Result<Statement<'_>> {
    Ok(c.prepare("DELETE FROM threads WHERE id=? AND archived=1 AND archived_at IS ? AND rollout_path=? AND is_pinned=0 AND history_mode=? AND history_mode IN ('legacy','paginated') AND thread_section_id IS NOT ?")?)
}
pub fn delete_row(statement: &mut Statement<'_>, t: &Thread) -> Result<()> {
    let affected = statement.execute(params![
        t.id,
        t.archived_at,
        t.path.to_str().context("non-UTF-8 Codex database path")?,
        t.history_mode,
        PINNED_SECTION_ID
    ])?;
    ensure!(affected == 1, "thread changed before deletion");
    Ok(())
}

/// Consume a statement prepared before journaling, releasing its transaction
/// borrow before the caller commits the staged group.
pub fn delete_rows(mut statement: Statement<'_>, threads: &[Thread]) -> Result<()> {
    for thread in threads {
        delete_row(&mut statement, thread)?;
    }
    Ok(())
}

pub(crate) struct BorrowedThread<'row> {
    pub id: &'row str,
    pub title: &'row str,
    pub path: &'row str,
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub pinned: i64,
    pub history_mode: &'row str,
    pub epoch: Option<i64>,
    pub recorded_archive: Option<i64>,
}

pub(crate) struct BatchThread {
    pub id: String,
    pub path: PathBuf,
    pub archived_at: Option<i64>,
    pub history_mode: &'static str,
}

const VISIT_ARCHIVED_FULL_SQL: &str = "SELECT t.id,t.rollout_path,t.archived,t.archived_at,CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,t.history_mode,e.archived_since,e.codex_archived_at,substr(coalesce(nullif(t.name,''),t.title),1,160) FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id WHERE t.archived > 0 \
UNION ALL SELECT t.id,t.rollout_path,t.archived,t.archived_at,CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,t.history_mode,e.archived_since,e.codex_archived_at,substr(coalesce(nullif(t.name,''),t.title),1,160) FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id WHERE t.archived < 0 \
UNION ALL SELECT t.id,t.rollout_path,t.archived,t.archived_at,CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,t.history_mode,e.archived_since,e.codex_archived_at,substr(coalesce(nullif(t.name,''),t.title),1,160) FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id WHERE t.archived IS ?2 \
LIMIT 100001";

const COUNT_ARCHIVED_SQL: &str = "SELECT count(*), max(is_null) FROM (\
    SELECT 0 AS is_null FROM threads WHERE archived > 0 \
    UNION ALL SELECT 0 AS is_null FROM threads WHERE archived < 0 \
    UNION ALL SELECT 1 AS is_null FROM threads WHERE archived IS ?1 \
    LIMIT 100001\
 )";

const VISIT_ARCHIVED_DUE_SQL: &str = "SELECT t.id,t.rollout_path,t.archived,t.archived_at,\
        CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,\
        t.history_mode,e.archived_since,e.codex_archived_at,\
        substr(coalesce(nullif(t.name,''),t.title),1,160) \
 FROM codex_retain_epochs e \
 CROSS JOIN threads t ON t.id = e.thread_id \
 WHERE e.archived_since > 0 AND e.archived_since < ?2 AND t.archived = 1";

const VISIT_ARCHIVED_TOP_SQL: &str = "SELECT t.id,t.rollout_path,t.archived,t.archived_at,\
        CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,\
        t.history_mode,e.archived_since,e.codex_archived_at,\
        substr(coalesce(nullif(t.name,''),t.title),1,160) \
 FROM threads t LEFT JOIN codex_retain_epochs e ON e.thread_id=t.id \
 WHERE t.archived <> 0 \
 ORDER BY t.id LIMIT ?2";

const COUNT_ACTIVE_BOUNDED_SQL: &str =
    "SELECT count(*) FROM (SELECT 1 FROM threads WHERE archived = 0 LIMIT ?1)";

const COUNT_DUE_EPOCHS_SQL: &str =
    "SELECT count(*) FROM codex_retain_epochs WHERE archived_since > 0 AND archived_since < ?1";

fn borrowed_row<'row>(r: &'row rusqlite::Row<'_>) -> rusqlite::Result<BorrowedThread<'row>> {
    Ok(BorrowedThread {
        id: r.get_ref(0)?.as_str()?,
        path: r.get_ref(1)?.as_str()?,
        archived: r.get(2)?,
        archived_at: r.get(3)?,
        pinned: r.get(4)?,
        history_mode: r.get_ref(5)?.as_str()?,
        epoch: r.get(6)?,
        recorded_archive: r.get(7)?,
        title: r.get_ref(8)?.as_str()?,
    })
}

fn count_archived_in_tx(tx: &Connection) -> Result<usize> {
    let mut count_stmt = tx.prepare(COUNT_ARCHIVED_SQL)?;
    let (examined, has_null): (i64, Option<i64>) =
        count_stmt.query_row(params![Null], |r| Ok((r.get(0)?, r.get(1)?)))?;
    ensure!(
        examined <= 100000,
        "archive exceeds the 100,000-thread safety limit; no changes made"
    );
    ensure!(
        has_null != Some(1),
        "archived status is NULL; cannot safely evaluate retention"
    );
    Ok(examined as usize)
}

/// Fast full-report archive scan: uses `idx_threads_archived` (skipping active
/// rows and scanning archived rows sequentially in rowid order), omits the
/// unused spawn-edge subquery, and borrows strings from SQLite without
/// intermediate `Thread` allocations. Callers sort entries by `id` in Rust.
pub(crate) fn visit_archived_full(
    c: &Connection,
    mut visit: impl FnMut(BorrowedThread<'_>) -> Result<()>,
) -> Result<()> {
    let mut s = c.prepare(VISIT_ARCHIVED_FULL_SQL)?;
    let mut rows = s.query(params![PINNED_SECTION_ID, Null])?;
    let mut index = 0usize;
    while let Some(r) = rows.next()? {
        ensure!(
            index < 100000,
            "archive exceeds the 100,000-thread safety limit; no changes made"
        );
        index += 1;
        visit(borrowed_row(r)?)?;
    }
    Ok(())
}

pub(crate) enum FullVisit<'row> {
    Count(usize),
    Row(BorrowedThread<'row>),
}

/// Consistent full-report archive scan that counts archived rows via the
/// covering `idx_threads_archived` index before visiting, allowing callers to
/// pre-size buffers with exact capacity and zero geometric reallocations.
pub(crate) fn visit_archived_full_with_count(
    c: &mut Connection,
    mut visit: impl FnMut(FullVisit<'_>) -> Result<()>,
) -> Result<()> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    visit(FullVisit::Count(examined))?;
    if examined > 0 {
        visit_archived_full(&tx, |row| visit(FullVisit::Row(row)))?;
    }
    drop(tx);
    Ok(())
}

pub(crate) struct BorrowedThreadCore<'row> {
    pub id: &'row str,
    pub title: &'row str,
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub pinned: i64,
    pub history_mode: &'row str,
}

pub(crate) struct BorrowedEpoch<'row> {
    pub thread_id: &'row str,
    pub archived_since: Option<i64>,
    pub codex_archived_at: Option<i64>,
}

pub(crate) enum CompactVisit<'row> {
    Count(usize),
    Thread(BorrowedThreadCore<'row>),
    ThreadsDone,
    Epoch(BorrowedEpoch<'row>),
}

const VISIT_ARCHIVED_THREADS_CORE_SQL: &str = "SELECT t.id,t.archived,t.archived_at,\
    CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,\
    t.history_mode,t.name,t.title FROM threads t WHERE t.archived > 0 \
    UNION ALL SELECT t.id,t.archived,t.archived_at,\
    CASE WHEN t.is_pinned=0 AND t.thread_section_id IS NOT ?1 THEN 0 ELSE 1 END,\
    t.history_mode,t.name,t.title FROM threads t WHERE t.archived < 0 \
    LIMIT 100001";

const VISIT_EPOCHS_SQL: &str =
    "SELECT thread_id,archived_since,codex_archived_at FROM codex_retain_epochs";

const LOOKUP_ELIGIBLE_PATH_SQL: &str = "SELECT rollout_path,history_mode FROM threads WHERE id=?1";
const VISIT_ELIGIBLE_PATHS_SEQ_SQL: &str =
    "SELECT id,rollout_path,history_mode FROM threads WHERE archived=1";

/// Match SQLite's `substr(text, 1, 160)` exactly: at most 160 Unicode code
/// points, and truncated at the first embedded NUL byte (`0x00`).
#[inline]
fn truncate_title_160(raw: &str) -> &str {
    let prefix = if raw.len() <= 160 {
        raw
    } else {
        match raw.char_indices().nth(160) {
            Some((byte_idx, _)) => &raw[..byte_idx],
            None => raw,
        }
    };
    match memchr::memchr(0, prefix.as_bytes()) {
        Some(nul) => &prefix[..nul],
        None => prefix,
    }
}

/// Decoupled two-table sequential archive scan in one consistent read snapshot:
/// avoids per-row SQLite `LEFT JOIN codex_retain_epochs` B-tree lookups, SQL
/// `substr()` overhead, and unused `rollout_path` reads on non-eligible rows.
/// Eligible rollout paths are resolved inside the same read transaction.
pub(crate) fn visit_archived_compact<S>(
    c: &mut Connection,
    state: &mut S,
    mut visit: impl FnMut(&mut S, CompactVisit<'_>) -> Result<()>,
    mut resolve_eligible: impl FnMut(&mut S, &Connection) -> Result<()>,
) -> Result<()> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    visit(state, CompactVisit::Count(examined))?;
    if examined > 0 {
        {
            let mut stmt = tx.prepare(VISIT_ARCHIVED_THREADS_CORE_SQL)?;
            let mut rows = stmt.query([PINNED_SECTION_ID])?;
            let mut index = 0usize;
            while let Some(r) = rows.next()? {
                ensure!(
                    index < 100000,
                    "archive exceeds the 100,000-thread safety limit; no changes made"
                );
                index += 1;
                let name = r.get_ref(5)?.as_str_or_null()?;
                let raw_title = match name {
                    Some(n) if !n.is_empty() => n,
                    _ => r.get_ref(6)?.as_str()?,
                };
                visit(
                    state,
                    CompactVisit::Thread(BorrowedThreadCore {
                        id: r.get_ref(0)?.as_str()?,
                        archived: r.get(1)?,
                        archived_at: r.get(2)?,
                        pinned: r.get(3)?,
                        history_mode: r.get_ref(4)?.as_str()?,
                        title: truncate_title_160(raw_title),
                    }),
                )?;
            }
        }
        visit(state, CompactVisit::ThreadsDone)?;
        {
            let mut stmt = tx.prepare(VISIT_EPOCHS_SQL)?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                visit(
                    state,
                    CompactVisit::Epoch(BorrowedEpoch {
                        thread_id: r.get_ref(0)?.as_str()?,
                        archived_since: r.get(1)?,
                        codex_archived_at: r.get(2)?,
                    }),
                )?;
            }
        }
        resolve_eligible(state, &tx)?;
    }
    drop(tx);
    Ok(())
}

pub(crate) fn prepare_eligible_path_lookup(tx: &Connection) -> Result<Statement<'_>> {
    Ok(tx.prepare(LOOKUP_ELIGIBLE_PATH_SQL)?)
}

pub(crate) fn lookup_eligible_path(
    stmt: &mut Statement<'_>,
    id: &str,
    mut on_found: impl FnMut(&str, bool) -> Result<()>,
) -> Result<()> {
    let mut rows = stmt.query([id])?;
    if let Some(r) = rows.next()? {
        let path = r.get_ref(0)?.as_str()?;
        let paginated = r.get_ref(1)?.as_str()? == "paginated";
        on_found(path, paginated)?;
    }
    Ok(())
}

pub(crate) fn visit_eligible_paths_sequential(
    tx: &Connection,
    mut on_row: impl FnMut(&str, &str, bool) -> Result<()>,
) -> Result<()> {
    let mut stmt = tx.prepare(VISIT_ELIGIBLE_PATHS_SEQ_SQL)?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id = r.get_ref(0)?.as_str()?;
        let path = r.get_ref(1)?.as_str()?;
        let paginated = r.get_ref(2)?.as_str()? == "paginated";
        on_row(id, path, paginated)?;
    }
    Ok(())
}

pub(crate) enum TextVisit<'row> {
    DueHint(usize),
    Due(BorrowedThread<'row>),
    Prefix(BorrowedThread<'row>),
    Full(BorrowedThread<'row>),
}

/// Fast text-mode archive scan in one consistent read snapshot:
/// for large archives (`examined >= 4096`) where active rows do not outnumber
/// archived rows, scans only due candidates plus the lexicographically smallest
/// `limit` archived rows via the `id` primary key index; otherwise scans
/// `idx_threads_archived` directly to avoid walking large active populations.
pub(crate) fn visit_archived_text(
    c: &mut Connection,
    now: i64,
    duration: i64,
    limit: usize,
    mut visit: impl FnMut(TextVisit<'_>) -> Result<()>,
) -> Result<Option<u64>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    if examined == 0 {
        return Ok(Some(0));
    }
    let use_top_fast_path = if examined >= 4096 {
        let mut active_stmt = tx.prepare(COUNT_ACTIVE_BOUNDED_SQL)?;
        let active_bounded: i64 = active_stmt.query_row([examined as i64], |r| r.get(0))?;
        (active_bounded as usize) < examined
    } else {
        false
    };
    if use_top_fast_path {
        if let Some(cutoff) = now.checked_sub(duration)
            && cutoff > 1
        {
            let due_count: i64 = tx.query_row(COUNT_DUE_EPOCHS_SQL, [cutoff], |r| r.get(0))?;
            let due_hint = usize::try_from(due_count.max(0))
                .unwrap_or(examined)
                .min(examined);
            if due_hint > 0 {
                visit(TextVisit::DueHint(due_hint))?;
                let mut stmt = tx.prepare(VISIT_ARCHIVED_DUE_SQL)?;
                let mut rows = stmt.query(params![PINNED_SECTION_ID, cutoff])?;
                while let Some(r) = rows.next()? {
                    visit(TextVisit::Due(borrowed_row(r)?))?;
                }
            }
        }
        {
            let mut top = tx.prepare(VISIT_ARCHIVED_TOP_SQL)?;
            let mut rows = top.query(params![PINNED_SECTION_ID, limit as i64])?;
            while let Some(r) = rows.next()? {
                visit(TextVisit::Prefix(borrowed_row(r)?))?;
            }
        }
        drop(tx);
        Ok(Some(examined as u64))
    } else {
        visit_archived_full(&tx, |row| visit(TextVisit::Full(row)))?;
        drop(tx);
        Ok(None)
    }
}

pub(crate) enum SummaryVisit<'row> {
    DueHint(usize),
    Row(BorrowedThread<'row>),
}

/// Fast scheduled/summary archive scan in one consistent read snapshot:
/// counts archived rows via the covering `idx_threads_archived` index and
/// joins `threads` only for epochs strictly past the retention cutoff.
pub(crate) fn visit_archived_summary(
    c: &mut Connection,
    now: i64,
    duration: i64,
    mut visit: impl FnMut(SummaryVisit<'_>) -> Result<()>,
) -> Result<u64> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    if examined > 0
        && let Some(cutoff) = now.checked_sub(duration)
        && cutoff > 1
    {
        let due_count: i64 = tx.query_row(COUNT_DUE_EPOCHS_SQL, [cutoff], |r| r.get(0))?;
        let due_hint = usize::try_from(due_count.max(0))
            .unwrap_or(examined)
            .min(examined);
        if due_hint > 0 {
            visit(SummaryVisit::DueHint(due_hint))?;
            let mut stmt = tx.prepare(VISIT_ARCHIVED_DUE_SQL)?;
            let mut rows = stmt.query(params![PINNED_SECTION_ID, cutoff])?;
            while let Some(r) = rows.next()? {
                visit(SummaryVisit::Row(borrowed_row(r)?))?;
            }
        }
    }
    drop(tx);
    Ok(examined as u64)
}

pub(crate) fn add_spawn_dependencies(
    c: &Connection,
    graph: &mut crate::relations::Dependencies,
) -> Result<bool> {
    let mut query = c.prepare(SPAWN_SELECT_SQL)?;
    let mut rows = query.query([])?;
    let mut index = 0usize;
    while let Some(row) = rows.next()? {
        ensure!(
            index < 500000,
            "spawn graph exceeds the 500,000-edge safety limit"
        );
        index += 1;
        let parent = row.get_ref(0)?.as_str()?;
        let child = row.get_ref(1)?.as_str()?;
        let parent_id = crate::lineage::parse_canonical_uuid(parent)
            .context("noncanonical spawn relationship identity")?;
        let child_id = crate::lineage::parse_canonical_uuid(child)
            .context("noncanonical spawn relationship identity")?;
        graph.add_parent(child_id, parent_id);
    }
    Ok(index > 0)
}
