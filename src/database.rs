//! Capability-based structural adapter for local Codex SQLite state databases.
//!
//! Works across Codex CLI, Codex Desktop, IDE extensions, and past/future
//! versions by discovering the active `state_<N>.sqlite` file and validating
//! the required structural contract on `threads` rather than pinning one exact
//! binary version or full schema checksum.

use crate::fsutil;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Statement, TransactionBehavior, params,
    params_from_iter,
    types::{Null, Value as SqlValue},
};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub const DB_NAME: &str = "state_5.sqlite";
pub const PINNED_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";
pub const DROP_CAPTURE: &str = "DROP TRIGGER IF EXISTS codex_retain_insert; DROP TRIGGER IF EXISTS codex_retain_update; DROP TRIGGER IF EXISTS codex_retain_delete; DROP TABLE IF EXISTS codex_retain_epochs; DROP TABLE IF EXISTS codex_retain_owner;";

const MAX_PINNED_SECTIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseAccess {
    ReadOnly,
    ReadWrite,
}

/// Capabilities probed dynamically from the active Codex state database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaCapabilities {
    pub database_name: String,
    pub has_name: bool,
    pub has_title: bool,
    pub has_history_mode: bool,
    pub has_is_pinned: bool,
    pub has_thread_section_id: bool,
    pub has_thread_sections_table: bool,
    pub has_spawn_edges: bool,
    pub has_backfill_state: bool,
    pub has_sqlx_migrations: bool,
    pub has_archived_index: bool,
    pub pinned_section_ids: Vec<String>,
    pub extra_pin_columns: Vec<String>,
    /// Foreign keys on other tables referencing `threads` with `RESTRICT` or
    /// `NO ACTION` (`(table, from_col, to_col)`). Referenced threads are
    /// protected as `dependent_thread` before staging any files.
    pub restricting_fks: Vec<(String, String, String)>,
}

/// Known standard `threads` columns from reviewed Codex schemas that do not
/// introduce new pin/protection semantics.
fn is_known_standard_thread_column(name: &str) -> bool {
    matches!(
        name,
        "id" | "rollout_path"
            | "created_at"
            | "updated_at"
            | "source"
            | "model_provider"
            | "cwd"
            | "title"
            | "sandbox_policy"
            | "approval_mode"
            | "tokens_used"
            | "has_user_event"
            | "archived"
            | "archived_at"
            | "git_sha"
            | "git_branch"
            | "git_origin_url"
            | "cli_version"
            | "first_user_message"
            | "agent_nickname"
            | "agent_role"
            | "memory_mode"
            | "model"
            | "reasoning_effort"
            | "agent_path"
            | "created_at_ms"
            | "updated_at_ms"
            | "thread_source"
            | "preview"
            | "recency_at"
            | "recency_at_ms"
            | "history_mode"
            | "name"
            | "is_pinned"
            | "thread_section_id"
            | "section_position"
            | "section_entered_at_ms"
            | "project_id"
    )
}

fn is_protective_column_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "pin", "protect", "keep", "retain", "star", "fav", "bookmark", "preserve", "hold",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_safe_sql_identifier(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parse `state_<N>.sqlite` where `<N>` is a non-empty ASCII integer.
fn parse_state_db_version(name: &str) -> Option<u64> {
    let middle = name.strip_prefix("state_")?.strip_suffix(".sqlite")?;
    if middle.is_empty() || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    middle.parse::<u64>().ok()
}

/// Discover the active Codex state database in `home` by selecting the highest
/// numeric `state_<N>.sqlite` file. Tolerates older leftover `state_<K>.sqlite`
/// files from earlier Codex releases.
pub fn discover_state_db(home: &Path) -> Result<(String, PathBuf)> {
    let mut best: Option<(u64, String, PathBuf)> = None;
    for entry in fs::read_dir(home)
        .with_context(|| format!("read Codex home directory {}", home.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(version) = parse_state_db_version(name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(cur, _, _)| version > *cur) {
            best = Some((version, name.to_owned(), entry.path()));
        }
    }
    let (_, name, path) = best.with_context(|| {
        format!(
            "no Codex state database (state_<N>.sqlite) found in {}",
            home.display()
        )
    })?;
    let _ = fsutil::regular(&path, false, false)?;
    Ok((name, path))
}

pub fn identity(home: &Path) -> Result<fsutil::Identity> {
    let (_name, path) = discover_state_db(home)?;
    let (_file, metadata) = fsutil::regular_with_metadata(&path, false, false)?;
    Ok(fsutil::Identity::of(&metadata))
}

/// Open the active Codex state database and validate its structural contract.
pub fn open_with_capabilities(
    home: &Path,
    access: DatabaseAccess,
) -> Result<(Connection, SchemaCapabilities)> {
    let (db_name, path) = discover_state_db(home)?;
    let flags = match access {
        DatabaseAccess::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        DatabaseAccess::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
    };
    let c = Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open Codex state database {}", path.display()))?;
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
    let caps = probe_schema(&c, &db_name)?;
    Ok((c, caps))
}

/// Convenience wrapper for callers that only need the validated connection.
pub fn open(home: &Path, access: DatabaseAccess) -> Result<Connection> {
    open_with_capabilities(home, access).map(|(c, _)| c)
}

pub(crate) fn shrink_memory(c: &Connection) {
    let _ = c.execute_batch("PRAGMA shrink_memory;");
}

fn table_columns(c: &Connection, table: &str) -> Result<Vec<(String, bool)>> {
    ensure!(is_safe_sql_identifier(table), "invalid table identifier");
    let mut stmt = c.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        let pk: i64 = row.get(5)?;
        Ok((name, pk > 0))
    })?;
    let mut cols = Vec::new();
    for row in rows {
        cols.push(row?);
    }
    Ok(cols)
}

