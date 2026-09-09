#!/usr/bin/env python3
"""Measure release binaries on disposable, marker-checked synthetic Codex data.

Requires hyperfine and a built Codex Session Janitor. No dependency installation,
real-chat access, background task installation, or release build is performed.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
import fixture  # noqa: E402


SCRIPT = Path(__file__).resolve()
BENCHMARK_MARKER = ".codex-retain-benchmark.json"
JANITOR_REVISION = "32737c7cc68a74a63a21e5b8a403e92f4f1398e6"


def execute(command, env=None, timeout=300):
    result = subprocess.run(command, env=env, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=timeout, check=False)
    if result.returncode:
        raise ValueError(f"command failed ({result.returncode}): {shlex.join(command)}\n"
                         + result.stderr[-4000:] + result.stdout[-1000:])
    return result.stdout


def executable(value):
    path = shutil.which(str(value))
    if path is None:
        raise ValueError("executable not found: " + str(value))
    return str(Path(path).resolve())


def benchmark_root(path):
    root = fixture.no_symlinks(path)
    marker = root / BENCHMARK_MARKER
    if marker.is_symlink() or not marker.is_file():
        raise ValueError("not a marked benchmark directory: " + str(root))
    data = json.loads(marker.read_text(encoding="utf-8"))
    if data.get("kind") != "codex-retain-benchmark" or data.get("root") != str(root):
        raise ValueError("benchmark marker does not identify this directory")
    return root


def read_case(path):
    path = fixture.no_symlinks(path)
    case = json.loads(path.read_text(encoding="utf-8"))
    root = benchmark_root(case["benchmark_root"])
    if path.parent != root / "cases" or not re.fullmatch(r"[a-z]+-[0-9]+\.json", path.name):
        raise ValueError("case descriptor is outside its benchmark directory")
    name = path.stem
    case_root = root / "fixtures" / name
    if Path(case["fixture_root"]) != case_root:
        raise ValueError("case fixture path does not match its descriptor")
    if case["operation"] not in {"preview", "cleanup"} or case["tool"] not in {"retain", "janitor"}:
        raise ValueError("unknown benchmark operation")
    fixture.no_symlinks(case_root)
    return case


def environment(case):
    root = Path(case["fixture_root"])
    home = root / "home"
    temporary = home / "tmp"
    temporary.mkdir(exist_ok=True)
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(home), "CODEX_HOME": str(root / "codex"),
        "XDG_CONFIG_HOME": str(home / ".config"), "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_CACHE_HOME": str(home / ".cache"), "TMPDIR": str(temporary),
        "LANG": "en_US.UTF-8", "LC_ALL": "C", "NO_COLOR": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
    }


def command(case):
    root = Path(case["fixture_root"])
    if case["tool"] == "retain":
        return [case["retain"], "--state-dir", str(root / "state"), "--json",
                "run" if case["operation"] == "cleanup" else "preview"]
    result = [case["node"], str(Path(case["janitor"]) / "dist/cli.js"),
              "clean" if case["operation"] == "cleanup" else "scan",
              "--codex-home", str(root / "codex"), "--retention-days", "30"]
    if case["operation"] == "cleanup":
        result.extend(["--confirm", "--mode", "delete"])
    return result


def prepare(case_path):
    started = time.perf_counter()
    case = read_case(case_path)
    root = Path(case["fixture_root"])
    if root.exists():
        fixture.remove_owned(root)
    fixture.create(root, case["count"], case["rollout_bytes"], case["now"])
    if case["tool"] == "retain":
        output = execute(
            [case["retain"], "--state-dir", str(root / "state"), "--json", "enable",
             "--codex-home", str(root / "codex"), "--codex-bin", case["codex_bin"],
             "--days", "30", "--no-schedule", "--yes"], env=environment(case),
        )
        (root / "enable-result.json").write_text(output, encoding="utf-8")
        # This bypasses onboarding grace ONLY in marker-checked generated data.
        # It must never be suggested as a way to change a real retention policy.
        fixture.owned_root(root)
        database = root / "codex" / "state_5.sqlite"
        with sqlite3.connect(database) as connection:
            columns = {row[1] for row in connection.execute("PRAGMA table_info(codex_retain_epochs)")}
            if "archived_since" not in columns:
                raise ValueError("fixture aging needs codex_retain_epochs.archived_since")
            changed = connection.execute("UPDATE codex_retain_epochs SET archived_since = ?",
                                         (case["now"] - 40 * fixture.DAY,)).rowcount
            if changed != case["count"]:
                raise ValueError(f"expected {case['count']} seeded epochs, got {changed}")
            connection.commit()
            connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    fixture.verify(root)
    elapsed = time.perf_counter() - started
    with (Path(case["benchmark_root"]) / "preparation.jsonl").open("a", encoding="utf-8") as stream:
        stream.write(json.dumps({"case": Path(case_path).stem, "seconds": elapsed}) + "\n")


def validate(case_path):
    case = read_case(case_path)
    removed = case["operation"] == "cleanup"
    expected_rows = 0 if removed and case["tool"] == "retain" else case["count"]
    return fixture.verify(case["fixture_root"], expect_removed=removed,
                          expect_thread_rows=expected_rows)


def preflight(case_path):
    prepare(case_path)
    case = read_case(case_path)
    output = execute(command(case), env=environment(case))
    result = validate(case_path)
    if case["tool"] == "retain":
        report = json.loads(output)
        count_key = "deleted" if case["operation"] == "cleanup" else "eligible"
        if report.get(count_key) != case["count"] or report.get("skipped") != 0:
            raise ValueError("Retain did not select the entire comparable fixture: " + output[:1000])
        if case["operation"] == "cleanup" and report.get("logical_bytes_removed") != case["count"] * case["rollout_bytes"]:
            raise ValueError("Retain logical byte report does not match the fixture")
        result["command_output"] = report
    else:
        result["command_output"] = output
    fixture.write_json(Path(case["benchmark_root"]) / (Path(case_path).stem + "-preflight.json"), result)


def timed_command(case):
    # shell=none plus shlex.join prevents interpolation of paths or environment.
    return shlex.join(["/usr/bin/env", "-i", *[key + "=" + value for key, value in environment(case).items()],
                       *command(case)])


def measure_case(case_path, hyperfine, runs):
    case = read_case(case_path)
    root = Path(case["benchmark_root"])
    name = Path(case_path).stem
    prepare_command = shlex.join([sys.executable, str(SCRIPT), "_prepare", "--case", str(case_path)])
    validate_command = shlex.join([sys.executable, str(SCRIPT), "_validate", "--case", str(case_path)])
    # Preflight left a consumed fixture for cleanup; create the environment
    # directories before constructing the literal timed command.
    literal = timed_command(case)
    subprocess.run(
        [hyperfine, "--shell=none", "--warmup", "3", "--runs", str(runs), "--output=pipe",
         "--prepare", prepare_command, "--conclude", validate_command,
         "--command-name", name, "--export-json", str(root / (name + "-hyperfine.json")), literal],
        check=True,
    )
    if platform.system() == "Darwin":
        prepare(case_path)
        with (root / (name + "-time.txt")).open("w", encoding="utf-8") as stream:
            subprocess.run(["/usr/bin/time", "-l", *command(case)], env=environment(case),
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stream, check=True)
        validate(case_path)


def version(command, env=None):
    return execute(command, env=env, timeout=30).strip()


def measure_startup(root, first_case, hyperfine, runs):
    case = read_case(first_case)
    with tempfile.TemporaryDirectory(prefix="startup-version-probe-", dir=root) as temporary:
        normal_env = environment(case)
        probe_env = dict(normal_env, HOME=temporary,
                         CODEX_HOME=str(Path(temporary) / "codex"), TMPDIR=temporary)
        commands = [
            ("retain-help", [case["retain"], "--help"], normal_env),
            ("janitor-help", [case["node"], str(Path(case["janitor"]) / "dist/cli.js"), "--help"], normal_env),
            ("codex-version-probe", [case["codex_bin"], "--version"], probe_env),
        ]
        invocation = [hyperfine, "--shell=none", "--warmup", "3", "--runs", str(runs), "--output=pipe",
                      "--export-json", str(root / "startup-hyperfine.json")]
        for name, arguments, env in commands:
            prefix = ["/usr/bin/env", "-i", *[key + "=" + value for key, value in env.items()]]
            invocation.extend(["--command-name", name, shlex.join(prefix + arguments)])
        subprocess.run(invocation, check=True)


def summarize(root):
    measurements = []
    for path in sorted(root.glob("*-hyperfine.json")):
        for result in json.loads(path.read_text(encoding="utf-8"))["results"]:
            measurements.append({"source": path.name, **result})
    accounting = []
    for path in sorted(root.glob("*-time.txt")):
        content = path.read_text(encoding="utf-8")
        match = re.search(r"(\d+)\s+maximum resident set size", content)
        accounting.append({"source": path.name,
                           "maximum_resident_set_size_bytes": int(match[1]) if match else None})
    fixture.write_json(root / "summary.json", {"measurements": measurements,
                                              "macos_process_accounting": accounting,
                                              "physical_disk_reads": None})


def setup(args):
    root = fixture.no_symlinks(args.root)
    if root.exists():
        raise ValueError("benchmark output directory must not exist: " + str(root))
    retain, codex_bin, node = map(executable, (args.retain, args.codex_bin, args.node))
    hyperfine = executable(args.hyperfine)
    janitor = Path(args.janitor).resolve(strict=True)
    if not (janitor / "dist/cli.js").is_file():
        raise ValueError("build the pinned Janitor before measuring; missing dist/cli.js")
    revision = version(["git", "-C", str(janitor), "rev-parse", "HEAD"])
    if revision != JANITOR_REVISION:
        raise ValueError("Janitor revision differs from the reviewed baseline: " + revision)
    dirty = version(["git", "-C", str(janitor), "status", "--porcelain", "--untracked-files=no"])
    if dirty:
        raise ValueError("Janitor tracked files are modified; use a pristine baseline")
    if any(not 0 <= count <= 100000 for count in args.counts) or not 3 <= args.runs <= 100:
        raise ValueError("counts must be 0..100000 and runs 3..100")
    if len(set(args.counts)) != len(args.counts):
        raise ValueError("fixture counts must be distinct")
    root.mkdir(parents=True, mode=0o700)
    fixture.write_json(root / BENCHMARK_MARKER,
                       {"kind": "codex-retain-benchmark", "schema": 1, "root": str(root)})
    (root / "cases").mkdir()
    (root / "fixtures").mkdir()
    now = int(time.time())
    common = {"benchmark_root": str(root), "retain": retain, "codex_bin": codex_bin,
              "node": node, "janitor": str(janitor), "rollout_bytes": args.rollout_bytes, "now": now}
    cases = []
    operations = ["preview", "cleanup"] if args.only == "all" else [args.only]
    for count in args.counts:
        for operation in operations:
            # Alternate order across fixture sizes; report each sample set,
            # including variance, instead of choosing the fastest trial.
            tool_order = ("retain", "janitor") if args.counts.index(count) % 2 == 0 else ("janitor", "retain")
            for tool in tool_order:
                name = tool + operation + "-" + str(count)
                case = dict(common, tool=tool, operation=operation, count=count,
                            fixture_root=str(root / "fixtures" / name))
                path = root / "cases" / (name + ".json")
                fixture.write_json(path, case)
                cases.append(path)
    with tempfile.TemporaryDirectory(prefix="version-probe-", dir=root) as temporary:
        probe_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": temporary,
                     "CODEX_HOME": str(Path(temporary) / "codex"), "TMPDIR": temporary, "LC_ALL": "C"}
        codex_version = version([codex_bin, "--version"], env=probe_env)
    metadata = {
        "schema": 1, "date_utc": fixture.iso(now), "platform": platform.platform(),
        "architecture": platform.machine(), "python": platform.python_version(),
        "hyperfine": version([hyperfine, "--version"]), "node": version([node, "--version"]),
        "codex": codex_version, "retain": version([retain, "--version"]),
        "retain_sha256": hashlib.sha256(Path(retain).read_bytes()).hexdigest(),
        "retain_binary_bytes": Path(retain).stat().st_size,
        "janitor_revision": revision,
        "janitor_package": json.loads((janitor / "package.json").read_text(encoding="utf-8")),
        "janitor_lock_sha256": hashlib.sha256((janitor / "package-lock.json").read_bytes()).hexdigest(),
        "janitor_cli_sha256": hashlib.sha256((janitor / "dist/cli.js").read_bytes()).hexdigest(),
        "counts": args.counts, "rollout_bytes": args.rollout_bytes, "warmups": 3, "runs": args.runs,
        "cache": "warm filesystem cache; fixtures freshly generated before each sample",
        "physical_disk_reads": "not measured", "background_services_installed": False,
        "limits": ["Janitor deletes rollout files; Retain also maintains verified SQLite thread state.",
                   "Neither measured cleanup mode retains backup or Trash copies.",
                   "All fixture threads are expired archives; correctness edge cases require separate tests.",
                   "macOS time maximum RSS is not summed simultaneous process-tree RSS."],
    }
    if platform.system() == "Darwin":
        metadata["macos"] = version(["sw_vers"])
        metadata["cpu"] = version(["sysctl", "-n", "machdep.cpu.brand_string"])
        metadata["physical_memory_bytes"] = version(["sysctl", "-n", "hw.memsize"])
    fixture.write_json(root / "environment.json", metadata)
    return root, cases, hyperfine


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    run = sub.add_parser("run", help="create fixtures, preflight, then collect comparable measurements")
    run.add_argument("--root", required=True, type=Path, help="new scratch/output directory")
    run.add_argument("--retain", required=True, help="already built release codex-retain executable")
    run.add_argument("--codex-bin", required=True, help="real compatible Codex executable")
    run.add_argument("--janitor", required=True, help="pristine built checkout at the documented baseline commit")
    run.add_argument("--node", default="node")
    run.add_argument("--hyperfine", default="hyperfine")
    run.add_argument("--counts", nargs="+", type=int, default=[1000, 10000])
    run.add_argument("--rollout-bytes", type=int, default=4096)
    run.add_argument("--runs", type=int, default=10)
    run.add_argument("--only", choices=["all", "preview", "cleanup"], default="all")
    run.add_argument("--prepare-only", action="store_true", help="perform isolated preflight without timed runs")
    for name in ("_prepare", "_validate"):
        internal = sub.add_parser(name, help="internal Hyperfine lifecycle command")
        internal.add_argument("--case", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.action == "_prepare":
            prepare(args.case)
        elif args.action == "_validate":
            validate(args.case)
        else:
            root, cases, hyperfine = setup(args)
            for path in cases:
                print("Preflight " + path.stem, flush=True)
                preflight(path)
            if not args.prepare_only:
                if args.only == "all":
                    measure_startup(root, cases[0], hyperfine, args.runs)
                for path in cases:
                    print("Measuring " + path.stem, flush=True)
                    measure_case(path, hyperfine, args.runs)
                summarize(root)
            print("Evidence: " + str(root))
    except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
        print("Benchmark error: " + str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
