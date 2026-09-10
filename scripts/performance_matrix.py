#!/usr/bin/env python3
"""Compare existing Retain binaries on explicitly owned synthetic edge workloads.

No builds, real profile access, scheduler registration, or dependency installation.
Native executables are timed directly; preparation, verification, and optional
sampled parent RSS observations are outside Hyperfine's timing samples.
"""

import argparse
from contextlib import closing, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import sqlite3
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
import benchmark  # noqa: E402
import fixture  # noqa: E402


SCRIPT = Path(__file__).resolve()
SCENARIOS = {"mixed-none", "mixed-one", "summary", "one-busy", "global-busy", "all-busy",
             "headers-plain", "headers-zstd", "cleanup-1000", "cleanup-10000"}
PROTECTED_FILES = ("codex/session_index.jsonl", "codex/history.jsonl", "state/policy.json")
GLOBAL_STOP_WARNING = (
    "shared cleanup prerequisites are unavailable; stopped this run instead of retrying every archive")


def digest(path):
    with Path(path).open("rb") as stream:
        # Most fixtures contain tiny files. Avoid allocating file_digest's
        # large transfer buffer for each one while still streaming binaries.
        if os.fstat(stream.fileno()).st_size <= 1024 * 1024:
            return hashlib.sha256(stream.read()).hexdigest()
        return hashlib.file_digest(stream, "sha256").hexdigest()


def sqlite_value_digest(value):
    encoded = json.dumps(value, separators=(",", ":"),
                         default=lambda blob: {"blob": blob.hex()}).encode()
    return hashlib.sha256(encoded).hexdigest()


