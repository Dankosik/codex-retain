CREATE TABLE codex_retain_owner (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    owner TEXT NOT NULL
);
CREATE TABLE codex_retain_epochs (
    thread_id TEXT PRIMARY KEY,
    archived_since INTEGER NOT NULL,
    codex_archived_at INTEGER
);
CREATE TRIGGER codex_retain_insert AFTER INSERT ON threads BEGIN
    DELETE FROM codex_retain_epochs WHERE thread_id=NEW.id;
    INSERT INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at)
    SELECT NEW.id,unixepoch(),NEW.archived_at WHERE NEW.archived=1;
END;
CREATE TRIGGER codex_retain_update AFTER UPDATE ON threads
WHEN NEW.archived IS NOT OLD.archived OR NEW.archived_at IS NOT OLD.archived_at OR NEW.id IS NOT OLD.id
BEGIN
    DELETE FROM codex_retain_epochs WHERE thread_id=OLD.id OR thread_id=NEW.id;
    INSERT INTO codex_retain_epochs(thread_id,archived_since,codex_archived_at)
    SELECT NEW.id,unixepoch(),NEW.archived_at WHERE NEW.archived=1;
END;
CREATE TRIGGER codex_retain_delete AFTER DELETE ON threads BEGIN
    DELETE FROM codex_retain_epochs WHERE thread_id=OLD.id;
END;
