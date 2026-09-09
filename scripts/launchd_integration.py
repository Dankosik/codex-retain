#!/usr/bin/env python3
"""Test native launchd with a unique, temporary job and synthetic Codex data only.

This intentionally registers a live GUI-session test job. It never installs the
production label or writes ~/Library/LaunchAgents. Run only after coordinating
the release binary and authorizing isolated native scheduler testing.
"""

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
import fixture  # noqa: E402


PREFIX = "io.github.codex-retain.test."
MARKER = ".codex-retain-launchd-test.json"
EVIDENCE = Path(__file__).resolve().parent.parent / "docs/evidence/launchd-integration.json"
WAIT_SECONDS = 20


@dataclass(frozen=True)
class Context:
    root: Path
    label: str
    uid: int
    retain: str
    codex: str

    @property
    def fixture(self):
        return self.root / "fixture"

    @property
    def target(self):
        return f"gui/{self.uid}/{self.label}"

    @property
    def plist(self):
        return self.root / "job.plist"


def validate_identity(context):
    root = fixture.no_symlinks(context.root)
    suffix = context.label.removeprefix(PREFIX)
    if not context.label.startswith(PREFIX) or str(uuid.UUID(suffix)) != suffix:
        raise ValueError("refusing a noncanonical or production launchd label")
    if context.uid != os.getuid() or context.uid == 0:
        raise ValueError("test job must belong to the current non-root GUI user")
    if "LaunchAgents" in root.parts or "LaunchDaemons" in root.parts:
        raise ValueError("test plist must stay outside automatic launchd directories")
    marker_path = fixture.no_symlinks(root / MARKER)
    data = json.loads(marker_path.read_text(encoding="utf-8"))
    if data != {"kind": "codex-retain-launchd-test", "schema": 1, "root": str(root),
                "label": context.label, "uid": context.uid}:
        raise ValueError("test root marker identity changed")
    return root