def database_snapshot(connection):
    """Retain compact fingerprints of complete rows, and exact capture values."""
    cursor = connection.execute("SELECT * FROM threads ORDER BY id")
    columns = [column[0] for column in cursor.description]
    identity_column = columns.index("id")
    threads = {row[identity_column]: sqlite_value_digest(row) for row in cursor}
    epochs = {ident: [archived_since, archived_at] for ident, archived_since, archived_at in
              connection.execute("SELECT thread_id,archived_since,codex_archived_at FROM codex_retain_epochs")}
    schema = connection.execute(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name").fetchall()
    protected = {"schema_sha256": sqlite_value_digest(schema), "tables": {}}
    for kind, name, _, _ in schema:
        if kind != "table" or name in {"threads", "codex_retain_epochs"}:
            continue
        quoted = '"' + name.replace('"', '""') + '"'
        cursor = connection.execute("SELECT * FROM " + quoted + " LIMIT 0")
        names = [column[0] for column in cursor.description]
        order = ",".join(str(index + 1) for index in range(len(names)))
        rows = connection.execute("SELECT * FROM " + quoted + " ORDER BY " + order).fetchall()
        protected["tables"][name] = sqlite_value_digest([names, rows])
    return {"thread_columns": columns, "threads": threads, "epochs": epochs, "protected": protected}


def read_case(path):
    path = fixture.no_symlinks(path)
    case = json.loads(path.read_text(encoding="utf-8"))
    root = benchmark.benchmark_root(case["benchmark_root"])
    fixture_key = case.get("fixture_key", path.stem)
    if (path.parent != root / "cases" or not re.fullmatch(r"[a-z0-9-]+\.json", path.name)
            or case["scenario"] not in SCENARIOS
            or ("fixture_key" in case and fixture_key != case["scenario"])
            or Path(case["fixture_root"]) != root / "fixtures" / fixture_key):
        raise ValueError("matrix case descriptor does not identify its owned fixture")
    fixture.no_symlinks(case["fixture_root"])
    return case


def requires_reset(case):
    return case["scenario"].endswith("busy") or case["scenario"].startswith("cleanup-")


def command(case):
    result = [case["binary"], "--state-dir", str(Path(case["fixture_root"]) / "state"), "--json"]
    result += ["run", "--scheduled"] if case["scenario"] == "summary" else [
        "run" if requires_reset(case) else "preview"]
    return result


def guard_policy(case):
    """Check native operation authority without scanning every rollout per sample."""
    root = fixture.no_symlinks(case["fixture_root"])
    marker_path = fixture.no_symlinks(root / fixture.MARKER)
    marker = json.loads(marker_path.read_text(encoding="utf-8"))
    if marker.get("kind") != "codex-retain-synthetic-fixture" or marker.get("root") != str(root):
        raise ValueError("native invocation requires this exact marked synthetic fixture")
    home = fixture.no_symlinks(root / "codex")
    state = fixture.no_symlinks(root / "state")
    policy_path = fixture.no_symlinks(state / "policy.json")
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    if (policy.get("codex_home") != str(home) or policy.get("owner") != str(state)
            or policy.get("codex_bin") != case["codex_bin"]):
        raise ValueError("native policy must name exactly the synthetic home, state, and Codex binary")
    if (policy.get("enabled") is not True or policy.get("automatic") is not False
            or policy.get("paused") is not False or policy.get("retention_days") != 30):
        raise ValueError("native policy differs from the unscheduled synthetic retention contract")
    metadata = fixture.no_symlinks(home / "state_5.sqlite").stat()
    if policy.get("database") != {"device": metadata.st_dev, "inode": metadata.st_ino}:
        raise ValueError("native policy identifies a different database inode")
    expected_hash = case.get("policy_sha256")
    if expected_hash is None:
        manifest_path = fixture.no_symlinks(root / "matrix-manifest.json")
        expected_hash = json.loads(manifest_path.read_text(encoding="utf-8"))["protected"]["state/policy.json"]
    if digest(policy_path) != expected_hash:
        raise ValueError("native policy bytes changed since fixture preparation")


def read_manifest(root):
    # Callers first validate the entire owned tree. Exact derived locations
    # below reject path escapes without re-statting every ancestor per row.
    manifest = json.loads((root / "matrix-manifest.json").read_text(encoding="utf-8"))
    identities, paths = set(), set()
    for record in manifest["records"]:
        ident = record["id"]
        if str(uuid.UUID(ident)) != ident or ident in identities or record["archived"] not in (0, 1):
            raise ValueError("invalid or duplicate synthetic manifest identity")
        directory = root / "codex" / ("archived_sessions" if record["archived"] else "sessions")
        logical = directory / ("rollout-2026-01-01T00-00-00-" + ident + ".jsonl")
        path = root / record["path"]
        if (root / record["db_path"] != logical
                or path not in (logical, logical.with_suffix(".jsonl.zst")) or path in paths):
            raise ValueError("manifest path is outside its exact synthetic rollout location")
        identities.add(ident)
        paths.add(path)
    if set(manifest["protected"]) != set(PROTECTED_FILES):
        raise ValueError("unexpected protected manifest paths")
    return manifest


def expected(case, manifest):
    archived = {record["id"] for record in manifest["records"] if record["archived"]}
    scenario = case["scenario"]
    removed = archived if scenario.startswith("cleanup-") else (
        archived - set(case["busy_ids"]) if scenario == "one-busy" else set())
    eligible = len(removed) if requires_reset(case) else (
        1 if scenario == "mixed-one" else len(archived) if scenario.startswith("headers-") else 0)
    return {"examined": len(archived), "eligible": eligible, "deleted": len(removed),
            "skipped": len(archived) - eligible}, removed


def populate(case):
    root = Path(case["fixture_root"])
    count = int(case["scenario"].removeprefix("cleanup-")) if case["scenario"].startswith("cleanup-") else (
        100000 if case["scenario"] == "summary" else 1000 if case["scenario"].startswith("mixed-") else 128)
    # Initial cleanup must not consume the workload before the timed command.
    base = fixture.create(root, count, 4096, case["now"], archived_at=case["now"])
    records = [dict(record, db_path=record["path"], archived=1) for record in base["records"]]
    database = root / "codex/state_5.sqlite"
    with closing(sqlite3.connect(database)) as connection:
        if case["scenario"].startswith("mixed-"):
            for number in range(99000):
                thread_id = str(uuid.uuid5(fixture.NAMESPACE, "active-" + str(number)))
                path = root / "codex/sessions" / ("rollout-2026-01-01T00-00-00-" + thread_id + ".jsonl")
                data = (json.dumps({"type": "session_meta", "payload": {
                    "id": thread_id, "history_mode": "legacy"}}, separators=(",", ":")) + "\n").encode()
                path.write_bytes(data)
                connection.execute(
                    "INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,"
                    "title,sandbox_policy,approval_mode,archived,history_mode) "
                    "VALUES(?,?,0,0,'cli','openai','/fixture','Synthetic active','read-only','never',0,'legacy')",
                    (thread_id, str(path)))
                relative = str(path.relative_to(root))
                records.append({"id": thread_id, "path": relative, "db_path": relative,
                                "archived": 0, "sha256": hashlib.sha256(data).hexdigest()})
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")

    if case["scenario"].startswith("headers-"):
        for record in records:
            path = root / record["path"]
            first, rest = path.read_bytes().split(b"\n", 1)
            header = json.loads(first)
            header["payload"]["unused"] = {"large_text": "x" * (512 * 1024)}
            path.write_bytes(json.dumps(header, separators=(",", ":")).encode() + b"\n" + rest)
            if case["scenario"] == "headers-zstd":
                compressed = path.with_suffix(path.suffix + ".zst")
                with compressed.open("xb") as stream:
                    subprocess.run([case["zstd"], "-q", "-3", "-c", str(path)], stdout=stream,
                                   stdin=subprocess.DEVNULL, check=True)
                path.unlink()
                path = compressed
                record["path"] = str(path.relative_to(root))
            record["sha256"] = digest(path)

    output = benchmark.execute(
        [case["binary"], "--state-dir", str(root / "state"), "--json", "enable",
         "--codex-home", str(root / "codex"), "--codex-bin", case["codex_bin"],
         "--days", "30", "--no-schedule", "--yes"], env=benchmark.environment(case))
    (root / "enable-result.json").write_text(output, encoding="utf-8")
    fixture.owned_root(root)
    with closing(sqlite3.connect(database)) as connection:
        due = requires_reset(case) or case["scenario"].startswith("headers-")
        connection.execute("UPDATE codex_retain_epochs SET archived_since=?",
                           (case["now"] - 40 * fixture.DAY if due else case["now"],))
        if case["scenario"] == "mixed-one":
            chosen = min(record["id"] for record in records if record["archived"])
            connection.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                               (case["now"] - 40 * fixture.DAY, chosen))
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        if requires_reset(case):
            with closing(sqlite3.connect(root / "matrix-seed.sqlite")) as seed:
                connection.backup(seed)
        snapshot = database_snapshot(connection)
    manifest = {"scenario": case["scenario"], "now": case["now"], "codex_bin": case["codex_bin"],
                "records": records, "created_at": base["created_at"], "rollout_bytes": 4096,
                "protected": {path: digest(root / path) for path in PROTECTED_FILES}, "database": snapshot}
    fixture.write_json(root / "matrix-manifest.json", manifest)
    return manifest


