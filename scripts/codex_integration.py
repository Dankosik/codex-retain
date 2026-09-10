#!/usr/bin/env python3
"""Exercise installed Codex and codex-retain only inside a new synthetic profile.

No model turns, user credentials, real chats, or LaunchAgents are used.
The only direct state mutation is explicitly marked test-only epoch aging.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import sqlite3
import subprocess
import threading
import time

MARKER = ".codex-retain-integration-fixture.json"
PINNED_SECTION = "01984de2-8f74-7c91-a3b2-5c5e937cf318"
TIMEOUT = 30
LIMIT = 1024 * 1024


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def command(args, env, timeout=TIMEOUT, allowed=(0,)):
    proc = subprocess.Popen(args, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    captured = [bytearray(), bytearray()]
    overflow = threading.Event()
    def collect(stream, destination):
        while raw := stream.read(4096):
            available = LIMIT - len(destination)
            destination.extend(raw[:available])
            if len(raw) > available:
                overflow.set()
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
    readers = [threading.Thread(target=collect, args=(stream, captured[index]), daemon=True)
               for index, stream in enumerate((proc.stdout, proc.stderr))]
    for reader in readers:
        reader.start()
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
        raise
    finally:
        for reader in readers:
            reader.join(timeout=2)
    if overflow.is_set():
        raise RuntimeError("child output exceeded harness limit")
    if proc.returncode not in allowed:
        raise RuntimeError(f"{Path(args[0]).name} exited {proc.returncode}: "
                           + captured[1].decode(errors="replace")[-3000:]
                           + captured[0].decode(errors="replace")[-3000:])
    return captured[0].decode()


class AppServer:
    def __init__(self, binary, env, cwd):
        self.proc = subprocess.Popen([str(binary), "app-server", "--stdio"],
                                     env=env, cwd=cwd, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.messages = queue.Queue(maxsize=128)
        self.stderr_tail = bytearray()
        self.next_id = 0
        self.methods = []
        self.readers = [
            threading.Thread(target=self._stdout, daemon=True),
            threading.Thread(target=self._stderr, daemon=True),
        ]
        for reader in self.readers:
            reader.start()
        try:
            self.initialize = self.request("initialize", {
                "clientInfo": {"name": "codex-retain-conformance", "version": "1"},
                "capabilities": {"experimentalApi": True},
            })
            self._send({"method": "initialized"})
        except Exception:
            self.close()
            raise

    def _stdout(self):
        try:
            while True:
                raw = self.proc.stdout.readline(LIMIT + 1)
                if not raw:
                    self.messages.put(RuntimeError("app-server stdout closed"), timeout=1)
                    return
                if len(raw) > LIMIT:
                    raise RuntimeError("app-server message exceeded 1 MiB")
                self.messages.put(json.loads(raw), timeout=1)
        except Exception as error:
            try:
                self.messages.put(error, timeout=1)
            except queue.Full:
                pass

    def _stderr(self):
        while raw := self.proc.stderr.read(4096):
            self.stderr_tail.extend(raw)
            del self.stderr_tail[:-16384]

    def _send(self, message):
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        request_id = self.next_id
        self._send({"id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            try:
                message = self.messages.get(timeout=max(0.01, deadline-time.monotonic()))
            except queue.Empty as error:
                raise RuntimeError(f"app-server timeout: {method}") from error
            if isinstance(message, Exception):
                raise message
            if message.get("id") != request_id:
                continue
            if "error" in message:
                raise RuntimeError(f"{method}: {message['error']}")
            self.methods.append(method)
            return message["result"]
        raise RuntimeError(f"app-server timeout: {method}")

    def close(self):
        if self.proc.poll() is None:
            self.proc.stdin.close()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=5)
        for reader in self.readers:
            reader.join(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", type=Path, required=True)
    parser.add_argument("--retain-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True,
                        help="new, nonexistent directory; all fixture data stays here")
    parser.add_argument("--output", type=Path,
                        help="optional concise JSON evidence file")
    args = parser.parse_args()
    codex = args.codex_bin.resolve(strict=True)
    retain = args.retain_bin.resolve(strict=True)
    root = args.root.absolute()
    if root.exists() or root.is_symlink():
        parser.error("--root must not exist; never reuse a real or earlier Codex profile")
    root.parent.resolve(strict=True)
    root.mkdir(mode=0o700)
    root = root.resolve()
    (root / MARKER).write_text(json.dumps({"schema": 1, "purpose": "synthetic integration"}) + "\n")
    home, codex_home, state = root / "user", root / "codex", root / "state"
    for path in (home, codex_home):
        path.mkdir(mode=0o700)
    env = {name: os.environ[name] for name in ("PATH", "TMPDIR", "LANG", "LC_ALL", "TERM")
           if name in os.environ}
    env.update(HOME=str(home), CODEX_HOME=str(codex_home),
               XDG_CONFIG_HOME=str(home / ".config"),
               HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1")
    (codex_home / "config.toml").write_text("""
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
""")
    receipt = {
        "schema": 1, "passed": False, "platform": platform.platform(),
        "codex_version": command([str(codex), "--version"], env).strip(),
        "retain_version": command([str(retain), "--version"], env).strip(),
        "retain_sha256": digest(retain), "codex_launcher_sha256": digest(codex),
        "fixture": "new isolated HOME and CODEX_HOME; native schema and native legacy threads",
        "model_turns_submitted": 0, "launchagents_installed": 0,
        "checks": [], "test_only_mutation": "age one pre-enable synthetic archive timestamp and later capture epochs; adversarially mark a native-loaded synthetic thread archived while its native writer lock remains held",
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
            raise RuntimeError("fixture ownership assertion failed")
        return sqlite3.connect(codex_home / "state_5.sqlite", timeout=1)
    def epoch(thread_id):
        with db() as conn:
            row = conn.execute("SELECT archived_since FROM codex_retain_epochs WHERE thread_id=?", (thread_id,)).fetchone()
            return None if row is None else row[0]
    def age(thread_id):
        with db() as conn:
            conn.execute("UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                         (int(time.time())-31*86400, thread_id))
    def row(thread_id):
        with db() as conn:
            return conn.execute("SELECT archived,archived_at,rollout_path,is_pinned,thread_section_id FROM threads WHERE id=?", (thread_id,)).fetchone()
    try:
        app = AppServer(codex, env, root)
        receipt["app_server_user_agent"] = app.initialize["userAgent"]
        ids = []
        for _ in range(4):
            result = app.request("thread/start", {"cwd": str(root), "historyMode": "legacy", "ephemeral": False})
            thread_id = result["thread"]["id"]
            # Native section move explicitly persists a thread before its first turn.
            app.request("thread/section/move", {"threadId": thread_id, "sectionId": None, "beforeThreadId": None})
            ids.append(thread_id)
        candidate, pinned, active, initially_due = ids
        app.request("thread/archive", {"threadId": candidate})
        app.request("thread/archive", {"threadId": pinned})
        app.request("thread/archive", {"threadId": initially_due})
        # The native SQLx backfill may finish asynchronously.
        deadline = time.monotonic() + 10
        while True:
            with db() as conn:
                progress = conn.execute("SELECT status FROM backfill_state WHERE id=1").fetchone()
            if progress and progress[0] == "complete":
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("native backfill did not complete")
            time.sleep(0.05)
        archived_at = row(candidate)[1]
        initially_due_path = Path(row(initially_due)[2])
        with db() as conn:
            # Synthetic-only: retain the native row/header/locks, but represent
            # an archive that existed for 31 days before Retain was activated.
            conn.execute("UPDATE threads SET archived_at=? WHERE id=?",
                         (int(time.time()) - 31 * 86400, initially_due))
        enabled = cli("enable", "--days", "30", "--codex-home", str(codex_home),
            "--codex-bin", str(codex), "--no-schedule", "--yes")
        preview = cli("preview")
        check("enable_immediately_deletes_preexisting_due_native_archive",
              enabled["initial_cleanup"]["deleted"] == 1 and row(initially_due) is None
              and not initially_due_path.exists())
        check("actual_utility_enable_preserves_recent_archive_date",
              epoch(candidate) == archived_at and preview["eligible"] == 0,
              eligible=preview["eligible"], archived=preview["examined"])
        old_epoch = epoch(candidate)
        app.request("thread/unarchive", {"threadId": candidate})
        check("native_unarchive_clears_capture", epoch(candidate) is None)
        # Ensure the next epoch is distinguishable at SQLite's one-second resolution.
        while int(time.time()) <= old_epoch:
            time.sleep(0.05)
        app.request("thread/archive", {"threadId": candidate})
        check("native_rearchive_between_utility_runs_resets_capture", epoch(candidate) > old_epoch)
        app.request("thread/section/move", {"threadId": pinned, "sectionId": PINNED_SECTION, "beforeThreadId": None})
        age(pinned)
        preview = cli("preview")
        pinned_entry = next(entry for entry in preview["entries"] if entry["id"] == pinned)
        check("native_pinned_section_is_protected", pinned_entry["reason"] == "pinned_or_unknown_pin",
              native_legacy_pin_bit=row(pinned)[3], reason=pinned_entry["reason"])
        try:
            app.request("thread/resume", {"threadId": candidate, "excludeTurns": True})
        except RuntimeError as error:
            check("native_resume_rejects_archived_threads", "is archived" in str(error))
        else:
            raise AssertionError("native archived resume unexpectedly succeeded")
        app.request("thread/unarchive", {"threadId": candidate})
        app.request("thread/resume", {"threadId": candidate, "excludeTurns": True})
        check("native_live_active_thread_is_ignored", row(candidate)[0] == 0 and cli("run")["deleted"] == 0)
        # TEST ONLY: make the native-loaded synthetic thread an adversarial
        # stale archive candidate. Ordinary Codex APIs prevent this state.
        # Retention must still respect the native process's existing lock.
        live_path = Path(row(candidate)[2])
        adversarial_archive = codex_home / "archived_sessions" / live_path.name
        live_path.rename(adversarial_archive)
        with db() as conn:
            conn.execute("UPDATE threads SET archived=1,archived_at=unixepoch(),rollout_path=? WHERE id=?",
                         (str(adversarial_archive), candidate))
        age(candidate)
        run = cli("run", allowed=(0, 3))
        entry = next(entry for entry in run["entries"] if entry["id"] == candidate)
        check("live_codex_writer_blocks_actual_utility_cleanup", run["deleted"] == 0 and row(candidate) is not None
              and entry["reason"] == "changed_busy_or_error" and ".lock" in (entry.get("detail") or ""),
              reason=entry["reason"], detail=(entry.get("detail") or "").replace(str(root), "$FIXTURE"))
        preserved_active = Path(row(active)[2]).read_bytes()
        candidate_path = Path(row(candidate)[2])
        pinned_path = Path(row(pinned)[2])
        app.close()
        receipt["native_methods"] = app.methods
        receipt["native_stderr_tail"] = app.stderr_tail.decode(errors="replace")[-1500:]
        app = None
        run = cli("run")
        check("native_shutdown_allows_only_due_unpinned_archive_deletion",
              run["deleted"] == 1 and row(candidate) is None and not candidate_path.exists()
              and row(pinned) is not None and pinned_path.exists() and row(active)[0] == 0
              and Path(row(active)[2]).read_bytes() == preserved_active,
              deleted=run["deleted"], skipped=run["skipped"],
              logical_bytes_removed=run["logical_bytes_removed"],
              actual_reclaimed_bytes=run["actual_reclaimed_bytes"])
        check("repeat_actual_cleanup_is_idempotent", cli("run")["deleted"] == 0)
        cli("disable")
        with db() as conn:
            capture_count = conn.execute("SELECT count(*) FROM sqlite_schema WHERE name LIKE 'codex_retain_%'").fetchone()[0]
        check("disable_removes_capture_and_installs_no_agent",
              capture_count == 0 and not (home / "Library/LaunchAgents").exists())
        receipt["passed"] = True
    except Exception as error:
        receipt["error"] = str(error)
    finally:
        if app is not None:
            app.close()
            receipt["native_methods"] = app.methods
            receipt["native_stderr_tail"] = app.stderr_tail.decode(errors="replace")[-1500:]
        receipt["finished_at_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))
    return 0 if receipt["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