/// Validate the minimum required structural contract on `c` and probe optional
/// version-dependent capabilities (pins, sections, history_mode, spawn edges).
pub fn probe_schema(c: &Connection, database_name: &str) -> Result<SchemaCapabilities> {
    let mut tables = HashSet::<String>::new();
    let mut has_archived_index = false;
    {
        let mut stmt = c.prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let kind = row.get_ref(0)?.as_str()?;
            let name = row.get_ref(1)?.as_str()?;
            if kind == "table" {
                tables.insert(name.to_owned());
            } else if kind == "index"
                && row.get_ref(2)?.as_str()? == "threads"
                && (name == "idx_threads_archived"
                    || row
                        .get_ref(3)?
                        .as_str_or_null()?
                        .is_some_and(|sql| sql.contains("(archived)")))
            {
                has_archived_index = true;
            }
        }
    }

    ensure!(
        tables.contains("threads"),
        "unsupported Codex database: missing threads table"
    );

    let thread_cols = table_columns(c, "threads")?;
    let mut has_id_pk = false;
    let mut has_rollout_path = false;
    let mut has_archived = false;
    let mut has_archived_at = false;
    let mut has_name = false;
    let mut has_title = false;
    let mut has_history_mode = false;
    let mut has_is_pinned = false;
    let mut has_thread_section_id = false;
    let mut extra_pin_columns = Vec::new();

    for (col, is_pk) in &thread_cols {
        match col.as_str() {
            "id" => has_id_pk = *is_pk,
            "rollout_path" => has_rollout_path = true,
            "archived" => has_archived = true,
            "archived_at" => has_archived_at = true,
            "name" => has_name = true,
            "title" => has_title = true,
            "history_mode" => has_history_mode = true,
            "is_pinned" => has_is_pinned = true,
            "thread_section_id" => has_thread_section_id = true,
            other => {
                if !is_known_standard_thread_column(other) && is_protective_column_name(other) {
                    ensure!(
                        is_safe_sql_identifier(other),
                        "unsupported Codex schema: protective column name cannot be safely checked: {other}"
                    );
                    extra_pin_columns.push(other.to_owned());
                }
            }
        }
    }
    extra_pin_columns.sort();

    ensure!(
        has_id_pk,
        "unsupported Codex schema: threads.id must be a primary key"
    );
    ensure!(
        has_rollout_path && has_archived && has_archived_at,
        "unsupported Codex schema: threads table lacks required archive columns (rollout_path, archived, archived_at)"
    );

    // Inspect foreign keys referencing `threads`:
    // 1. Reject a self-referential ON DELETE CASCADE on `threads` because
    //    deleting an archived parent row would silently cascade to children.
    // 2. Record any child table with ON DELETE RESTRICT / NO ACTION so
    //    referenced threads are skipped cleanly before staging files.
    let mut restricting_fks = Vec::new();
    let mut table_names: Vec<&str> = tables.iter().map(String::as_str).collect();
    table_names.sort_unstable();
    for tbl in table_names {
        if tbl.starts_with("codex_retain_") {
            continue;
        }
        let escaped_tbl = tbl.replace('"', "\"\"");
        let mut fk_stmt = c.prepare(&format!("PRAGMA foreign_key_list(\"{escaped_tbl}\")"))?;
        let mut fks = fk_stmt.query([])?;
        while let Some(row) = fks.next()? {
            let target_table = row.get_ref(2)?.as_str()?;
            if !target_table.eq_ignore_ascii_case("threads") {
                continue;
            }
            let from_col = row.get_ref(3)?.as_str()?;
            let to_col = row.get_ref(4)?.as_str_or_null()?.unwrap_or("id");
            let on_delete = row.get_ref(6)?.as_str()?;
            if tbl.eq_ignore_ascii_case("threads") && on_delete.eq_ignore_ascii_case("CASCADE") {
                bail!(
                    "unsupported Codex schema: self-referential ON DELETE CASCADE on threads would risk removing child threads"
                );
            }
            if tbl.eq_ignore_ascii_case("thread_spawn_edges") {
                continue;
            }
            if on_delete.eq_ignore_ascii_case("RESTRICT")
                || on_delete.eq_ignore_ascii_case("NO ACTION")
            {
                ensure!(
                    is_safe_sql_identifier(tbl)
                        && is_safe_sql_identifier(from_col)
                        && is_safe_sql_identifier(to_col),
                    "unsupported Codex schema: foreign key target cannot be safely inspected ({tbl}.{from_col})"
                );
                restricting_fks.push((tbl.to_owned(), from_col.to_owned(), to_col.to_owned()));
            }
        }
    }

    let has_sqlx_migrations = tables.contains("_sqlx_migrations");
    if has_sqlx_migrations {
        let mig_cols = table_columns(c, "_sqlx_migrations")?;
        if mig_cols.iter().any(|(col, _)| col == "success") {
            let has_failed: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE success = 0 OR success IS NULL)",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                !has_failed,
                "Codex database has an incomplete or failed migration; wait for Codex to finish migrating"
            );
        }
    }

    let has_backfill_state = tables.contains("backfill_state");
    if has_backfill_state {
        let bf_cols = table_columns(c, "backfill_state")?;
        if bf_cols.iter().any(|(col, _)| col == "id")
            && bf_cols.iter().any(|(col, _)| col == "status")
        {
            let status: Option<String> = c
                .query_row(
                    "SELECT status FROM backfill_state WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(status) = status {
                ensure!(
                    status == "complete",
                    "Codex index backfill is incomplete ({status}); wait for Codex to finish indexing"
                );
            }
        }
    }

    let has_spawn_edges = if tables.contains("thread_spawn_edges") {
        let edge_cols = table_columns(c, "thread_spawn_edges")?;
        let has_parent = edge_cols.iter().any(|(col, _)| col == "parent_thread_id");
        let has_child = edge_cols.iter().any(|(col, _)| col == "child_thread_id");
        ensure!(
            has_parent && has_child,
            "unsupported Codex schema: thread_spawn_edges lacks parent_thread_id/child_thread_id columns"
        );
        true
    } else {
        false
    };

    let has_thread_sections_table = tables.contains("thread_sections");
    let mut pinned_section_ids = Vec::new();
    if has_thread_section_id {
        pinned_section_ids.push(PINNED_SECTION_ID.to_owned());
        if has_thread_sections_table {
            let sec_cols = table_columns(c, "thread_sections")?;
            if sec_cols.iter().any(|(col, _)| col == "id")
                && sec_cols.iter().any(|(col, _)| col == "name")
            {
                let mut stmt = c.prepare(
                    "SELECT id FROM thread_sections WHERE lower(trim(name)) = 'pinned' ORDER BY id",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let sid: String = row.get(0)?;
                    if !pinned_section_ids.iter().any(|existing| existing == &sid) {
                        ensure!(
                            pinned_section_ids.len() < MAX_PINNED_SECTIONS,
                            "Codex database defines more than {MAX_PINNED_SECTIONS} pinned sections"
                        );
                        pinned_section_ids.push(sid);
                    }
                }
            }
        }
    }

    Ok(SchemaCapabilities {
        database_name: database_name.to_owned(),
        has_name,
        has_title,
        has_history_mode,
        has_is_pinned,
        has_thread_section_id,
        has_thread_sections_table,
        has_spawn_edges,
        has_backfill_state,
        has_sqlx_migrations,
        has_archived_index,
        pinned_section_ids,
        extra_pin_columns,
        restricting_fks,
    })
}

