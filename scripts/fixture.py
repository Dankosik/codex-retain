#!/usr/bin/env python3
"""Create synthetic Codex 0.153.4 data in a new, explicitly named directory."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import sys
import time
import uuid


REPOSITORY = Path(__file__).resolve().parent.parent
MARKER = ".codex-retain-fixture.json"
DAY = 86400
NAMESPACE = uuid.UUID("aebaa89f-86b1-4e31-83bb-73f15d5e94ae")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def no_symlinks(path):
    path = Path(os.path.abspath(path))
    for candidate in [*reversed(path.parents), path]:
        if candidate.is_symlink():
            raise ValueError("refusing a symlink in fixture path: " + str(candidate))
    return path


def owned_root(path):
    root = no_symlinks(path)
    marker = root / MARKER
    if not marker.is_file() or marker.is_symlink():
        raise ValueError("not a marked synthetic fixture: " + str(root))
    data = json.loads(marker.read_text(encoding="utf-8"))
    if data.get("kind") != "codex-retain-synthetic-fixture" or data.get("root") != str(root):
        raise ValueError("fixture marker identity mismatch: " + str(root))
    for directory, subdirs, files in os.walk(root, followlinks=False):
        for name in [*subdirs, *files]:
            entry = Path(directory) / name
            if entry.is_symlink():
                # The real supported Codex launcher creates these executable
                # aliases even for --version. They are outside every data/state
                # path used by cleanup. Never follow them here; shutil.rmtree
                # unlinks them during a marked fixture reset.
                if (entry.parent.parent == root / "codex/tmp/arg0"
                        and entry.parent.name.startswith("codex-arg0")
                        and entry.name in {"applypatch", "apply_patch", "codex-execve-wrapper"}):
                    continue
                raise ValueError("refusing a link inside fixture: " + str(entry))
            if entry.is_file() and entry.stat().st_nlink != 1:
                raise ValueError("refusing a multiply linked fixture file: " + str(entry))
    return root


def remove_owned(path):
    """Only generated, marker-identified trees are eligible for fixture reset."""
    root = owned_root(path)
    shutil.rmtree(root)


def schema_statements(text):
    statements, pending = [], ""
    for line in text.splitlines(keepends=True):
        pending += line
        if sqlite3.complete_statement(pending):
            statements.append(pending.strip())
            pending = ""
    if pending.strip():
        raise ValueError("incomplete fixture schema SQL")
    priorities = {"TABLE": 0, "INDEX": 1, "TRIGGER": 2}
    for statement in statements:
        words = statement.split()
        if len(words) < 3 or words[0] != "CREATE" or words[1] not in priorities:
            raise ValueError("unexpected statement in fixture schema")
    return sorted(statements, key=lambda statement: priorities[statement.split()[1]])


def iso(timestamp):
    return datetime.fromtimestamp(timestamp, timezone.utc).isoformat().replace("+00:00", "Z")


def transcript(thread_id, created_at, size):
    header = {
        "timestamp": iso(created_at), "type": "session_meta",
        "payload": {
            "id": thread_id, "cwd": "/fixture", "timestamp": iso(created_at),
            "originator": "codex_cli_rs", "cli_version": "0.153.4",
            "source": "cli", "history_mode": "legacy",
        },
    }
    message = {"timestamp": iso(created_at), "type": "event_msg",
               "payload": {"type": "user_message", "message": "Synthetic benchmark conversation."}}
    padding = {"timestamp": iso(created_at), "type": "event_msg",
               "payload": {"type": "agent_message", "message": ""}}
    lines = [json.dumps(item, separators=(",", ":")) + "\n" for item in (header, message, padding)]
    remaining = size - len("".join(lines).encode("utf-8"))
    if remaining < 0:
        raise ValueError("rollout size is too small for synthetic metadata")
    padding["payload"]["message"] = "x" * remaining
    lines[-1] = json.dumps(padding, separators=(",", ":")) + "\n"
    return "".join(lines).encode("utf-8")


def create(root, count=1000, rollout_bytes=4096, now=None):
    root = no_symlinks(root)
    if root.exists():
        raise ValueError("fixture destination must not exist: " + str(root))
    if not 0 <= count <= 100000 or not 1024 <= rollout_bytes <= 64 * 1024 * 1024:
        raise ValueError("count must be 0..100000 and rollout bytes 1024..67108864")
    if count * rollout_bytes > 2 * 1024**3:
        raise ValueError("fixture exceeds the 2 GiB per-case guardrail")
    now = int(time.time()) if now is None else int(now)
    created_at, archived_at = now - 100 * DAY, now - 40 * DAY
    root.mkdir(parents=True, mode=0o700)
    write_json(root / MARKER, {"kind": "codex-retain-synthetic-fixture", "schema": 1,
                              "root": str(root), "nonce": str(uuid.uuid4())})
    codex_home = root / "codex"
    for directory in (codex_home / "sessions", codex_home / "archived_sessions", root / "home", root / "state"):
        directory.mkdir(parents=True, mode=0o700)
    schema_path = REPOSITORY / "compatibility" / "fixture-schema.sql"
    migrations_path = REPOSITORY / "compatibility" / "migrations.json"
    records = []
    with sqlite3.connect(codex_home / "state_5.sqlite") as connection:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("PRAGMA foreign_keys=ON")
        for statement in schema_statements(schema_path.read_text(encoding="utf-8")):
            connection.execute(statement)
        migrations = json.loads(migrations_path.read_text(encoding="utf-8"))
        connection.executemany(
            "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            [(row["version"], row["description"], row["installed_on"], row["success"],
              bytes.fromhex(row["checksum"]), row["execution_time"]) for row in migrations],
        )
        connection.execute(
            "INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at) "
            "VALUES (1, 'complete', NULL, ?, ?)", (now, now),
        )
        with (codex_home / "session_index.jsonl").open("w", encoding="utf-8") as index:
            for number in range(count):
                thread_id = str(uuid.uuid5(NAMESPACE, str(number)))
                name = "rollout-2026-01-01T00-00-00-" + thread_id + ".jsonl"
                path = codex_home / "archived_sessions" / name
                data = transcript(thread_id, created_at, rollout_bytes)
                path.write_bytes(data)
                os.utime(path, (created_at, created_at))
                connection.execute(
                    "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, "
                    "cwd, title, sandbox_policy, approval_mode, archived, archived_at, cli_version, "
                    "first_user_message, preview, history_mode, has_user_event) "
                    "VALUES (?, ?, ?, ?, 'cli', 'openai', '/fixture', ?, 'read-only', 'never', 1, ?, "
                    "'0.153.4', 'Synthetic benchmark conversation.', 'Synthetic benchmark conversation.', 'legacy', 1)",
                    (thread_id, str(path), created_at, created_at, "Fixture " + str(number), archived_at),
                )
                index.write(json.dumps({"id": thread_id, "thread_name": "Fixture " + str(number),
                                        "updated_at": iso(created_at)}) + "\n")
                records.append({"id": thread_id, "path": str(path.relative_to(root)),
                                "sha256": hashlib.sha256(data).hexdigest()})
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        if connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise ValueError("generated fixture failed SQLite integrity check")
    (codex_home / "history.jsonl").write_text("", encoding="utf-8")
    manifest = {"schema": 1, "generated_at": now, "archived_at": archived_at,
                "created_at": created_at, "count": count, "rollout_bytes": rollout_bytes,
                "logical_rollout_bytes": count * rollout_bytes,
                "schema_sha256": hashlib.sha256(schema_path.read_bytes()).hexdigest(),
                "migrations_sha256": hashlib.sha256(migrations_path.read_bytes()).hexdigest(),
                "records": records}
    write_json(root / "manifest.json", manifest)
    return manifest


def verify(root, expect_removed=False, expect_thread_rows=None):
    root = owned_root(root)
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    expected = {record["id"]: record for record in manifest["records"]}
    for record in expected.values():
        path = no_symlinks(root / record["path"])
        if not path.is_relative_to(root / "codex" / "archived_sessions"):
            raise ValueError("manifest path is outside synthetic archive")
        if expect_removed:
            if path.exists():
                raise ValueError("expected rollout was not removed: " + record["id"])
        elif not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != record["sha256"]:
            raise ValueError("fixture rollout content changed: " + record["id"])
    files = list((root / "codex" / "archived_sessions").glob("*.jsonl"))
    if len(files) != (0 if expect_removed else len(expected)):
        raise ValueError("unexpected archive file set")
    if any((root / "codex" / "sessions").iterdir()):
        raise ValueError("throughput fixture must not have active sessions")
    database = root / "codex" / "state_5.sqlite"
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
        if connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise ValueError("fixture database integrity check failed")
        rows = connection.execute("SELECT id, rollout_path, archived FROM threads").fetchall()
    expected_count = manifest["count"] if expect_thread_rows is None else expect_thread_rows
    if len(rows) != expected_count:
        raise ValueError(f"expected {expected_count} database rows, got {len(rows)}")
    if expected_count and {row[0] for row in rows} != expected.keys():
        raise ValueError("unexpected surviving thread IDs")
    for thread_id, rollout_path, archived in rows:
        if Path(rollout_path) != root / expected[thread_id]["path"] or archived != 1:
            raise ValueError("thread row points outside the original synthetic archive")
    return {"verified": True, "count": manifest["count"], "removed": expect_removed,
            "remaining_thread_rows": len(rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    generate = sub.add_parser("create", help="create a new marked fixture; never overwrite an existing path")
    generate.add_argument("--root", required=True, type=Path)
    generate.add_argument("--count", type=int, default=1000)
    generate.add_argument("--rollout-bytes", type=int, default=4096)
    generate.add_argument("--now", type=int, help="fixed Unix timestamp for reproducibility")
    check = sub.add_parser("verify", help="verify an untouched synthetic fixture")
    check.add_argument("--root", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "create":
            result = create(args.root, args.count, args.rollout_bytes, args.now)
            print(json.dumps({key: value for key, value in result.items() if key != "records"}))
        else:
            print(json.dumps(verify(args.root)))
    except (OSError, ValueError, sqlite3.Error, KeyError) as error:
        print("Fixture error: " + str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
