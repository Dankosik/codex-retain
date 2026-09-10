#!/usr/bin/env python3
"""Test constructed organizational relations through native Codex and Retain.

All threads start natively inside a new synthetic profile. Spawn metadata and
SQL edges are explicitly constructed after native shutdown, not native spawns.
No model requests, credentials, real chats, or LaunchAgents are used.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import sqlite3
import time

from codex_integration import AppServer, MARKER, PINNED_SECTION, command, digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", type=Path, required=True)
    parser.add_argument("--retain-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True,
                        help="new nonexistent directory for all synthetic fixture data")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    codex = args.codex_bin.resolve(strict=True)
    retain = args.retain_bin.resolve(strict=True)
    root = args.root.absolute()
    if root.exists() or root.is_symlink():
        parser.error("--root must not exist; never reuse a profile")
    root.parent.resolve(strict=True)
    root.mkdir(mode=0o700)
    root = root.resolve()
    (root / MARKER).write_text(json.dumps({"schema": 1, "purpose": "synthetic relation integration"}) + "\n")
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
        "model_turns_submitted": 0, "launchagents_installed": 0,
        "fixture": "native legacy starts plus constructed organizational parent metadata and SQL spawn edges",
        "test_only_mutations": [
            "after native shutdown, add parent_thread_id, source.subagent.thread_spawn, multi_agent_version v2 and SQL spawn edges",
            "age synthetic capture epochs",
        ],
        "limitations": [
            "Constructed organizational fixture; no native model-driven spawn conformance claim.",
            "Empty legacy histories establish metadata/lifecycle compatibility, not populated child transcript rendering.",
            "Assertions cover main rows, incident spawn edges and owned rollout files; not separate projection caches.",
        ],
        "checks": [],
    }
    app = None

    def check(name, condition, **details):
        if not condition:
            raise AssertionError(name + ": " + json.dumps(details))
        receipt["checks"].append({"name": name, "passed": True, **details})

    def close_app():
        nonlocal app
        if app is not None:
            app.close()
            receipt.setdefault("native_methods", []).extend(app.methods)
            receipt["native_stderr_tail"] = app.stderr_tail.decode(errors="replace")[-1500:]
            app = None

    def db():
        if not (root / MARKER).is_file() or codex_home.parent != root:
            raise RuntimeError("synthetic fixture ownership assertion failed")
        return sqlite3.connect(codex_home / "state_5.sqlite", timeout=1)

    def row(thread_id):
        with db() as conn:
            return conn.execute("SELECT archived,rollout_path FROM threads WHERE id=?", (thread_id,)).fetchone()

    def edge(parent, child):
        with db() as conn:
            return conn.execute("SELECT count(*) FROM thread_spawn_edges WHERE parent_thread_id=? AND child_thread_id=?",
                                (parent, child)).fetchone()[0]

    def age(thread_id):
        with db() as conn:
            changed = conn.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                                   (int(time.time())-31*86400, thread_id)).rowcount
            if changed != 1:
                raise AssertionError("missing synthetic archive epoch")

    def cli(*arguments, allowed=(0,)):
        return json.loads(command([str(retain), "--state-dir", str(state), "--json", *arguments], env, allowed=allowed))

    def archive(thread_id):
        app.request("thread/archive", {"threadId": thread_id})
        return Path(row(thread_id)[1])

    try:
        receipt["codex_version"] = command([str(codex), "--version"], env).strip()
        receipt["retain_version"] = command([str(retain), "--version"], env).strip()
        if receipt["codex_version"] != "codex-cli 0.153.4":
            raise RuntimeError("harness certifies only codex-cli 0.153.4")
        app = AppServer(codex, env, root)
        receipt["app_server_user_agent"] = app.initialize["userAgent"]
        ids = []
        for _ in range(11):
            thread_id = app.request("thread/start", {"cwd": str(root), "historyMode": "legacy", "ephemeral": False})["thread"]["id"]
            app.request("thread/section/move", {"threadId": thread_id, "sectionId": None, "beforeThreadId": None})
            ids.append(thread_id)
        control_parent, control_child, live_parent, live_child, chain_root, chain_middle, chain_leaf, pinned_parent, pinned_child, excluded_parent, excluded_child = ids
        close_app()
        relations = [(control_parent, control_child, 1), (live_parent, live_child, 1),
                     (chain_root, chain_middle, 1), (chain_middle, chain_leaf, 2),
                     (pinned_parent, pinned_child, 1), (excluded_parent, excluded_child, 1)]
        for parent, child, depth in relations:
            path = Path(row(child)[1])
            if not path.resolve().is_relative_to(codex_home):
                raise RuntimeError("synthetic child rollout escaped fixture")
            lines = [json.loads(line) for line in path.read_text().splitlines() if line]
            source = {"subagent": {"thread_spawn": {"parent_thread_id": parent, "depth": depth}}}
            for line in lines:
                if line.get("type") == "session_meta":
                    line["payload"].update(parent_thread_id=parent, source=source, multi_agent_version="v2")
            path.write_text("".join(json.dumps(line) + "\n" for line in lines))
            with db() as conn:
                conn.execute("UPDATE threads SET source=? WHERE id=?", (json.dumps(source), child))
                conn.execute("INSERT INTO thread_spawn_edges(parent_thread_id,child_thread_id,status) VALUES(?,?,'open')", (parent, child))
        app = AppServer(codex, env, root)
        for _, child, _ in relations:
            read = app.request("thread/read", {"threadId": child, "includeTurns": False})
            check("native_read_accepts_constructed_child", read["thread"]["id"] == child)
        control_path = Path(row(control_child)[1])
        parent_path = Path(row(control_parent)[1])
        parent_bytes = parent_path.read_bytes()
        app.request("thread/delete", {"threadId": control_child})
        check("native_delete_child_preserves_parent_and_removes_edge", row(control_child) is None
              and not control_path.exists() and row(control_parent) is not None
              and parent_path.read_bytes() == parent_bytes and edge(control_parent, control_child) == 0)
        deadline = time.monotonic() + 10
        while True:
            with db() as conn:
                progress = conn.execute("SELECT status FROM backfill_state WHERE id=1").fetchone()
            if progress and progress[0] == "complete":
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("native backfill did not complete")
            time.sleep(0.05)
        cli("enable", "--days", "30", "--codex-home", str(codex_home),
            "--codex-bin", str(codex), "--no-schedule", "--yes")
        child_path = archive(live_child)
        child_bytes = child_path.read_bytes()
        age(live_child)
        app.request("thread/resume", {"threadId": live_parent, "excludeTurns": True})
        run = cli("run", allowed=(0, 3))
        entry = next(item for item in run["entries"] if item["id"] == live_child)
        check("loaded_parent_writer_blocks_child_cleanup", run["deleted"] == 0
              and entry["reason"] == "changed_busy_or_error" and "lock" in (entry.get("detail") or "").lower()
              and row(live_child) is not None and child_path.read_bytes() == child_bytes,
              reason=entry["reason"], detail=entry.get("detail"))
        parent_path = archive(live_parent)
        app.request("thread/section/move", {"threadId": live_parent, "sectionId": PINNED_SECTION, "beforeThreadId": None})
        parent_bytes = parent_path.read_bytes()
        run = cli("run", allowed=(0, 3))
        check("child_cleanup_preserves_pinned_parent_and_removes_edge", run["deleted"] == 1
              and row(live_child) is None and not child_path.exists()
              and row(live_parent) is not None and parent_path.read_bytes() == parent_bytes
              and edge(live_parent, live_child) == 0, deleted=run["deleted"])
        app.request("thread/unarchive", {"threadId": live_parent})
        resumed = app.request("thread/resume", {"threadId": live_parent, "excludeTurns": True})
        readable = app.request("thread/read", {"threadId": live_parent, "includeTurns": False})
        check("native_parent_resume_and_read_after_child_cleanup", resumed["thread"]["id"] == live_parent
              and readable["thread"]["id"] == live_parent)
        chain = [chain_root, chain_middle, chain_leaf]
        protected = [pinned_parent, pinned_child, excluded_parent, excluded_child]
        # Native root archive cascades through organizational descendants.
        for thread_id in [chain_root, pinned_parent, excluded_parent]:
            archive(thread_id)
        check("native_root_archive_cascades_to_constructed_descendants",
              all(row(thread_id)[0] == 1 for thread_id in chain + protected))
        paths = {thread_id: Path(row(thread_id)[1]) for thread_id in chain + protected}
        for thread_id in chain + protected:
            age(thread_id)
        app.request("thread/section/move", {"threadId": pinned_child, "sectionId": PINNED_SECTION, "beforeThreadId": None})
        cli("exclude", excluded_child)
        run = cli("run", allowed=(0, 3))
        deleted_ids = [entry["id"] for entry in run["entries"] if entry["reason"] == "deleted"]
        check("due_three_level_chain_deleted_in_one_run", run["deleted"] == 3
              and set(deleted_ids) == set(chain)
              and all(row(thread_id) is None and not paths[thread_id].exists() for thread_id in chain)
              and edge(chain_root, chain_middle) == 0 and edge(chain_middle, chain_leaf) == 0,
              deleted=run["deleted"], deleted_ids=deleted_ids)
        check("pinned_and_excluded_children_preserve_ancestors", all(row(thread_id) is not None
              and paths[thread_id].exists() for thread_id in protected)
              and edge(pinned_parent, pinned_child) == 1 and edge(excluded_parent, excluded_child) == 1,
              entries=[entry for entry in run["entries"] if entry["id"] in protected])
        check("related_cleanup_idempotent", cli("run", allowed=(0, 3))["deleted"] == 0)
        cli("disable")
        check("no_launchagent_installed", not (home / "Library/LaunchAgents").exists())
        receipt["passed"] = True
    except Exception as error:
        receipt["error"] = str(error)
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