def run(command, *, env=None, timeout=5, check=True):
    """Own and reap the entire short-lived tool process group on interruption."""
    process = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate(timeout=5)
        raise
    if check and process.returncode != 0:
        raise ValueError(f"tool failed ({process.returncode}): {command[0]} {command[1:]}: {stderr[-1000:]}")
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def environment(context):
    home = context.fixture / "home"
    temporary = home / "tmp"
    temporary.mkdir(exist_ok=True)
    return {"PATH": os.environ.get("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
            "HOME": str(home), "CODEX_HOME": str(context.fixture / "codex"),
            "TMPDIR": str(temporary), "LC_ALL": "C", "NO_COLOR": "1"}


def control(context, operation, *arguments, check=True, timeout=5):
    validate_identity(context)
    if operation == "bootstrap":
        expected = (f"gui/{context.uid}", str(context.plist))
    elif operation in {"print", "bootout"}:
        expected = (context.target,)
    else:
        raise ValueError("launchctl operation is outside the test allowlist")
    if arguments != expected:
        raise ValueError("launchctl arguments do not identify the unique test job")
    return run(["/bin/launchctl", operation, *arguments],
               env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LC_ALL": "C"}, check=check, timeout=timeout)


def job_state(context, timeout=5):
    result = control(context, "print", context.target, check=False, timeout=timeout)
    if result.returncode == 113 and "Could not find service" in result.stderr and context.label in result.stderr:
        return {"registered": False, "pid": None, "runs": None, "last_exit_code": None}
    if result.returncode:
        raise ValueError("cannot establish test-job registration: " + result.stderr[-1000:])
    def number(name):
        match = re.search(r"(?m)^\s*" + re.escape(name) + r" = (\d+)\s*$", result.stdout)
        return int(match[1]) if match else None
    state = re.search(r"(?m)^\s*state = (.+)$", result.stdout)
    return {"registered": True, "pid": number("pid"), "runs": number("runs"),
            "last_exit_code": number("last exit code"),
            "state": state[1].strip() if state else None}


def wait_for(description, predicate, seconds=WAIT_SECONDS):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate(max(0.01, deadline - time.monotonic()))
        if result:
            return result
        time.sleep(min(0.2, max(0, deadline - time.monotonic())))
    raise ValueError(f"timed out after {seconds}s waiting for {description}")


def utility(context, *arguments):
    if not arguments or arguments[0] not in {"enable", "pause", "preview"}:
        raise ValueError("utility operation is outside the fixture-test allowlist")
    if arguments[0] == "enable" and arguments != (
        "enable", "--codex-home", str(context.fixture / "codex"), "--codex-bin", context.codex,
        "--days", "30", "--no-schedule", "--yes",
    ):
        raise ValueError("fixture enable must use explicit isolated paths and no scheduling")
    validate_identity(context)
    fixture.owned_root(context.fixture)
    return json.loads(run([context.retain, "--state-dir", str(context.fixture / "state"),
                           "--json", *arguments], env=environment(context)).stdout)


def receipt(context):
    path = fixture.no_symlinks(context.fixture / "state/last-run.json")
    if not path.is_file():
        return None
    if path.stat().st_size > 128 * 1024:
        raise ValueError("unexpectedly large scheduled receipt")
    return json.loads(path.read_text(encoding="utf-8"))


def completed_receipt(context):
    report = receipt(context)
    if report is not None and report.get("status") in {"error", "attention"}:
        raise ValueError("scheduled cleanup reported failure: " + json.dumps(report))
    if report is not None and report.get("deleted") == 2 and report.get("skipped") == 0:
        return report
    return None


def idle(context, timeout=5):
    state = job_state(context, timeout=timeout)
    if not state["registered"]:
        raise ValueError("test job disappeared before scheduled execution was verified")
    if state["pid"] is None and state["last_exit_code"] == 0:
        return state
    return None


def paused_candidate(context):
    """Insert one new, aged synthetic archive after the real policy is paused."""
    fixture.owned_root(context.fixture)
    now = int(time.time())
    thread_id = str(uuid.uuid4())
    path = context.fixture / "codex/archived_sessions" / ("rollout-2026-01-01T00-00-00-" + thread_id + ".jsonl")
    data = fixture.transcript(thread_id, now - 100 * fixture.DAY, 4096)
    path.write_bytes(data)
    os.utime(path, (now - 100 * fixture.DAY, now - 100 * fixture.DAY))
    with sqlite3.connect(context.fixture / "codex/state_5.sqlite", timeout=1) as connection:
        connection.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, "
            "title, sandbox_policy, approval_mode, archived, archived_at, cli_version, first_user_message, "
            "preview, history_mode, has_user_event) "
            "VALUES (?, ?, ?, ?, 'cli', 'openai', '/fixture', 'Paused fixture', 'read-only', 'never', "
            "1, ?, '0.153.4', 'Synthetic paused conversation.', 'Synthetic paused conversation.', 'legacy', 1)",
            (thread_id, str(path), now - 100 * fixture.DAY, now - 100 * fixture.DAY, now - 40 * fixture.DAY),
        )
        changed = connection.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                                     (now - 40 * fixture.DAY, thread_id)).rowcount
        if changed != 1:
            raise ValueError("paused fixture did not receive a captured archive epoch")
    return {"id": thread_id, "path": str(path), "sha256": hashlib.sha256(data).hexdigest()}


def verify_paused(context, candidate):
    fixture.owned_root(context.fixture)
    path = Path(candidate["path"])
    if path.parent != context.fixture / "codex/archived_sessions":
        raise ValueError("paused candidate path escaped its fixture")
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != candidate["sha256"]:
        raise ValueError("paused candidate was deleted or modified")
    if list(path.parent.glob("*.jsonl")) != [path]:
        raise ValueError("unexpected fixture archive contents after pause")
    database = context.fixture / "codex/state_5.sqlite"
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=1) as connection:
        rows = connection.execute("SELECT id,archived FROM threads").fetchall()
        if rows != [(candidate["id"], 1)] or connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise ValueError("paused candidate or database integrity changed")


def fail_closed_fixture(context):
    """Emergency test-only safety if launchd refuses to unregister the test job."""
    validate_identity(context)
    fixture.owned_root(context.fixture)
    path = fixture.no_symlinks(context.fixture / "state/policy.json")
    policy = json.loads(path.read_text(encoding="utf-8"))
    if policy.get("owner") != str(context.fixture / "state") or policy.get("codex_home") != str(context.fixture / "codex"):
        raise ValueError("refusing to disable an unrecognized fixture policy")
    policy["enabled"] = False
    temporary = path.with_name("policy.launchd-test-disabled.json")
    fixture.write_json(temporary, policy)
    temporary.replace(path)


