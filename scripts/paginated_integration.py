#!/usr/bin/env python3
"""Native paginated conformance in a new synthetic profile; no model calls."""

import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import platform
import sqlite3
import time

from codex_integration import AppServer, MARKER, command, digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", type=Path, required=True)
    parser.add_argument("--retain-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True,
                        help="new nonexistent directory for the complete synthetic fixture")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    codex = args.codex_bin.resolve(strict=True)
    retain = args.retain_bin.resolve(strict=True)
    root = args.root.absolute()
    if root.exists() or root.is_symlink():
        parser.error("--root must not exist; profiles cannot be reused")
    root.parent.resolve(strict=True)
    root.mkdir(mode=0o700)
    root = root.resolve()
    (root / MARKER).write_text(json.dumps({"schema": 1, "purpose": "synthetic paginated integration"}) + "\n")
    home, codex_home, state = root / "user", root / "codex", root / "state"
    for path in (home, codex_home):
        path.mkdir(mode=0o700)
    env = {name: os.environ[name] for name in ("PATH", "TMPDIR", "LANG", "LC_ALL", "TERM")
           if name in os.environ}
    env.update(HOME=str(home), CODEX_HOME=str(codex_home),
               XDG_CONFIG_HOME=str(home / ".config"),
               HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1")
    (codex_home / "config.toml").write_text('''
model_provider = "fixture"
model = "fixture-model"
[model_providers.fixture]
name = "Synthetic local fixture"
base_url = "http://127.0.0.1:1/v1"
wire_api = "responses"
[analytics]
enabled = false
[features]
plugins = false
recommended_plugins = false
local_thread_store_compression = false
background_paginated_rollout_migration = false
''')
    receipt = {
        "schema": 1, "passed": False, "platform": platform.platform(),
        "retain_sha256": digest(retain), "codex_launcher_sha256": digest(codex),
        "fixture": "new isolated HOME/CODEX_HOME; native paginated starts, archive and fork",
        "model_turns_submitted": 0, "launchagents_installed": 0,
        "checks": [], "test_only_mutations": ["age synthetic capture epochs"],
        "limitations": [
            "No model requests; populated history is a canonical synthetic fixture.",
            "Deletion assertions cover owned rollout files and main thread rows, not separate projection caches.",
        ],
    }
    app = None

    def check(name, condition, **details):
        if not condition:
            raise AssertionError(name + ": " + json.dumps(details))
        receipt["checks"].append({"name": name, "passed": True, **details})

    def cli(*arguments, allowed=(0,)):
        return json.loads(command([str(retain), "--state-dir", str(state), "--json", *arguments], env, allowed=allowed))

    def db():
        if not (root / MARKER).is_file() or codex_home.parent != root:
            raise RuntimeError("synthetic fixture ownership assertion failed")
        return sqlite3.connect(codex_home / "state_5.sqlite", timeout=1)

    def row(thread_id):
        with db() as conn:
            return conn.execute("SELECT archived,rollout_path,history_mode FROM threads WHERE id=?", (thread_id,)).fetchone()

    def age(thread_id):
        with db() as conn:
            changed = conn.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                                   (int(time.time())-31*86400, thread_id)).rowcount
            if changed != 1:
                raise AssertionError("synthetic archive epoch missing")

    def persist(thread_id):
        app.request("thread/section/move", {"threadId": thread_id, "sectionId": None, "beforeThreadId": None})

    def close_app():
        nonlocal app
        if app is not None:
            app.close()
            receipt.setdefault("native_methods", []).extend(app.methods)
            receipt["native_stderr_tail"] = app.stderr_tail.decode(errors="replace")[-1500:].replace(str(root), "$FIXTURE")
            app = None

    @contextmanager
    def writer_lock(thread_id):
        directory = codex_home / "thread-writer-locks"
        directory.mkdir(exist_ok=True, mode=0o700)
        with (directory / ".coordination.lock").open("a+b") as coordination:
            fcntl.flock(coordination, fcntl.LOCK_EX)
            writer = (directory / (thread_id + ".lock")).open("a+b")
            try:
                fcntl.flock(writer, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(coordination, fcntl.LOCK_UN)
                yield
            finally:
                fcntl.flock(coordination, fcntl.LOCK_EX)
                writer.close()
                fcntl.flock(coordination, fcntl.LOCK_UN)

    try:
        receipt["codex_version"] = command([str(codex), "--version"], env).strip()
        receipt["retain_version"] = command([str(retain), "--version"], env).strip()
        if receipt["codex_version"] != "codex-cli 0.153.4":
            raise RuntimeError("this harness certifies only codex-cli 0.153.4")
        app = AppServer(codex, env, root)
        receipt["app_server_user_agent"] = app.initialize["userAgent"]
        standalone = app.request("thread/start", {"cwd": str(root), "historyMode": "paginated", "ephemeral": False})["thread"]["id"]
        persist(standalone)
        app.request("thread/archive", {"threadId": standalone})
        standalone_path = Path(row(standalone)[1])
        check("native_paginated_archive_persisted", row(standalone)[2] == "paginated" and standalone_path.is_file())
        deadline = time.monotonic() + 10
        while True:
            with db() as conn:
                progress = conn.execute("SELECT status FROM backfill_state WHERE id=1").fetchone()
            if progress and progress[0] == "complete":
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("native backfill did not complete")
            time.sleep(0.05)
        enabled = cli("enable", "--days", "30", "--codex-home", str(codex_home),
                      "--codex-bin", str(codex), "--no-schedule", "--yes")
        preview = cli("preview")
        check("native_paginated_enable_preserves_full_grace", enabled["existing_archives_given_grace"] == 1 and preview["eligible"] == 0)
        age(standalone)
        run = cli("run")
        check("native_paginated_standalone_deleted", run["deleted"] == 1 and row(standalone) is None and not standalone_path.exists(), deleted=run["deleted"])
        source = app.request("thread/start", {"cwd": str(root), "historyMode": "paginated", "ephemeral": False})["thread"]["id"]
        persist(source)
        # Empty histories intentionally produce no history_base. Seed one
        # complete canonical synthetic turn only after native writers shut down;
        # native prepare_fork then projects it and creates the actual reference.
        close_app()
        source_path = Path(row(source)[1])
        if not source_path.resolve().is_relative_to(codex_home):
            raise RuntimeError("synthetic rollout escaped fixture")
        source_lines = [json.loads(line) for line in source_path.read_text().splitlines() if line]
        # Exact 0.153.4 EventMsg/TurnItem wire shapes, matching upstream
        # thread_history_materialization_tests.rs turn_started/completed_item/
        # turn_completed helpers. Canonical ItemCompleted events hydrate turns.
        seed_text = "synthetic fixture seed; no model request"
        seed_answer = "synthetic fixture answer; no provider was contacted"
        turn_id = "synthetic-turn-1"
        payloads = [
            {"type": "task_started", "turn_id": turn_id, "started_at": 10,
             "model_context_window": None},
            {"type": "item_completed", "thread_id": source, "turn_id": turn_id,
             "item": {"type": "UserMessage", "id": "synthetic-user-1",
                      "content": [{"type": "text", "text": seed_text, "text_elements": []}]},
             "started_at_ms": 10000, "completed_at_ms": 10001},
            {"type": "item_completed", "thread_id": source, "turn_id": turn_id,
             "item": {"type": "AgentMessage", "id": "synthetic-agent-1",
                      "content": [{"type": "Text", "text": seed_answer}]},
             "started_at_ms": 10001, "completed_at_ms": 19999},
            {"type": "task_complete", "turn_id": turn_id, "last_agent_message": seed_answer,
             "started_at": 10, "completed_at": 20, "duration_ms": 10000},
        ]
        next_ordinal = max(item["ordinal"] for item in source_lines) + 1
        with source_path.open("a") as stream:
            for offset, payload in enumerate(payloads):
                stream.write(json.dumps({"timestamp": "2026-09-10T00:00:00Z",
                                         "ordinal": next_ordinal + offset,
                                         "type": "event_msg", "payload": payload}) + "\n")
        receipt["test_only_mutations"].append("append synthetic TaskStarted, canonical UserMessage and AgentMessage ItemCompleted, TaskComplete events after native writer shutdown")
        app = AppServer(codex, env, root)
        child = app.request("thread/fork", {"threadId": source, "excludeTurns": True})["thread"]["id"]
        persist(child)
        child_path = Path(row(child)[1])
        headers = [json.loads(line) for line in child_path.read_text().splitlines() if line]
        bases = [entry["payload"].get("history_base") for entry in headers if entry.get("type") == "session_meta"]
        check("native_fork_persists_shared_history_base", any(base and base["thread_id"] == source for base in bases), history_bases=bases)
        inherited = app.request("thread/read", {"threadId": child, "includeTurns": True})["thread"]["turns"]
        check("native_fork_hydrates_complete_inherited_turn", len(inherited) >= 1
              and seed_text in json.dumps(inherited) and seed_answer in json.dumps(inherited),
              hydrated_turn_count=len(inherited))
        original_source_path = Path(row(source)[1])
        app.request("thread/resume", {"threadId": source, "excludeTurns": True})
        app.request("thread/revert", {"threadId": source, "beforeTurnId": turn_id})
        replacement_source_path = Path(row(source)[1])
        check("native_revert_creates_another_owned_rollout", replacement_source_path != original_source_path
              and original_source_path.exists() and replacement_source_path.exists())
        app.request("thread/archive", {"threadId": source})
        source_path = Path(row(source)[1])
        source_bytes = source_path.read_bytes()
        archived_original = codex_home / "archived_sessions" / original_source_path.name
        original_bytes = archived_original.read_bytes()
        age(source)
        with writer_lock(source):
            try:
                app.request("thread/unarchive", {"threadId": source})
            except RuntimeError as error:
                check("external_writer_lock_blocks_native_unarchive", "active writer" in str(error).lower()
                      and row(source)[0] == 1 and source_path.read_bytes() == source_bytes,
                      error=str(error))
            else:
                raise AssertionError("native unarchive ignored external writer lock")
        try:
            app.request("thread/fork", {"threadId": source, "excludeTurns": True})
        except RuntimeError as error:
            check("native_fork_rejects_archived_source", "is archived" in str(error), error=str(error))
        else:
            raise AssertionError("native fork unexpectedly accepted archived source")
        run = cli("run", allowed=(0, 3))
        entry = next(item for item in run["entries"] if item["id"] == source)
        check("active_fork_preserves_archived_source", run["deleted"] == 0 and row(source) is not None and source_path.read_bytes() == source_bytes
              and archived_original.read_bytes() == original_bytes and entry["reason"] == "referenced_history",
              reason=entry["reason"], detail=entry.get("detail"))
        readable = app.request("thread/read", {"threadId": child, "includeTurns": True})
        check("native_child_read_survives_cleanup", readable["thread"]["id"] == child
              and readable["thread"]["turns"] == inherited,
              hydrated_turn_count=len(readable["thread"]["turns"]))
        app.request("thread/archive", {"threadId": child})
        child_path = Path(row(child)[1])
        age(child)
        # Dependency ordering removes the leaf, then rechecks and removes the
        # source's two owned segments within the same cleanup invocation.
        run = cli("run", allowed=(0, 3))
        check("archived_fork_and_released_source_deleted_in_one_run", run["deleted"] == 2 and row(child) is None
              and not child_path.exists() and row(source) is None and not source_path.exists()
              and not archived_original.exists(), deleted=run["deleted"], owned_rollouts_removed=3)
        check("native_paginated_cleanup_idempotent", cli("run")["deleted"] == 0)
        cli("disable")
        check("no_launchagent_installed", not (home / "Library/LaunchAgents").exists())
        receipt["passed"] = True
    except Exception as error:
        receipt["error"] = str(error).replace(str(root), "$FIXTURE")
    finally:
        close_app()
        receipt["finished_at_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        serialized = json.dumps(receipt, indent=2).replace(str(root), "$FIXTURE") + "\n"
        (root / "receipt.json").write_text(serialized)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(serialized)
    print(serialized, end="")
    return 0 if receipt["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