/// Verify only the base structural contract (used by tests and recovery).
pub fn verify_base(c: &Connection) -> Result<SchemaCapabilities> {
    probe_schema(c, DB_NAME)
}

impl SchemaCapabilities {
    fn pinned_case_sql(&self, first_param: usize) -> (String, usize) {
        let mut preds = Vec::new();
        let mut next_param = first_param;
        if self.has_is_pinned {
            preds.push("t.is_pinned=0".to_owned());
        }
        if self.has_thread_section_id {
            for _ in &self.pinned_section_ids {
                preds.push(format!("t.thread_section_id IS NOT ?{next_param}"));
                next_param += 1;
            }
        }
        for col in &self.extra_pin_columns {
            preds.push(format!(
                "(t.\"{col}\" IS NULL OR t.\"{col}\"=0 OR t.\"{col}\"='')"
            ));
        }
        if preds.is_empty() {
            ("0".to_owned(), next_param)
        } else {
            (
                format!("CASE WHEN {} THEN 0 ELSE 1 END", preds.join(" AND ")),
                next_param,
            )
        }
    }

    fn pin_param_values(&self) -> Vec<SqlValue> {
        if self.has_thread_section_id {
            self.pinned_section_ids
                .iter()
                .map(|id| SqlValue::Text(id.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    fn history_mode_sql(&self) -> &'static str {
        if self.has_history_mode {
            "coalesce(t.history_mode,'')"
        } else {
            "'legacy'"
        }
    }

    fn title_sql(&self) -> &'static str {
        match (self.has_name, self.has_title) {
            (true, true) => "substr(coalesce(nullif(t.name,''),t.title),1,160)",
            (true, false) => "substr(coalesce(t.name,''),1,160)",
            (false, true) => "substr(coalesce(t.title,''),1,160)",
            (false, false) => "''",
        }
    }

    fn name_and_title_sql(&self) -> &'static str {
        match (self.has_name, self.has_title) {
            (true, true) => "t.name,t.title",
            (true, false) => "t.name,''",
            (false, true) => "NULL,t.title",
            (false, false) => "NULL,''",
        }
    }

    fn batch_thread_lookup_sql(&self) -> (String, usize) {
        let (pinned, id_param) = self.pinned_case_sql(1);
        let mode = self.history_mode_sql();
        let sql = format!(
            "SELECT t.id,t.rollout_path,t.archived,t.archived_at,{pinned},{mode},'' \
             FROM threads t WHERE t.id=?{id_param}"
        );
        (sql, id_param)
    }

    fn batch_delete_sql(&self) -> String {
        let mut sql = String::from(
            "DELETE FROM threads WHERE id=?1 AND archived=1 AND archived_at IS ?2 AND rollout_path=?3",
        );
        let mut next_param = 4usize;
        if self.has_history_mode {
            sql.push_str(&format!(
                " AND history_mode=?{next_param} AND history_mode IN ('legacy','paginated')"
            ));
            next_param += 1;
        }
        if self.has_is_pinned {
            sql.push_str(" AND is_pinned=0");
        }
        if self.has_thread_section_id {
            for _ in &self.pinned_section_ids {
                sql.push_str(&format!(" AND thread_section_id IS NOT ?{next_param}"));
                next_param += 1;
            }
        }
        for col in &self.extra_pin_columns {
            sql.push_str(&format!(
                " AND (\"{col}\" IS NULL OR \"{col}\"=0 OR \"{col}\"='')"
            ));
        }
        sql
    }

    fn visit_archived_full_sql(&self) -> (String, usize) {
        let (pinned, null_param) = self.pinned_case_sql(1);
        let mode = self.history_mode_sql();
        let title = self.title_sql();
        let select = format!(
            "SELECT t.id,t.rollout_path,t.archived,t.archived_at,{pinned},{mode},{title} FROM threads t"
        );
        let sql = format!(
            "{select} WHERE t.archived > 0 \
             UNION ALL {select} WHERE t.archived < 0 \
             UNION ALL {select} WHERE t.archived IS ?{null_param} \
             LIMIT 100001"
        );
        (sql, null_param)
    }

    fn visit_archived_core_sql(&self) -> String {
        let (pinned, _) = self.pinned_case_sql(1);
        let mode = self.history_mode_sql();
        let name_title = self.name_and_title_sql();
        let select = format!(
            "SELECT t.id,t.archived,t.archived_at,{pinned},{mode},{name_title} FROM threads t"
        );
        format!(
            "{select} WHERE t.archived > 0 \
             UNION ALL {select} WHERE t.archived < 0 \
             LIMIT 100001"
        )
    }

    fn visit_archived_top_sql(&self) -> (String, usize) {
        let (pinned, limit_param) = self.pinned_case_sql(1);
        let mode = self.history_mode_sql();
        let title = self.title_sql();
        let sql = format!(
            "SELECT t.id,t.rollout_path,t.archived,t.archived_at,{pinned},{mode},{title} \
             FROM threads t WHERE t.archived <> 0 ORDER BY t.id LIMIT ?{limit_param}"
        );
        (sql, limit_param)
    }

    fn lookup_archived_by_id_sql(&self) -> (String, usize) {
        let (pinned, id_param) = self.pinned_case_sql(1);
        let mode = self.history_mode_sql();
        let title = self.title_sql();
        let sql = format!(
            "SELECT t.id,t.rollout_path,t.archived,t.archived_at,{pinned},{mode},{title} \
             FROM threads t WHERE t.id=?{id_param} AND t.archived=1"
        );
        (sql, id_param)
    }
}

/// Check whether legacy 0.1.x `codex_retain_*` SQLite objects exist in `c`.
pub fn has_legacy_capture(c: &Connection) -> Result<bool> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name LIKE 'codex_retain_%')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists)
}

