//! The adapter deliberately accepts one reviewed schema, not a best-effort guess.
use crate::{
    config::Policy,
    fsutil::{self, Identity},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Statement, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
struct Migration {
    version: i64,
    success: i64,
    checksum: String,
}

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
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Codex version check timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
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
    if access == DatabaseAccess::ReadWrite {
        c.execute_batch("PRAGMA synchronous=FULL;")?;
    }
    verify_base(&c)?;
    Ok(c)
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

pub fn verify_base(c: &Connection) -> Result<()> {
    let expected: Vec<SchemaObject> =
        serde_json::from_str(include_str!("../compatibility/schema.json"))?;
    ensure!(
        schema(c, false)? == expected,
        "unsupported or modified Codex schema; nothing will be deleted"
    );
    let expected: Vec<Migration> =
        serde_json::from_str(include_str!("../compatibility/migrations.json"))?;
    let mut stmt = c.prepare(
        "SELECT version,success,lower(hex(checksum)) FROM _sqlx_migrations ORDER BY version",
    )?;
    let actual = stmt
        .query_map([], |r| {
            Ok(Migration {
                version: r.get(0)?,
                success: r.get(1)?,
                checksum: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        actual == expected,
        "Codex migration history differs from the reviewed version"
    );
    let completed: Option<String> = c
        .query_row("SELECT status FROM backfill_state WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        completed.as_deref() == Some("complete"),
        "Codex index backfill is incomplete; wait for Codex to finish indexing"
    );
    Ok(())
}

pub fn identity(home: &Path) -> Result<Identity> {
    let (_file, metadata) = fsutil::regular_with_metadata(&home.join(DB_NAME), false, false)?;
    Ok(Identity::of(&metadata))
}
pub fn verify_policy(c: &Connection, p: &Policy) -> Result<()> {
    ensure!(
        identity(&p.codex_home)? == p.database,
        "Codex database was replaced; re-enable explicitly to start a new full grace period"
    );
    let reference = Connection::open_in_memory()?;
    // Trigger SQL is compared as stored by SQLite, avoiding whitespace/normalization guesses.
    reference
        .execute_batch("CREATE TABLE threads(id TEXT,archived INTEGER,archived_at INTEGER);")?;
    reference.execute_batch(CAPTURE_SQL)?;
    ensure!(
        schema(c, true)? == schema(&reference, true)?,
        "retention capture is missing or modified; deletion is disabled until explicit re-enable"
    );
    let owner: String = c.query_row(
        "SELECT owner FROM codex_retain_owner WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        owner == p.owner,
        "this Codex profile belongs to another retention policy"
    );
    Ok(())
}

pub fn install_capture(c: &mut Connection, owner: &str) -> Result<u64> {
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
    let count=tx.execute("INSERT INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at) SELECT id,unixepoch(),archived_at FROM threads WHERE archived=1",[])?;
    tx.commit()?;
    Ok(count as u64)
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

pub(crate) fn add_spawn_dependencies(
    c: &Connection,
    graph: &mut crate::relations::Dependencies,
) -> Result<()> {
    let mut query =
        c.prepare("SELECT parent_thread_id,child_thread_id FROM thread_spawn_edges LIMIT 500001")?;
    for (index, edge) in query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .enumerate()
    {
        ensure!(
            index < 500000,
            "spawn graph exceeds the 500,000-edge safety limit"
        );
        let (parent, child) = edge?;
        let parent_id = uuid::Uuid::parse_str(&parent)?;
        let child_id = uuid::Uuid::parse_str(&child)?;
        ensure!(
            parent_id.to_string() == parent && child_id.to_string() == child,
            "noncanonical spawn relationship identity"
        );
        graph.add_parent(child_id, parent_id);
    }
    Ok(())
}

/// Called in the same state transaction as owner deletion, never as a sweep.
pub(crate) fn delete_spawn_edges(c: &Connection, threads: &[Thread]) -> Result<()> {
    let mut query =
        c.prepare("DELETE FROM thread_spawn_edges WHERE parent_thread_id=? OR child_thread_id=?")?;
    for thread in threads {
        query.execute([&thread.id, &thread.id])?;
    }
    Ok(())
}
