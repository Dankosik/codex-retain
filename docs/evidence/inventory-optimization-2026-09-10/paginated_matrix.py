#!/usr/bin/env python3
"""Run the existing guarded matrix with independent paginated metadata fixtures.

Only generated profiles are changed. Bodies remain synthetic 4 KiB transcripts;
this measures retention metadata/I/O, not native transcript rendering.
"""
from pathlib import Path
from contextlib import closing
import json
import sqlite3
import sys
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))
import performance_matrix as matrix
import fixture

legacy_transcript = fixture.transcript
legacy_create = fixture.create
legacy_setup = matrix.setup

def transcript(thread_id, created_at, size):
    old = legacy_transcript(thread_id, created_at, size - 3)
    new = old.replace(b'"history_mode":"legacy"', b'"history_mode":"paginated"', 1)
    assert len(new) == size
    return new

def create(*args, **kwargs):
    result = legacy_create(*args, **kwargs)
    root = fixture.owned_root(args[0] if args else kwargs["root"])
    with closing(sqlite3.connect(root / "codex/state_5.sqlite")) as connection:
        connection.execute("UPDATE threads SET history_mode='paginated'")
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    return result

def setup(args):
    if args.cases != ["cleanup"]:
        raise ValueError("This wrapper supports only independent paginated cleanup fixtures")
    root, cases, hyperfine = legacy_setup(args)
    cases = [case for case in cases if case.name.startswith("cleanup-1000-")]
    result = root, cases, hyperfine
    path = root / "environment.json"
    environment = json.loads(path.read_text())
    environment["cases"] = ["cleanup-1000"]
    environment["history_mode"] = "paginated"
    environment["fixture_scope"] = "Independent archived owners, no parent/history references; 4 KiB synthetic bodies"
    fixture.write_json(path, environment)
    return result

fixture.transcript = transcript
fixture.create = create
matrix.setup = setup
matrix.SCRIPT = Path(__file__).resolve()
if __name__ == "__main__":
    raise SystemExit(matrix.main())