/// One legacy `0.1.x` captured archive epoch read from `codex_retain_epochs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEpoch {
    pub thread_id: String,
    pub archived_since: i64,
    pub codex_archived_at: Option<i64>,
}

/// Read legacy 0.1.x captured epochs from Codex's SQLite DB if present and owned
/// by `owner`, allowing seamless zero-data-loss migration to the sidecar tracker.
pub fn read_legacy_capture_epochs(c: &Connection, owner: &str) -> Result<Option<Vec<LegacyEpoch>>> {
    let has_owner_table: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_retain_owner')",
        [],
        |row| row.get(0),
    )?;
    let has_epochs_table: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_retain_epochs')",
        [],
        |row| row.get(0),
    )?;
    if !has_owner_table && !has_epochs_table {
        return Ok(None);
    }
    ensure!(
        has_owner_table && has_epochs_table,
        "legacy retention capture in Codex database is incomplete; run disable and re-enable"
    );
    let recorded_owner: String = c
        .query_row(
            "SELECT owner FROM codex_retain_owner WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .context("unrecognized legacy retention owner")?;
    ensure!(
        recorded_owner == owner,
        "this Codex profile has legacy retention triggers belonging to another policy"
    );
    let mut stmt =
        c.prepare("SELECT thread_id, archived_since, codex_archived_at FROM codex_retain_epochs")?;
    let mut rows = stmt.query([])?;
    let mut epochs = Vec::new();
    while let Some(row) = rows.next()? {
        epochs.push(LegacyEpoch {
            thread_id: row.get(0)?,
            archived_since: row.get(1)?,
            codex_archived_at: row.get(2)?,
        });
    }
    Ok(Some(epochs))
}

/// Remove any leftover legacy 0.1.x `codex_retain_*` triggers/tables owned by
/// `owner` from Codex's SQLite DB. No-op when no legacy objects exist.
pub fn remove_legacy_capture(c: &mut Connection, owner: &str) -> Result<bool> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !has_legacy_capture(&tx)? {
        tx.commit()?;
        return Ok(false);
    }
    let has_owner_table: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='codex_retain_owner')",
        [],
        |row| row.get(0),
    )?;
    if has_owner_table {
        let recorded_owner: Option<String> = tx
            .query_row(
                "SELECT owner FROM codex_retain_owner WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(recorded_owner) = recorded_owner {
            ensure!(
                recorded_owner == owner,
                "this Codex profile has legacy retention triggers belonging to another policy"
            );
        }
    }
    tx.execute_batch(DROP_CAPTURE)?;
    tx.commit()?;
    Ok(true)
}

const SPAWN_SELECT_SQL: &str =
    "SELECT parent_thread_id,child_thread_id FROM thread_spawn_edges LIMIT 500001";
