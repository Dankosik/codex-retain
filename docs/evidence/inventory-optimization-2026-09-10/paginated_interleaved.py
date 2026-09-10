#!/usr/bin/env python3
"""One fixed follow-up: five alternating pairs on an existing guarded matrix.

Each timed native invocation has three warmups. Restoration and full result
verification use the original matrix callbacks outside Hyperfine's timer.
"""

import argparse
import json
from pathlib import Path
import shlex
import statistics
import subprocess
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))
import performance_matrix as matrix
import benchmark


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = benchmark.benchmark_root(args.root)
    environment = json.loads((root / "environment.json").read_text())
    if environment.get("history_mode") != "paginated":
        raise ValueError("requires the independently verified paginated matrix")
    if args.output.exists():
        raise ValueError("refusing to overwrite an experiment")
    args.output.mkdir(parents=True)
    adapter = Path(__file__).with_name("paginated_matrix.py")
    cases = {}
    for tool in ("baseline", "candidate"):
        case_path = root / "cases" / f"cleanup-1000-{tool}.json"
        case = matrix.read_case(case_path)
        if matrix.digest(case["binary"]) != environment["binaries"][tool]["sha256"]:
            raise ValueError("binary differs from the original series")
        matrix.guard_policy(case)
        cases[tool] = (case_path, case)
    order = [tool for pair in range(5) for tool in (
        ("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline"))]
    plan = {"pairs": 5, "order": order, "warmups_per_sample": 3,
            "timed_runs_per_sample": 1, "original_environment": environment}
    (args.output / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    samples = []
    for index, tool in enumerate(order):
        case_path, case = cases[tool]
        env = benchmark.environment(case)
        native = shlex.join(["/usr/bin/env", "-i",
                            *[key + "=" + value for key, value in env.items()],
                            *matrix.command(case)])
        internal = [sys.executable, str(adapter)]
        output = args.output / f"{index + 1:02d}-{tool}-hyperfine.json"
        subprocess.run([
            "hyperfine", "--shell=none", "--warmup", "3", "--runs", "1", "--output=pipe",
            "--command-name", f"pair-{index // 2 + 1}-{tool}", "--export-json", str(output),
            "--prepare", shlex.join(internal + ["_prepare", "--case", str(case_path)]),
            "--conclude", shlex.join(internal + ["_validate", "--case", str(case_path)]),
            native,
        ], check=True)
        result = json.loads(output.read_text())["results"][0]
        if result["exit_codes"] != [0]:
            raise ValueError("unexpected timed exit code")
        receipt = matrix.verify(case_path)
        if matrix.digest(case["binary"]) != environment["binaries"][tool]["sha256"]:
            raise ValueError("binary changed during measurement")
        samples.append({"pair": index // 2 + 1, "tool": tool,
                        "seconds": result["times"][0], "verification": receipt})
    medians = {tool: statistics.median(s["seconds"] for s in samples if s["tool"] == tool)
               for tool in cases}
    summary = {"samples": samples, "medians_seconds": medians,
               "candidate_change_percent": (medians["candidate"] / medians["baseline"] - 1) * 100}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(medians))


if __name__ == "__main__":
    main()
