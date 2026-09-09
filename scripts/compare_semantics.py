#!/usr/bin/env python3
"""Compare the requested archive-retention contract on new synthetic profiles."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

import fixture

REVISION = "32737c7cc68a74a63a21e5b8a403e92f4f1398e6"
CASES = ["old_chat_archived_today", "old_active_chat", "restored_then_rearchived",
         "unknown_archive_time", "native_pinned", "explicitly_excluded", "due_archive"]


def run(command, env=None):
    result = subprocess.run(command, env=env, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise ValueError(f"command failed ({result.returncode}): {result.stderr[-2000:]}")
    return result.stdout


def compare(args):
    root = fixture.no_symlinks(args.root)
    if root.exists():
        raise ValueError("comparison root must not exist")
    retain = Path(args.retain).resolve(strict=True)
    codex = Path(args.codex_bin).resolve(strict=True)
    janitor = Path(args.janitor).resolve(strict=True)
    node = shutil.which(args.node)
    if not node:
        raise ValueError("Node executable unavailable")
    if run(["git", "-C", str(janitor), "rev-parse", "HEAD"]).strip() != REVISION:
        raise ValueError("unexpected Janitor revision")
    if run(["git", "-C", str(janitor), "status", "--porcelain", "--untracked-files=no"]).strip():
        raise ValueError("Janitor source must be pristine")
    root.mkdir(parents=True, mode=0o700)
    reports = {}
    for tool in ("retain", "janitor"):
        case_root = root / tool
        manifest = fixture.create(case_root, len(CASES))
        fixture.owned_root(case_root)
        home, state = case_root / "codex", case_root / "state"
        env = {"HOME": str(case_root / "home"), "CODEX_HOME": str(home),
               "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "NO_COLOR": "1"}
        ids = [r["id"] for r in manifest["records"]]
        paths = [case_root / r["path"] for r in manifest["records"]]
        prefix = [str(retain), "--state-dir", str(state), "--json"]
        if tool == "retain":
            run([*prefix, "enable", "--codex-home", str(home), "--codex-bin", str(codex),
                 "--no-schedule", "--days", "30", "--yes"], env)
        now = int(time.time())
        with sqlite3.connect(home / "state_5.sqlite") as db:
            # Test-only time setup. Real users never need to edit capture epochs.
            if tool == "retain":
                db.execute("UPDATE codex_retain_epochs SET archived_since=?", (now - 40 * fixture.DAY,))
            db.execute("UPDATE threads SET archived_at=? WHERE id=?", (now, ids[0]))
            active_path = home / "sessions" / paths[1].name
            paths[1].rename(active_path)
            paths[1] = active_path
            db.execute("UPDATE threads SET archived=0,archived_at=NULL,rollout_path=? WHERE id=?",
                       (str(active_path), ids[1]))
            db.execute("UPDATE threads SET archived=0,archived_at=NULL WHERE id=?", (ids[2],))
            db.execute("UPDATE threads SET archived=1,archived_at=? WHERE id=?", (now, ids[2]))
            db.execute("UPDATE threads SET archived_at=NULL WHERE id=?", (ids[3],))
            db.execute("INSERT OR IGNORE INTO thread_sections(id,name) VALUES(?, 'Pinned')",
                       ("01984de2-8f74-7c91-a3b2-5c5e937cf318",))
            db.execute("UPDATE threads SET thread_section_id=?,is_pinned=0 WHERE id=?",
                       ("01984de2-8f74-7c91-a3b2-5c5e937cf318", ids[4]))
            if tool == "retain":
                db.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                           (now - 40 * fixture.DAY, ids[3]))
        if tool == "retain":
            run([*prefix, "exclude", ids[5]], env)
            output = json.loads(run([*prefix, "run"], env))
        else:
            output = run([node, str(janitor / "dist/cli.js"), "clean", "--codex-home", str(home),
                          "--retention-days", "30", "--confirm", "--mode", "delete"], env)
        outcomes = [{"scenario": name, "expected_deleted_by_requested_contract": name == "due_archive",
                     "actually_deleted": not path.exists(), "thread_id": thread_id}
                    for name, path, thread_id in zip(CASES, paths, ids)]
        with sqlite3.connect((home / "state_5.sqlite").as_uri() + "?mode=ro", uri=True) as db:
            assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
            remaining = db.execute("SELECT count(*) FROM threads").fetchone()[0]
        reports[tool] = {"cases": outcomes, "remaining_sqlite_threads": remaining, "output": output}
        if tool == "retain":
            assert all(r["actually_deleted"] == r["expected_deleted_by_requested_contract"] for r in outcomes)
            run([*prefix, "disable"], env)
    return {"schema": 1, "retention_days": 30, "fixture_kind": "synthetic only",
            "retain_sha256": hashlib.sha256(retain.read_bytes()).hexdigest(),
            "janitor_revision": REVISION, "reports": reports,
            "interpretation": "Agreement with this product's requested archive-retention contract; alternatives have different advertised scopes. No timing claim.",
            "test_only_mutations": "Backdate generated capture epochs; native-shaped SQLite transitions and pin update; no real profiles."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--retain", required=True)
    parser.add_argument("--codex-bin", required=True)
    parser.add_argument("--janitor", required=True)
    parser.add_argument("--node", default="node")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = compare(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"completed": True, "evidence": str(args.output)}))


if __name__ == "__main__":
    main()