const SPAWN_DELETE_SQL: &str =
    "DELETE FROM thread_spawn_edges WHERE parent_thread_id=? OR child_thread_id=?";

const COUNT_ARCHIVED_SQL: &str = "SELECT count(*), max(is_null) FROM (\
    SELECT 0 AS is_null FROM threads WHERE archived > 0 \
    UNION ALL SELECT 0 AS is_null FROM threads WHERE archived < 0 \
    UNION ALL SELECT 1 AS is_null FROM threads WHERE archived IS ?1 \
    LIMIT 100001\
 )";

const COUNT_ACTIVE_BOUNDED_SQL: &str =
    "SELECT count(*) FROM (SELECT 1 FROM threads WHERE archived = 0 LIMIT ?1)";

/// Reusable SQLite prepared statements bound to one cleanup connection.
/// Automatically re-probes `SchemaCapabilities` on every `BEGIN IMMEDIATE` and
/// recompiles if Codex migrated schema or changed pinned sections mid-run.
pub(crate) struct BatchStatements<'connection> {
    connection: &'connection Connection,
    caps: SchemaCapabilities,
    thread_lookup: Statement<'connection>,
    spawn_select: Option<Statement<'connection>>,
    batch_delete: Statement<'connection>,
    spawn_delete: Option<Statement<'connection>>,
}

impl<'connection> BatchStatements<'connection> {
    pub(crate) fn prepare_and_verify(c: &'connection Connection, db_name: &str) -> Result<Self> {
        let caps = probe_schema(c, db_name)?;
        Self::compile(c, caps)
    }

    fn compile(c: &'connection Connection, caps: SchemaCapabilities) -> Result<Self> {
        let (lookup_sql, _) = caps.batch_thread_lookup_sql();
        let delete_sql = caps.batch_delete_sql();
        let (spawn_select, spawn_delete) = if caps.has_spawn_edges {
            (
                Some(c.prepare(SPAWN_SELECT_SQL)?),
                Some(c.prepare(SPAWN_DELETE_SQL)?),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            connection: c,
            caps,
            thread_lookup: c.prepare(&lookup_sql)?,
            spawn_select,
            batch_delete: c.prepare(&delete_sql)?,
            spawn_delete,
        })
    }

    /// Re-verify the structural schema contract inside `BEGIN IMMEDIATE`. If
    /// optional capabilities or pinned sections changed mid-run, recompile the
    /// batch statements against the fresh schema.
    pub(crate) fn verify_and_refresh(&mut self) -> Result<()> {
        let fresh = probe_schema(self.connection, &self.caps.database_name)?;
        if fresh != self.caps {
            *self = Self::compile(self.connection, fresh)?;
        }
        Ok(())
    }

    pub(crate) fn lookup_batch_thread(
        &mut self,
        id: &str,
        mut on_row: impl FnMut(BorrowedThreadRow<'_>) -> Result<BatchThread>,
    ) -> Result<Option<BatchThread>> {
        let mut bound = self.caps.pin_param_values();
        bound.push(SqlValue::Text(id.to_owned()));
        let mut rows = self.thread_lookup.query(params_from_iter(bound))?;
        let Some(r) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(on_row(borrowed_thread_row(r)?)?))
    }

    pub(crate) fn add_spawn_dependencies(
        &mut self,
        graph: &mut crate::relations::Dependencies,
    ) -> Result<bool> {
        let mut index = 0usize;
        if let Some(stmt) = self.spawn_select.as_mut() {
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                ensure!(
                    index < 500_000,
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
        }
        add_restricting_fk_dependencies(self.connection, &self.caps, graph)?;
        Ok(index > 0)
    }

    pub(crate) fn delete_batch_rows(&mut self, threads: &[BatchThread]) -> Result<()> {
        for thread in threads {
            let path = thread
                .path
                .to_str()
                .context("non-UTF-8 Codex database path")?;
            let mut bound: Vec<SqlValue> =
                Vec::with_capacity(6 + self.caps.pinned_section_ids.len());
            bound.push(SqlValue::Text(thread.id.clone()));
            bound.push(thread.archived_at.map_or(SqlValue::Null, SqlValue::Integer));
            bound.push(SqlValue::Text(path.to_owned()));
            if self.caps.has_history_mode {
                bound.push(SqlValue::Text(thread.history_mode.to_owned()));
            }
            if self.caps.has_thread_section_id {
                for sid in &self.caps.pinned_section_ids {
                    bound.push(SqlValue::Text(sid.clone()));
                }
            }
            let affected = self.batch_delete.execute(params_from_iter(bound))?;
            ensure!(affected == 1, "thread changed before deletion");
        }
        Ok(())
    }

    pub(crate) fn delete_spawn_edges(&mut self, threads: &[BatchThread]) -> Result<()> {
        let Some(stmt) = self.spawn_delete.as_mut() else {
            return Ok(());
        };
        for thread in threads {
            stmt.execute([&thread.id, &thread.id])?;
        }
        Ok(())
    }
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

pub(crate) struct BorrowedThreadRow<'row> {
    pub id: &'row str,
    pub title: &'row str,
    pub path: &'row str,
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub pinned: i64,
    pub history_mode: &'row str,
}

pub(crate) struct BorrowedThreadCore<'row> {
    pub id: &'row str,
    pub title: &'row str,
    pub archived: i64,
    pub archived_at: Option<i64>,
    pub pinned: i64,
    pub history_mode: &'row str,
}

