use codex_retain::{
    config::{Policy, Store},
    database, engine, fsutil, tracker,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub home: PathBuf,
    pub c: Connection,
    pub policy: Policy,
    pub store: Store,
}
impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("codex");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join("archived_sessions")).unwrap();
        fs::create_dir(home.join("sessions")).unwrap();
        let mut c = Connection::open(home.join(database::DB_NAME)).unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")
            .unwrap();
        let objects: Vec<Value> =
            serde_json::from_str(include_str!("../../compatibility/schema.json")).unwrap();
        for kind in ["table", "index", "trigger", "view"] {
            for object in objects.iter().filter(|o| o["type"] == kind) {
                if let Some(sql) = object["sql"].as_str() {
                    c.execute_batch(sql).unwrap();
                }
            }
        }
        let migrations: Vec<Value> =
            serde_json::from_str(include_str!("../../compatibility/migrations.json")).unwrap();
        for m in migrations {
            let raw = m["checksum"].as_str().unwrap();
            let checksum: Vec<u8> = raw
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
                .collect();
            c.execute("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(?,'synthetic compatibility fixture',?,?,0)",
                params![m["version"].as_i64().unwrap(),m["success"].as_i64().unwrap_or_else(||i64::from(m["success"].as_bool().unwrap())),checksum]).unwrap();
        }
        c.execute(
            "INSERT INTO backfill_state(id,status,updated_at) VALUES(1,'complete',?)",
            [now()],
        )
        .unwrap();
        database::verify_base(&c).unwrap();
        let store = Store::open(root.join("state")).unwrap();
        let owner = store.root.to_str().unwrap().to_owned();
        let enabled_at = now() - 10;
        let policy = Policy {
            schema: 1,
            codex_home: home.clone(),
            codex_bin: None,
            database: Some(database::identity(&home).unwrap()),
            database_name: Some(database::DB_NAME.to_owned()),
            retention_days: 30,
            enabled: true,
            paused: false,
            automatic: false,
            enabled_at,
            owner,
            exclusions: BTreeSet::new(),
        };
        tracker::seed(&store, &mut c, &policy, enabled_at).unwrap();
        Self {
            temp,
            home,
            c,
            policy,
            store,
        }
    }
    pub fn tracker_conn(&self) -> Connection {
        Connection::open(tracker::tracker_path(&self.store)).unwrap()
    }
    pub fn path(&self, id: &str, archived: bool) -> PathBuf {
        self.home
            .join(if archived {
                "archived_sessions"
            } else {
                "sessions"
            })
            .join(format!("rollout-2025-01-01T00-00-00-{id}.jsonl"))
    }
    pub fn add(&self, n: u128, archived: i64) -> String {
        let id = uuid::Uuid::from_u128(n).to_string();
        let path = self.path(&id, archived != 0);
        let header = json!({"timestamp":"2025-01-01T00:00:00Z","type":"session_meta","payload":{"id":id,"timestamp":"2025-01-01T00:00:00Z","history_mode":"legacy"}});
        fs::write(&path,format!("{header}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"synthetic only\"}}}}\n")).unwrap();
        let archived_at = if archived == 0 { None } else { Some(1i64) };
        self.c.execute("INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at) VALUES(?,?,1,1,'cli','fixture',?,'fixture','{}','never',?,?)",
            params![id,path.to_str().unwrap(),self.home.to_str().unwrap(),archived,archived_at]).unwrap();
        if archived == 1 && tracker::tracker_path(&self.store).exists() {
            let tc = self.tracker_conn();
            tc.execute(
                "INSERT OR REPLACE INTO tracked_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?3)",
                params![&id, now(), archived_at],
            )
            .unwrap();
        }
        id
    }
    pub fn epoch(&self, id: &str) -> Option<i64> {
        let tc = self.tracker_conn();
        tc.query_row(
            "SELECT archived_since FROM tracked_epochs WHERE thread_id=?",
            [id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }
    pub fn age(&self, id: &str) {
        let tc = self.tracker_conn();
        let archived_at: Option<i64> = self
            .c
            .query_row("SELECT archived_at FROM threads WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
            .flatten();
        tc.execute(
            "INSERT OR REPLACE INTO tracked_epochs(thread_id,archived_since,codex_archived_at) VALUES(?1,?2,?3)",
            params![id, now() - self.policy.duration() - 100, archived_at],
        )
        .unwrap();
    }
    pub fn run(&mut self, apply: bool) -> anyhow::Result<engine::Report> {
        engine::execute(
            &mut self.c,
            &self.policy,
            &self.store,
            now(),
            if apply {
                engine::ExecutionMode::Run
            } else {
                engine::ExecutionMode::Preview
            },
        )
    }
    pub fn exists(&self, id: &str) -> bool {
        self.c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }
    pub fn pending(&self, id: &str, source: &Path) -> PathBuf {
        use std::os::unix::fs::MetadataExt;
        let m = source.metadata().unwrap();
        let staged = self
            .home
            .join("archived_sessions")
            .join(format!(".codex-retain-pending-{id}"));
        fsutil::atomic_json(&self.store.root.join("pending.json"),&json!({
            "schema":1,"owner":self.policy.owner,"thread_id":id,"artifact":{
                "path":source,"identity":fsutil::Identity::of(&m),"bytes":m.len(),"allocated":m.blocks()*512
            },"staged":staged
        })).unwrap();
        staged
    }
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