def cleanup(context):
    validate_identity(context)
    errors = []
    for _ in range(3):
        try:
            control(context, "bootout", context.target, check=False)
            if not job_state(context)["registered"]:
                return {"bootout_verified": True, "registered": False, "errors": errors}
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append(str(error))
        time.sleep(0.2)
    try:
        fail_closed_fixture(context)
        errors.append("fixture policy disabled directly after failed test-job removal")
    except (OSError, ValueError, KeyError) as error:
        errors.append("fixture fail-closed step failed: " + str(error))
    return {"bootout_verified": False, "registered": "unknown", "errors": errors,
            "manual_cleanup_target": context.target}


def integration(context, evidence):
    attempted = False
    original_handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    def interrupted(signum, _frame):
        raise InterruptedError("native scheduler test interrupted by signal " + str(signum))
    for sig in original_handlers:
        signal.signal(sig, interrupted)
    try:
        fixture.create(context.fixture, count=2)
        evidence["retention_version"] = run([context.retain, "--version"], env=environment(context)).stdout.strip()
        with tempfile.TemporaryDirectory(prefix="version-probe-", dir=context.root) as temporary:
            probe_env = dict(environment(context), HOME=temporary,
                             CODEX_HOME=str(Path(temporary) / "codex"), TMPDIR=temporary)
            evidence["codex_version"] = run([context.codex, "--version"], env=probe_env).stdout.strip()
        evidence["enable"] = utility(context, "enable", "--codex-home", str(context.fixture / "codex"),
                                      "--codex-bin", context.codex, "--days", "30", "--no-schedule", "--yes")
        fixture.owned_root(context.fixture)
        with sqlite3.connect(context.fixture / "codex/state_5.sqlite", timeout=1) as connection:
            if connection.execute("UPDATE codex_retain_epochs SET archived_since=unixepoch()-40*86400").rowcount != 2:
                raise ValueError("expected exactly two synthetic retention epochs")
        expected_plist = {
            "Label": context.label,
            "ProgramArguments": [context.retain, "--state-dir", str(context.fixture / "state"), "run", "--scheduled"],
            "EnvironmentVariables": environment(context), "StartInterval": 2, "RunAtLoad": False,
            "StandardOutPath": "/dev/null", "StandardErrorPath": "/dev/null",
        }
        with context.plist.open("xb") as stream:
            plistlib.dump(expected_plist, stream)
        context.plist.chmod(0o600)
        if plistlib.loads(context.plist.read_bytes()) != expected_plist:
            raise ValueError("test plist failed exact readback")
        evidence["plist_sha256"] = hashlib.sha256(context.plist.read_bytes()).hexdigest()
        evidence["before_bootstrap"] = job_state(context)
        if evidence["before_bootstrap"]["registered"]:
            raise ValueError("generated test label already exists; refusing to touch it")
        attempted = True  # A failed/timed-out bootstrap may still register it.
        control(context, "bootstrap", f"gui/{context.uid}", str(context.plist))
        started = time.monotonic()
        evidence["scheduled_cleanup"] = wait_for("two scheduled synthetic deletions", lambda _: completed_receipt(context))
        evidence["first_completion_seconds"] = time.monotonic() - started
        evidence["post_cleanup"] = fixture.verify(context.fixture, expect_removed=True, expect_thread_rows=0)
        evidence["idle_after_cleanup"] = wait_for("completed job to have no PID", lambda remaining: idle(context, min(5, remaining)), seconds=5)
        if utility(context, "pause").get("paused") is not True:
            raise ValueError("fixture policy did not pause")
        candidate = paused_candidate(context)
        preview = utility(context, "preview")
        if preview.get("eligible") != 1 or preview.get("skipped") != 0:
            raise ValueError("paused fixture is not an otherwise eligible archive")
        previous_receipt = receipt(context)
        baseline = job_state(context)
        if baseline["runs"] is None:
            raise ValueError("launchctl did not report an execution counter")
        def paused_execution(remaining):
            state = idle(context, min(5, remaining))
            return state if state and state["runs"] is not None and state["runs"] > baseline["runs"] else None
        evidence["paused_invocation"] = wait_for("a completed scheduled invocation while paused", paused_execution)
        verify_paused(context, candidate)
        if receipt(context) != previous_receipt:
            raise ValueError("paused scheduler invocation unexpectedly changed the run receipt")
        evidence["paused_candidate_preserved"] = True
        evidence["paused_candidate_id"] = candidate["id"]
        evidence["status"] = "passed"
    except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
        evidence["status"] = "failed"
        evidence["error"] = str(error)
    finally:
        # A second interrupt must not bypass removal of this uniquely owned job.
        for sig in original_handlers:
            signal.signal(sig, signal.SIG_IGN)
        try:
            try:
                evidence["cleanup"] = cleanup(context) if attempted else {"bootout_verified": None, "registration_attempted": False}
            except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
                evidence["cleanup"] = {"bootout_verified": False, "error": str(error),
                                       "manual_cleanup_target": context.target}
            if attempted and not evidence["cleanup"]["bootout_verified"]:
                evidence["status"] = "failed_cleanup_required"
        finally:
            for sig, handler in original_handlers.items():
                signal.signal(sig, handler)