pub(crate) struct BatchThread {
    pub id: String,
    pub path: PathBuf,
    pub archived_at: Option<i64>,
    pub history_mode: &'static str,
}

fn borrowed_thread_row<'row>(
    r: &'row rusqlite::Row<'_>,
) -> rusqlite::Result<BorrowedThreadRow<'row>> {
    Ok(BorrowedThreadRow {
        id: r.get_ref(0)?.as_str()?,
        path: r.get_ref(1)?.as_str()?,
        archived: r.get(2)?,
        archived_at: r.get(3)?,
        pinned: r.get(4)?,
        history_mode: r.get_ref(5)?.as_str()?,
        title: r.get_ref(6)?.as_str()?,
    })
}

pub(crate) fn count_archived_in_tx(tx: &Connection) -> Result<usize> {
    let mut count_stmt = tx.prepare(COUNT_ARCHIVED_SQL)?;
    let (examined, has_null): (i64, Option<i64>) =
        count_stmt.query_row(params![Null], |r| Ok((r.get(0)?, r.get(1)?)))?;
    ensure!(
        examined <= 100_000,
        "archive exceeds the 100,000-thread safety limit; no changes made"
    );
    ensure!(
        has_null != Some(1),
        "archived status is NULL; cannot safely evaluate retention"
    );
    Ok(examined as usize)
}