def prepare_fixture(case):
    root = Path(case["fixture_root"])
    reused = root.exists()
    if reused:
        manifest = read_manifest(fixture.owned_root(root))
        # Old descriptors used one fixture per binary and did not record these
        # provenance fields. Their path.stem routing remains valid for callbacks.
        for key in ("scenario", "now", "codex_bin"):
            if key in manifest and manifest[key] != case[key]:
                raise ValueError("reused fixture differs from its matrix scenario: " + key)
    else:
        manifest = populate(case)
    archived_ids = sorted(record["id"] for record in manifest["records"] if record["archived"])
    case["busy_ids"] = archived_ids if case["scenario"] == "all-busy" else (
        [archived_ids[len(archived_ids) // 2]] if case["scenario"] == "one-busy" else [])
    case["policy_sha256"] = manifest["protected"]["state/policy.json"]
    guard_policy(case)
    return reused


@contextmanager
def held_locks(case):
    """Keep native lock inodes alive across fixture resets and all samples."""
    root = fixture.owned_root(case["fixture_root"])
    files = []
    case["held_locks"] = []
    try:
        if case["scenario"] == "global-busy":
            paths = [root / "codex/.tmp/rollout-maintenance.lock"]
        else:
            paths = [root / "codex/thread-writer-locks" / (ident + ".lock") for ident in case["busy_ids"]]
        if paths:
            directory = root / "codex/thread-writer-locks"
            directory.mkdir(mode=0o700, exist_ok=True)
            with (directory / ".coordination.lock").open("a+b") as coordination:
                fcntl.flock(coordination, fcntl.LOCK_EX | fcntl.LOCK_NB)
                for path in paths:
                    path.parent.mkdir(mode=0o700, exist_ok=True)
                    stream = path.open("a+b")
                    files.append(stream)
                    fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    metadata = os.fstat(stream.fileno())
                    case["held_locks"].append({"path": str(path.relative_to(root)),
                                               "device": metadata.st_dev, "inode": metadata.st_ino})
        yield
    finally:
        # Keep stale paths rather than unlinking without a fresh coordinator.
        for stream in reversed(files):
            stream.close()


def verify_locks(case, root):
    for record in case.get("held_locks", []):
        path = fixture.no_symlinks(root / record["path"])
        if path != root / "codex/.tmp/rollout-maintenance.lock":
            ident = path.name.removesuffix(".lock")
            if (path.parent != root / "codex/thread-writer-locks"
                    or str(uuid.UUID(ident)) != ident or path.name != ident + ".lock"):
                raise ValueError("held lock path is outside the synthetic fixture")
        metadata = path.stat()
        if (metadata.st_dev, metadata.st_ino) != (record["device"], record["inode"]):
            raise ValueError("foreign lock inode changed: " + str(path))
        with path.open("r+b") as stream:
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                continue
            raise ValueError("the orchestrator no longer holds lock " + str(path))


def restore(case_path):
    case = read_case(case_path)
    if not requires_reset(case):
        raise ValueError("only cleanup or contention fixtures need reset")
    root = fixture.owned_root(case["fixture_root"])
    verify_locks(case, root)
    if (root / "state/pending.json").exists():
        raise ValueError("refusing to reset a pending intent")
    manifest = read_manifest(root)
    with closing(sqlite3.connect((root / "matrix-seed.sqlite").as_uri() + "?mode=ro", uri=True)) as seed:
        with closing(sqlite3.connect(root / "codex/state_5.sqlite")) as connection:
            seed.backup(connection)
            connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    for record in manifest["records"]:
        path = root / record["path"]
        if not path.exists():
            data = fixture.transcript(record["id"], manifest["created_at"], manifest["rollout_bytes"])
            if hashlib.sha256(data).hexdigest() != record["sha256"]:
                raise ValueError("reset bytes differ from the original synthetic rollout")
            with path.open("xb") as stream:
                stream.write(data)
            os.utime(path, (manifest["created_at"], manifest["created_at"]))
        elif digest(path) != record["sha256"]:
            raise ValueError("refusing to overwrite altered rollout during reset")
    receipt = root / "state/last-run.json"
    if receipt.exists():
        receipt.unlink()
    guard_policy(case)


def verify(case_path, report=None, before=False):
    case = read_case(case_path)
    root = fixture.owned_root(case["fixture_root"])
    manifest_path = root / "matrix-manifest.json"
    manifest = read_manifest(root)
    counts, removed = expected(case, manifest)
    if before:
        removed = set()
    verify_locks(case, root)
    remaining = {record["id"]: record for record in manifest["records"] if record["id"] not in removed}
    expected_paths = {root / record["path"] for record in remaining.values()}
    actual_paths = {path for directory in (root / "codex/sessions", root / "codex/archived_sessions")
                    for path in directory.rglob("*") if path.is_file()}
    if actual_paths != expected_paths:
        raise ValueError("surviving rollout path set differs from expected IDs")
    for record in remaining.values():
        if digest(root / record["path"]) != record["sha256"]:
            raise ValueError("surviving rollout content changed: " + record["id"])
    for path, sha256 in manifest["protected"].items():
        if digest(root / path) != sha256:
            raise ValueError("protected fixture file changed: " + path)
    if (root / "state/pending.json").exists():
        raise ValueError("pending intent remains")
    with closing(sqlite3.connect((root / "codex/state_5.sqlite").as_uri() + "?mode=ro", uri=True)) as connection:
        if connection.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise ValueError("SQLite integrity check failed")
        if connection.execute("PRAGMA foreign_key_check").fetchall():
            raise ValueError("SQLite foreign key check failed")
        actual = database_snapshot(connection)
        original = manifest["database"]
        if (actual["thread_columns"] != original["thread_columns"]
                or actual["threads"] != {ident: original["threads"][ident] for ident in remaining}):
            raise ValueError("surviving SQLite thread metadata differs")
        if actual["epochs"] != {ident: original["epochs"][ident]
                                for ident, record in remaining.items() if record["archived"]}:
            raise ValueError("capture epoch values differ from surviving archives")
        if actual["protected"] != original["protected"]:
            raise ValueError("protected SQLite schema or metadata differs")
    if not before:
        if report is None and command(case)[-1] != "preview":
            report = json.loads((root / "state/last-run.json").read_text(encoding="utf-8"))
        if report is not None:
            for key, value in counts.items():
                if key == "eligible" and "eligible" not in report:
                    continue  # The bounded scheduled receipt deliberately omits eligibility.
                if report.get(key) != value:
                    raise ValueError(f"report {key}: expected {value}, got {report.get(key)}")
            if "entries" in report:
                archive_ids = {record["id"] for record in manifest["records"] if record["archived"]}
                if (len(report["entries"]) != len(archive_ids)
                        or {entry["id"] for entry in report["entries"]} != archive_ids):
                    raise ValueError("report entry IDs differ from indexed archives")
                if {entry["id"] for entry in report["entries"] if entry["reason"] == "deleted"} != removed:
                    raise ValueError("reported deleted IDs differ from the authorized fixture set")
                if command(case)[-1] == "preview":
                    eligible_ids = {min(archive_ids)} if case["scenario"] == "mixed-one" else (
                        archive_ids if case["scenario"].startswith("headers-") else set())
                    if {entry["id"] for entry in report["entries"] if entry["reason"] == "eligible"} != eligible_ids:
                        raise ValueError("reported eligible IDs differ from the aged fixture set")
            if report.get("logical_bytes_removed") != len(removed) * manifest["rollout_bytes"]:
                raise ValueError("reported logical bytes differ from removed synthetic contents")
            warnings = report.get("warnings", [])
            if warnings and not (case["scenario"] == "global-busy" and warnings == [GLOBAL_STOP_WARNING]):
                raise ValueError("unexpected recovery warning")
    return {"verified": True, "manifest_sha256": digest(manifest_path), "counts": counts,
            "removed_ids": sorted(removed), "remaining_rows": len(remaining),
            "remaining_ids_sha256": hashlib.sha256("\n".join(sorted(remaining)).encode()).hexdigest(),
            "rollout_content_verified": True, "thread_metadata_verified": True,
            "epoch_values_verified": True, "protected_metadata_verified": True,
            "sqlite_integrity": "ok", "pending": False,
            "held_locks": case.get("held_locks", [])}


def observe(case_path, label):
    case = read_case(case_path)
    guard_policy(case)
    result = subprocess.run(command(case), env=benchmark.environment(case), stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=300)
    wanted = 3 if case["scenario"].endswith("busy") else 0
    if result.returncode != wanted or result.stderr:
        raise ValueError(f"unexpected command outcome ({result.returncode}, expected {wanted}): "
                         + result.stderr[-4000:])
    if case["scenario"] == "summary" and result.stdout:
        raise ValueError("scheduled run produced stdout")
    report = json.loads(result.stdout) if result.stdout else None
    receipt = verify(case_path, report)
    receipt.update(exit_code=result.returncode, stdout=report if report is not None else result.stdout)
    fixture.write_json(Path(case["benchmark_root"]) / (case_path.stem + "-" + label + ".json"), receipt)
    return receipt


def sample_rss(case_path):
    case = read_case(case_path)
    if requires_reset(case):
        restore(case_path)
    guard_policy(case)
    values = []
    started = time.monotonic()
    with open(os.devnull, "wb") as sink:
        process = subprocess.Popen(command(case), env=benchmark.environment(case), stdin=subprocess.DEVNULL,
                                   stdout=sink, stderr=sink)
        try:
            while process.poll() is None:
                if time.monotonic() - started > 300:
                    raise ValueError("RSS observation timed out")
                result = subprocess.run(["/bin/ps", "-o", "rss=", "-p", str(process.pid)],
                                        capture_output=True, text=True, timeout=5)
                if result.stdout.strip():
                    values.append(int(result.stdout.strip()))
                time.sleep(0.005)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
    wanted = 3 if case["scenario"].endswith("busy") else 0
    if process.returncode != wanted:
        raise ValueError("RSS observation command failed")
    return {"sampled_parent_rss_max_kib": max(values, default=None), "samples": values,
            "measurement": "separate ps observation; sampled lower bound, excludes child processes"}


def measure(case_path, hyperfine, runs, rss, prepare_only):
    case = read_case(case_path)
    environment = json.loads((Path(case["benchmark_root"]) / "environment.json").read_text(encoding="utf-8"))
    expected_binary_hash = environment["binaries"][case["tool"]]["sha256"]
    if digest(case["binary"]) != expected_binary_hash:
        raise ValueError("measured binary changed since matrix setup")
    reused = prepare_fixture(case)
    with held_locks(case):
        fixture.write_json(case_path, case)
        if reused and requires_reset(case):
            restore(case_path)
        observe(case_path, "preflight")
        if prepare_only:
            if digest(case["binary"]) != expected_binary_hash:
                raise ValueError("measured binary changed during preflight")
            return
        invocation = [hyperfine, "--shell=none", "--warmup", "3", "--runs", str(runs), "--output=pipe",
                      "--command-name", case_path.stem, "--export-json",
                      str(Path(case["benchmark_root"]) / (case_path.stem + "-hyperfine.json"))]
        contention = case["scenario"].endswith("busy")
        internal = [sys.executable, str(SCRIPT)]
        if requires_reset(case):
            invocation += ["--prepare", shlex.join(internal + ["_prepare", "--case", str(case_path)]),
                           "--conclude", shlex.join(internal + ["_validate", "--case", str(case_path)])]
            if contention:
                invocation += ["--ignore-failure"]
        else:
            invocation += ["--prepare", shlex.join(internal + ["_guard", "--case", str(case_path)])]
        env = benchmark.environment(case)
        literal = shlex.join(["/usr/bin/env", "-i", *[key + "=" + value for key, value in env.items()], *command(case)])
        subprocess.run(invocation + [literal], check=True)
        raw = json.loads((Path(case["benchmark_root"]) / (case_path.stem + "-hyperfine.json")).read_text())
        for result in raw["results"]:
            if result["exit_codes"] != [3 if contention else 0] * runs:
                raise ValueError("Hyperfine recorded unexpected command exit codes")
        # The final complete check covers all read-only timing and RSS runs.
        # Cleanup and contention retain per-sample conclude checks and reset.
        observation = sample_rss(case_path) if rss else None
        if requires_reset(case):
            restore(case_path)
        receipt = observe(case_path, "postflight")
        if observation is not None:
            receipt["rss"] = observation
        if digest(case["binary"]) != expected_binary_hash:
            raise ValueError("measured binary changed during measurement")
        receipt["measured_binary_sha256"] = expected_binary_hash
        fixture.write_json(Path(case["benchmark_root"]) / (case_path.stem + "-result.json"), receipt)


def setup(args):
    root = fixture.no_symlinks(args.root)
    if root.exists() or root.is_relative_to(fixture.REPOSITORY):
        raise ValueError("matrix root must be a nonexistent scratch directory outside the repository")
    if not 3 <= args.runs <= 100:
        raise ValueError("runs must be 3..100")
    baseline, candidate, codex_bin, hyperfine = map(benchmark.executable,
        (args.baseline, args.candidate, args.codex_bin, args.hyperfine))
    zstd = benchmark.executable(args.zstd) if "headers" in args.cases else None
    root.mkdir(parents=True, mode=0o700)
    fixture.write_json(root / benchmark.BENCHMARK_MARKER,
                       {"kind": "codex-retain-benchmark", "schema": 1, "root": str(root)})
    (root / "cases").mkdir()
    (root / "fixtures").mkdir()
    scenarios = []
    for group in dict.fromkeys(args.cases):
        scenarios += {"mixed": ["mixed-none", "mixed-one"], "summary": ["summary"],
                      "contention": ["one-busy", "global-busy"] + (["all-busy"] if args.all_busy else []),
                      "cleanup": ["cleanup-1000", "cleanup-10000"],
                      "headers": ["headers-plain", "headers-zstd"]}[group]
    now = int(time.time())
    cases = []
    for index, scenario in enumerate(scenarios):
        order = [("baseline", baseline), ("candidate", candidate)]
        if index % 2:
            order.reverse()
        for tool, binary in order:
            path = root / "cases" / (scenario + "-" + tool + ".json")
            case = {"benchmark_root": str(root), "fixture_key": scenario,
                    "fixture_root": str(root / "fixtures" / scenario),
                    "scenario": scenario, "tool": tool, "binary": binary, "codex_bin": codex_bin,
                    "zstd": zstd, "now": now, "busy_ids": [], "held_locks": []}
            fixture.write_json(path, case)
            cases.append(path)
    versions = {}
    probe = root / "version-home"
    probe.mkdir(mode=0o700)
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(probe),
           "CODEX_HOME": str(probe / "codex"), "TMPDIR": str(probe), "LC_ALL": "C"}
    for name, binary in (("baseline", baseline), ("candidate", candidate), ("codex", codex_bin),
                         ("hyperfine", hyperfine)):
        versions[name] = {"path": binary, "sha256": digest(binary), "bytes": Path(binary).stat().st_size,
                          "version": benchmark.version([binary, "--version"], env=env)}
    fixture.write_json(root / "environment.json", {
        "schema": 1, "date_utc": fixture.iso(now), "platform": platform.platform(),
        "architecture": platform.machine(), "python": platform.python_version(), "binaries": versions,
        "script_sha256": digest(SCRIPT), "warmups": 3, "runs": args.runs, "cases": scenarios,
        "cache": "warm filesystem cache; read-only fixtures reused, cleanup and contention data restored outside timing",
        "pairing": "baseline and candidate share the same fixture path, policy, and source manifest per scenario",
        "background_services_installed": False,
        "limits": ["Shared host; no CPU or filesystem-cache isolation.",
                   "RSS is a separate sampled parent-only lower bound, not allocation or process-tree memory.",
                   "Synthetic version-specific profiles establish only the measured workloads."]})
    return root, cases, hyperfine


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    run = sub.add_parser("run", help="prepare isolated cases, verify, and measure existing binaries")
    for name in ("baseline", "candidate", "codex-bin"):
        run.add_argument("--" + name, required=True)
    run.add_argument("--root", required=True, type=Path)
    run.add_argument("--cases", nargs="+", choices=["mixed", "summary", "contention", "headers", "cleanup"],
                     default=["mixed", "summary", "contention"])
    run.add_argument("--runs", type=int, default=5)
    run.add_argument("--hyperfine", default="hyperfine")
    run.add_argument("--zstd", default="zstd", help="required only for the headers group")
    run.add_argument("--all-busy", action="store_true")
    run.add_argument("--rss", action="store_true")
    run.add_argument("--prepare-only", action="store_true")
    for action in ("_prepare", "_validate", "_guard"):
        internal = sub.add_parser(action, help="internal Hyperfine lifecycle callback")
        internal.add_argument("--case", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.action == "_prepare":
            restore(args.case)
        elif args.action == "_validate":
            verify(args.case)
        elif args.action == "_guard":
            guard_policy(read_case(args.case))
        else:
            root, cases, hyperfine = setup(args)
            for path in cases:
                print("Matrix case " + path.stem, flush=True)
                measure(path, hyperfine, args.runs, args.rss, args.prepare_only)
            benchmark.summarize(root)
            print("Evidence: " + str(root))
    except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
        print("Performance matrix error: " + str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