def binary(value):
    selected = shutil.which(str(value))
    if selected is None:
        raise ValueError("executable is unavailable: " + str(value))
    return str(Path(selected).resolve(strict=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="new scratch directory outside LaunchAgents and real Codex data")
    parser.add_argument("--retain", required=True, help="coordinated final release binary")
    parser.add_argument("--codex-bin", required=True, help="real supported Codex executable")
    args = parser.parse_args()
    try:
        if platform.system() != "Darwin" or os.getuid() == 0:
            raise ValueError("native integration requires a non-root macOS GUI login")
        root = fixture.no_symlinks(args.root)
        if root.exists() or "LaunchAgents" in root.parts or "LaunchDaemons" in root.parts:
            raise ValueError("test root must be new and outside automatic launchd directories")
        real_codex = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).absolute()
        real_policy = Path(os.environ.get("CODEX_RETAIN_STATE_DIR", str(Path.home() / "Library/Application Support/codex-retain"))).absolute()
        if root.is_relative_to(real_codex) or root.is_relative_to(real_policy):
            raise ValueError("test root must be outside the user's Codex and retention data")
        context = Context(root, PREFIX + str(uuid.uuid4()), os.getuid(), binary(args.retain), binary(args.codex_bin))
        root.mkdir(parents=True, mode=0o700)
        fixture.write_json(root / MARKER, {"kind": "codex-retain-launchd-test", "schema": 1,
                                          "root": str(root), "label": context.label, "uid": context.uid})
        before_hash = hashlib.sha256(Path(context.retain).read_bytes()).hexdigest()
        evidence = {
            "schema": 1, "status": "running", "started_at": fixture.iso(int(time.time())),
            "platform": platform.platform(), "architecture": platform.machine(),
            "test_label": context.label, "retention_binary_sha256": before_hash,
            "start_interval_seconds": 2, "fixture_threads": 2,
            "production_label_touched": False, "production_launchagents_directory_touched": False,
            "scope": "Native temporary test job invokes the real run --scheduled entrypoint. Production install/disable adapter is not invoked; its registration tests remain mocked.",
        }
        integration(context, evidence)
        if hashlib.sha256(Path(context.retain).read_bytes()).hexdigest() != before_hash:
            if evidence["status"] != "failed_cleanup_required":
                evidence["status"] = "failed"
            evidence["error"] = "release binary changed during native integration"
        evidence["finished_at"] = fixture.iso(int(time.time()))
        fixture.write_json(root / "result.json", evidence)
        destination = fixture.no_symlinks(EVIDENCE)
        destination.parent.mkdir(parents=True, exist_ok=True)
        fixture.write_json(destination, evidence)
        print(json.dumps({"status": evidence["status"], "cleanup": evidence["cleanup"], "evidence": str(destination)}))
        return 0 if evidence["status"] == "passed" else 1
    except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
        print("Native launchd integration error: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