pub(crate) fn visit_archived_full(
    c: &Connection,
    caps: &SchemaCapabilities,
    mut visit: impl FnMut(BorrowedThreadRow<'_>) -> Result<()>,
) -> Result<()> {
    let (sql, _) = caps.visit_archived_full_sql();
    let mut bound = caps.pin_param_values();
    bound.push(SqlValue::Null);
    let mut s = c.prepare(&sql)?;
    let mut rows = s.query(params_from_iter(bound))?;
    let mut index = 0usize;
    while let Some(r) = rows.next()? {
        ensure!(
            index < 100_000,
            "archive exceeds the 100,000-thread safety limit; no changes made"
        );
        index += 1;
        visit(borrowed_thread_row(r)?)?;
    }
    Ok(())
}

pub(crate) enum FullVisit<'row> {
    Count(usize),
    Row(BorrowedThreadRow<'row>),
}

pub(crate) fn visit_archived_full_with_count(
    c: &mut Connection,
    caps: &SchemaCapabilities,
    mut visit: impl FnMut(FullVisit<'_>) -> Result<()>,
) -> Result<()> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    visit(FullVisit::Count(examined))?;
    if examined > 0 {
        visit_archived_full(&tx, caps, |row| visit(FullVisit::Row(row)))?;
    }
    drop(tx);
    Ok(())
}

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

pub(crate) enum CompactThreadsEvent<'row> {
    Count(usize),
    Thread(BorrowedThreadCore<'row>),
}

/// Sequential archive core scan in one consistent read transaction, omitting
/// `rollout_path` until eligible candidates are identified.
pub(crate) fn visit_archived_compact_threads<S>(
    c: &mut Connection,
    caps: &SchemaCapabilities,
    state: &mut S,
    mut visit: impl FnMut(&mut S, CompactThreadsEvent<'_>) -> Result<()>,
    mut after_threads: impl FnMut(&mut S, &Connection) -> Result<()>,
) -> Result<()> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    visit(state, CompactThreadsEvent::Count(examined))?;
    if examined > 0 {
        {
            let sql = caps.visit_archived_core_sql();
            let bound = caps.pin_param_values();
            let mut stmt = tx.prepare(&sql)?;
            let mut rows = stmt.query(params_from_iter(bound))?;
            let mut index = 0usize;
            while let Some(r) = rows.next()? {
                ensure!(
                    index < 100_000,
                    "archive exceeds the 100,000-thread safety limit; no changes made"
                );
                index += 1;
                let name = r.get_ref(5)?.as_str_or_null()?;
                let raw_title = match name {
                    Some(n) if !n.is_empty() => n,
                    _ => r.get_ref(6)?.as_str_or_null()?.unwrap_or(""),
                };
                visit(
                    state,
                    CompactThreadsEvent::Thread(BorrowedThreadCore {
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
        after_threads(state, &tx)?;
    }
    drop(tx);
    Ok(())
}

pub(crate) fn prepare_eligible_path_lookup<'c>(
    tx: &'c Connection,
    caps: &SchemaCapabilities,
) -> Result<Statement<'c>> {
    let sql = if caps.has_history_mode {
        "SELECT rollout_path, coalesce(history_mode,'') FROM threads WHERE id=?1"
    } else {
        "SELECT rollout_path, 'legacy' FROM threads WHERE id=?1"
    };
    Ok(tx.prepare(sql)?)
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
    caps: &SchemaCapabilities,
    mut on_row: impl FnMut(&str, &str, bool) -> Result<()>,
) -> Result<()> {
    let sql = if caps.has_history_mode {
        "SELECT id, rollout_path, coalesce(history_mode,'') FROM threads WHERE archived=1"
    } else {
        "SELECT id, rollout_path, 'legacy' FROM threads WHERE archived=1"
    };
    let mut stmt = tx.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id = r.get_ref(0)?.as_str()?;
        let path = r.get_ref(1)?.as_str()?;
        let paginated = r.get_ref(2)?.as_str()? == "paginated";
        on_row(id, path, paginated)?;
    }
    Ok(())
}

pub(crate) enum TextScanEvent<'row> {
    Due(BorrowedThreadRow<'row>),
    Prefix(BorrowedThreadRow<'row>),
    Full(BorrowedThreadRow<'row>),
}

/// Fast text-mode archive scan in one consistent read snapshot.
/// When `examined >= 4096` and active rows do not outnumber archived rows,
/// queries only `due_ids` (pre-filtered by sidecar tracker) plus the
/// lexicographically smallest `limit` archived rows.
pub(crate) fn visit_archived_text(
    c: &mut Connection,
    caps: &SchemaCapabilities,
    due_ids: Option<&[String]>,
    limit: usize,
    mut visit: impl FnMut(TextScanEvent<'_>) -> Result<()>,
) -> Result<Option<u64>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    if examined == 0 {
        return Ok(Some(0));
    }
    let use_top_fast_path = if examined >= 4096 && due_ids.is_some() {
        let mut active_stmt = tx.prepare(COUNT_ACTIVE_BOUNDED_SQL)?;
        let active_bounded: i64 = active_stmt.query_row([examined as i64], |r| r.get(0))?;
        (active_bounded as usize) < examined
    } else {
        false
    };
    if use_top_fast_path {
        let due_ids = due_ids.expect("checked above");
        if !due_ids.is_empty() {
            let (sql, _) = caps.lookup_archived_by_id_sql();
            let mut stmt = tx.prepare(&sql)?;
            let base_params = caps.pin_param_values();
            let mut bound = Vec::with_capacity(base_params.len() + 1);
            for id in due_ids {
                bound.clear();
                bound.extend(base_params.iter().cloned());
                bound.push(SqlValue::Text(id.clone()));
                let mut rows = stmt.query(params_from_iter(bound.iter()))?;
                if let Some(r) = rows.next()? {
                    visit(TextScanEvent::Due(borrowed_thread_row(r)?))?;
                }
            }
        }
        {
            let (sql, _) = caps.visit_archived_top_sql();
            let mut bound = caps.pin_param_values();
            bound.push(SqlValue::Integer(limit as i64));
            let mut top = tx.prepare(&sql)?;
            let mut rows = top.query(params_from_iter(bound))?;
            while let Some(r) = rows.next()? {
                visit(TextScanEvent::Prefix(borrowed_thread_row(r)?))?;
            }
        }
        drop(tx);
        Ok(Some(examined as u64))
    } else {
        visit_archived_full(&tx, caps, |row| visit(TextScanEvent::Full(row)))?;
        drop(tx);
        Ok(None)
    }
}

/// Fast scheduled/summary archive scan in one consistent read snapshot:
/// counts archived rows via `idx_threads_archived` and looks up ONLY `due_ids`
/// identified from the sidecar tracker.
pub(crate) fn visit_archived_summary_by_ids(
    c: &mut Connection,
    caps: &SchemaCapabilities,
    due_ids: &[String],
    mut visit: impl FnMut(BorrowedThreadRow<'_>) -> Result<()>,
) -> Result<u64> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let examined = count_archived_in_tx(&tx)?;
    if examined > 0 && !due_ids.is_empty() {
        let (sql, _) = caps.lookup_archived_by_id_sql();
        let mut stmt = tx.prepare(&sql)?;
        let base_params = caps.pin_param_values();
        let mut bound = Vec::with_capacity(base_params.len() + 1);
        for id in due_ids {
            bound.clear();
            bound.extend(base_params.iter().cloned());
            bound.push(SqlValue::Text(id.clone()));
            let mut rows = stmt.query(params_from_iter(bound.iter()))?;
            if let Some(r) = rows.next()? {
                visit(borrowed_thread_row(r)?)?;
            }
        }
    }
    drop(tx);
    Ok(examined as u64)
}

fn add_restricting_fk_dependencies(
    c: &Connection,
    caps: &SchemaCapabilities,
    graph: &mut crate::relations::Dependencies,
) -> Result<()> {
    for (tbl, from_col, to_col) in &caps.restricting_fks {
        let sql = format!(
            "SELECT t.id FROM threads t WHERE t.archived = 1 \
             AND EXISTS(SELECT 1 FROM \"{tbl}\" c WHERE c.\"{from_col}\" = t.\"{to_col}\")"
        );
        let mut stmt = c.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get_ref(0)?.as_str()?;
            if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
                graph.add_external_dependent(uuid);
            }
        }
    }
    Ok(())
}

pub(crate) fn add_spawn_dependencies(
    c: &Connection,
    caps: &SchemaCapabilities,
    graph: &mut crate::relations::Dependencies,
) -> Result<bool> {
    let mut index = 0usize;
    if caps.has_spawn_edges {
        let mut query = c.prepare(SPAWN_SELECT_SQL)?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            ensure!(
                index < 500_000,
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
    }
    add_restricting_fk_dependencies(c, caps, graph)?;
    Ok(index > 0)
}

/// Public single-row lookup helper (used by tests to inspect current thread state).
pub fn lookup_thread_row(
    c: &Connection,
    caps: &SchemaCapabilities,
    id: &str,
) -> Result<Option<Thread>> {
    let (pinned, id_param) = caps.pinned_case_sql(1);
    let mode = caps.history_mode_sql();
    let title = caps.title_sql();
    let related = if caps.has_spawn_edges {
        "EXISTS(SELECT 1 FROM thread_spawn_edges s WHERE s.parent_thread_id=t.id OR s.child_thread_id=t.id)"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT t.id,t.rollout_path,t.archived,t.archived_at,{pinned},{mode},{related},{title} \
         FROM threads t WHERE t.id=?{id_param}"
    );
    let mut bound = caps.pin_param_values();
    bound.push(SqlValue::Text(id.to_owned()));
    let mut stmt = c.prepare(&sql)?;
    let row = stmt
        .query_row(params_from_iter(bound), |r| {
            Ok(Thread {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                archived: r.get(2)?,
                archived_at: r.get(3)?,
                pinned: r.get(4)?,
                history_mode: r.get(5)?,
                related: r.get(6)?,
                title: r.get(7)?,
                epoch: None,
                recorded_archive: None,
            })
        })
        .optional()?;
    Ok(row)
}

/// Public conditional delete helper for tests verifying SQL pin/state guards.
pub fn delete_thread_checked(c: &Connection, caps: &SchemaCapabilities, t: &Thread) -> Result<()> {
    let sql = caps.batch_delete_sql();
    let mut stmt = c.prepare(&sql)?;
    let path = t.path.to_str().context("non-UTF-8 Codex database path")?;
    let mut bound: Vec<SqlValue> = Vec::with_capacity(6 + caps.pinned_section_ids.len());
    bound.push(SqlValue::Text(t.id.clone()));
    bound.push(t.archived_at.map_or(SqlValue::Null, SqlValue::Integer));
    bound.push(SqlValue::Text(path.to_owned()));
    if caps.has_history_mode {
        bound.push(SqlValue::Text(t.history_mode.clone()));
    }
    if caps.has_thread_section_id {
        for sid in &caps.pinned_section_ids {
            bound.push(SqlValue::Text(sid.clone()));
        }
    }
    let affected = stmt.execute(params_from_iter(bound))?;
    ensure!(affected == 1, "thread changed before deletion");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_highest_state_db_version_and_ignores_backups() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("state_4.sqlite"), b"").unwrap();
        fs::write(temp.path().join("state_5.sqlite"), b"").unwrap();
        fs::write(temp.path().join("state_12.sqlite"), b"").unwrap();
        fs::write(temp.path().join("state_99_backup.sqlite"), b"").unwrap();
        fs::write(temp.path().join("state_abc.sqlite"), b"").unwrap();
        let (name, path) = discover_state_db(temp.path()).unwrap();
        assert_eq!(name, "state_12.sqlite");
        assert_eq!(path, temp.path().join("state_12.sqlite"));
    }

    #[test]
    fn probes_minimal_legacy_schema_without_optional_columns_or_tables() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                title TEXT NOT NULL DEFAULT ''
            );",
        )
        .unwrap();
        let caps = probe_schema(&c, "state_4.sqlite").unwrap();
        assert_eq!(caps.database_name, "state_4.sqlite");
        assert!(!caps.has_name);
        assert!(caps.has_title);
        assert!(!caps.has_history_mode);
        assert!(!caps.has_is_pinned);
        assert!(!caps.has_thread_section_id);
        assert!(!caps.has_spawn_edges);
        assert!(!caps.has_backfill_state);
        assert!(!caps.has_sqlx_migrations);
        assert!(caps.extra_pin_columns.is_empty());
    }

    #[test]
    fn probes_future_schema_with_extra_tables_migrations_and_protective_columns() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL
            );
            INSERT INTO _sqlx_migrations VALUES (100, 'future feature', 1, x'deadbeef');
            CREATE TABLE thread_sections (id TEXT PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO thread_sections VALUES ('custom-pinned-uuid', 'Pinned');
            CREATE TABLE future_plugins (id TEXT PRIMARY KEY, config TEXT);
            CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                title TEXT NOT NULL DEFAULT '',
                history_mode TEXT NOT NULL DEFAULT 'legacy',
                is_pinned INTEGER NOT NULL DEFAULT 0,
                thread_section_id TEXT,
                pinned_at INTEGER,
                is_starred INTEGER NOT NULL DEFAULT 0,
                future_ui_color TEXT
            );",
        )
        .unwrap();
        let caps = probe_schema(&c, "state_6.sqlite").unwrap();
        assert_eq!(caps.database_name, "state_6.sqlite");
        assert!(
            caps.pinned_section_ids
                .contains(&PINNED_SECTION_ID.to_owned())
        );
        assert!(
            caps.pinned_section_ids
                .contains(&"custom-pinned-uuid".to_owned())
        );
        assert_eq!(caps.extra_pin_columns, vec!["is_starred", "pinned_at"]);
    }

    #[test]
    fn rejects_failed_migration_missing_core_columns_and_self_cascade_fk() {
        // Missing archived_at
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, archived INTEGER);",
        )
        .unwrap();
        assert!(probe_schema(&c, "state_5.sqlite").is_err());

        // Self-referential ON DELETE CASCADE
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                parent_id TEXT REFERENCES threads(id) ON DELETE CASCADE,
                rollout_path TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER
            );",
        )
        .unwrap();
        assert!(
            probe_schema(&c, "state_6.sqlite")
                .unwrap_err()
                .to_string()
                .contains("CASCADE")
        );

        // Failed SQLx migration
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, success BOOLEAN NOT NULL);
             INSERT INTO _sqlx_migrations VALUES (53, 0);
             CREATE TABLE threads (
                 id TEXT PRIMARY KEY,
                 rollout_path TEXT NOT NULL,
                 archived INTEGER NOT NULL DEFAULT 0,
                 archived_at INTEGER
             );",
        )
        .unwrap();
        assert!(
            probe_schema(&c, "state_5.sqlite")
                .unwrap_err()
                .to_string()
                .contains("migration")
        );
    }
}
