use codex_retain::{
    config::{Policy, Store},
    database, engine, fsutil,
};
use rusqlite::{Connection, params};
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
        database::install_capture(&mut c, &owner).unwrap();
        let policy = Policy {
            schema: 1,
            codex_home: home.clone(),
            codex_bin: PathBuf::from("/synthetic/codex"),
            database: database::identity(&home).unwrap(),
            retention_days: 30,
            enabled: true,
            paused: false,
            automatic: false,
            enabled_at: now() - 10,
            owner,
            exclusions: BTreeSet::new(),
        };
        Self {
            temp,
            home,
            c,
            policy,
            store,
        }
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
        self.c.execute("INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at) VALUES(?,?,1,1,'cli','fixture',?,'fixture','{}','never',?,?)",
            params![id,path.to_str().unwrap(),self.home.to_str().unwrap(),archived,if archived==0{None}else{Some(1i64)}]).unwrap();
        id
    }
    pub fn epoch(&self, id: &str) -> Option<i64> {
        use rusqlite::OptionalExtension;
        self.c
            .query_row(
                "SELECT archived_since FROM codex_retain_epochs WHERE thread_id=?",
                [id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }
    pub fn age(&self, id: &str) {
        self.c
            .execute(
                "UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                params![now() - self.policy.duration() - 100, id],
            )
            .unwrap();
    }
    pub fn run(&mut self, apply: bool) -> anyhow::Result<engine::Report> {
        engine::execute(&mut self.c, &self.policy, &self.store, now(), apply)
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
