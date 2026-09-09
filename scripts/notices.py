#!/usr/bin/env python3
"""Assemble attribution texts from the exact locked Cargo package sources.

No third-party Python modules, builds, registry publication, or license decisions.
Run with --check in a source checkout whose Cargo sources have been fetched.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
START = "<!-- BEGIN GENERATED DEPENDENCY NOTICES -->"
END = "<!-- END GENERATED DEPENDENCY NOTICES -->"
FILE_LIMIT = 256 * 1024
METADATA_LIMIT = 16 * 1024 * 1024
CODEX_COMMIT = "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a"
CODEX_FILES = {
    "LICENSE": "d17f227e4df5da1600391338865ce0f3055211760a36688f816941d58232d8dc",
    "NOTICE": "9d71575ecfd9a843fc1677b0efb08053c6ba9fd686a0de1a6f5382fd3c220915",
}
GRANT_MARKERS = (
    "permission is hereby granted",
    "terms and conditions for use, reproduction",
    "redistribution and use in source and binary forms",
    "permission to use, copy, modify",
    "this is free and unencumbered software",
    "this software is provided 'as-is'",
    'this software is provided "as-is"',
)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_bounded(path: Path, limit: int = FILE_LIMIT) -> bytes:
    if not path.is_file():
        raise ValueError(f"not a regular source file: {path}")
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"source exceeds {limit} bytes: {path}")
    if not data.strip():
        raise ValueError(f"empty attribution source: {path}")
    return data


def metadata(cargo: str, offline: bool) -> dict:
    command = [cargo, "metadata", "--locked", "--format-version", "1"]
    if offline:
        command.append("--offline")
    with tempfile.TemporaryFile() as output:
        result = subprocess.run(
            command, cwd=ROOT, stdout=output, stderr=subprocess.PIPE,
            check=False, timeout=120,
        )
        if result.returncode:
            raise ValueError(f"cargo metadata failed: {result.stderr.decode(errors='replace')[:8192]}")
        output.seek(0)
        raw = output.read(METADATA_LIMIT + 1)
    if len(raw) > METADATA_LIMIT:
        raise ValueError("Cargo metadata exceeds the 16 MiB bound")
    return json.loads(raw)


def notice_record(root: Path, path: Path, texts: dict[str, str]) -> dict:
    relative = path.resolve().relative_to(root.resolve()).as_posix()
    data = read_bounded(path)
    text = data.decode("utf-8")
    sha = digest(data)
    texts.setdefault(sha, text)
    return {"path": relative, "sha256": sha, "bytes": len(data)}


def override_notice(package: dict, texts: dict[str, str], overrides: dict) -> dict | None:
    key = f"{package['name']} {package['version']}"
    override = overrides.get(key)
    if override is None:
        return None
    root = Path(package["manifest_path"]).parent
    if "packaged_vcs_commit" in override:
        vcs = json.loads(read_bounded(root / ".cargo_vcs_info.json"))
        if vcs["git"]["sha1"] != override["packaged_vcs_commit"]:
            raise ValueError("packaged VCS receipt does not match the reviewed license override")
    for name, expected in override.get("package_files", {}).items():
        if digest(read_bounded(root / name)) != expected:
            raise ValueError(f"package source differs from the reviewed license override: {name}")
    if "registry_file" in override:
        record = notice_record(root, root / override["registry_file"], texts)
    else:
        directory = ROOT / "docs/evidence/licenses"
        record = notice_record(directory, directory / override["vendored_file"], texts)
    if record["sha256"] != override["sha256"]:
        raise ValueError("license override text has changed; review its original source")
    return {**record, "reviewed_override": override}


def package_notices(package: dict, texts: dict[str, str], overrides: dict) -> dict:
    root = Path(package["manifest_path"]).parent
    candidates = {
        child for child in root.iterdir()
        if child.is_file() and re.match(r"^(LICENSE|LICENCE|COPYING|NOTICE)(?:$|[._-])", child.name, re.I)
    }
    if package["license_file"]:
        candidates.add(root / package["license_file"])
    records = [notice_record(root, path, texts) for path in sorted(candidates)]
    if not records:
        override = override_notice(package, texts, overrides)
        if override is not None:
            records.append(override)
    if not records:
        raise ValueError("no LICENSE/COPYING/NOTICE or declared license-file")
    if not any(
        any(marker in texts[item["sha256"]].lower() for marker in GRANT_MARKERS)
        for item in records
    ):
        raise ValueError("no substantive license grant found; review package license sources")
    return {
        "id": package["id"], "name": package["name"],
        "version": package["version"], "license": package["license"],
        "source": package["source"], "files": records,
    }


def sqlite_notice(package: dict, texts: dict[str, str]) -> dict:
    """SQLite's checked public-domain declaration is its first header comment."""
    root = Path(package["manifest_path"]).parent
    path = root / "sqlite3/sqlite3.h"
    with path.open("rb") as stream:
        prefix = stream.read(16 * 1024)
    match = re.match(rb"/\*.*?\*/", prefix, re.S)
    if not match or b"The author disclaims copyright to this source code." not in match[0]:
        raise ValueError("bundled SQLite header no longer has the reviewed copyright declaration")
    data = match[0]
    sha = digest(data)
    texts.setdefault(sha, data.decode("utf-8"))
    return {
        "name": "SQLite C amalgamation", "origin": f"{package['name']} {package['version']}",
        "license": "Public domain declaration in the bundled source header",
        "files": [{"path": "sqlite3/sqlite3.h (first comment only)", "sha256": sha, "bytes": len(data)}],
    }


def supplemental_notices(packages: list[dict], codex: Path, texts: dict[str, str]) -> list[dict]:
    sqlite = [p for p in packages if p["name"] == "libsqlite3-sys"]
    zstd = [p for p in packages if p["name"] == "zstd-sys"]
    if len(sqlite) != 1 or len(zstd) != 1:
        raise ValueError("review bundled-C attribution after a SQLite/zstd dependency change")
    notices = [sqlite_notice(sqlite[0], texts)]
    root = Path(zstd[0]["manifest_path"]).parent
    notices.append({
        "name": "Zstandard C library", "origin": f"zstd-sys {zstd[0]['version']}",
        "license": "BSD-3-Clause option (zstd/LICENSE); alternative GPL terms are not selected",
        "files": [notice_record(root, root / "zstd/LICENSE", texts)],
    })
    codex_records = []
    for name, expected in CODEX_FILES.items():
        record = notice_record(codex, codex / name, texts)
        if record["sha256"] != expected:
            raise ValueError(f"Codex {name} differs from the reviewed {CODEX_COMMIT} source")
        codex_records.append(record)
    notices.append({
        "name": "OpenAI Codex compatibility source material",
        "origin": f"https://github.com/openai/codex/tree/{CODEX_COMMIT}",
        "license": "Apache-2.0; upstream NOTICE retained verbatim",
        "files": codex_records,
    })
    return notices


def compiler_notices(rustc: str) -> tuple[dict, bytes]:
    def query(*arguments: str) -> str:
        result = subprocess.run([rustc, *arguments], check=False, capture_output=True, timeout=10)
        if result.returncode or len(result.stdout) > 64 * 1024:
            raise ValueError(f"rustc {' '.join(arguments)} failed or exceeded its output bound")
        return result.stdout.decode("utf-8").strip()

    version = query("--version", "--verbose")
    required = re.search(r'^rust-version\s*=\s*"([^"]+)"', read_bounded(ROOT / "Cargo.toml").decode(), re.M)
    release = re.search(r"^release: (.+)$", version, re.M)
    if not required or not release or release[1] != required[1]:
        raise ValueError("generate release notices with the exact rust-version declared in Cargo.toml")
    sysroot = Path(query("--print", "sysroot"))
    # This is the compiler distribution's full standard-library attribution
    # bundle, not one license file; retain its original HTML bytes intact.
    data = read_bounded(sysroot / "share/doc/rust/COPYRIGHT-library.html", 8 * 1024 * 1024)
    return {
        "compiler": version,
        "source": "share/doc/rust/COPYRIGHT-library.html in the identified Rust distribution",
        "artifact": "RUST_STDLIB_NOTICES.html", "sha256": digest(data), "bytes": len(data),
    }, data


def block_id(sha: str) -> str:
    return f"license-{sha}"


def references(files: list[dict]) -> str:
    return ", ".join(f"[{item['path']}](#{block_id(item['sha256'])})" for item in files)


def render(manifest: dict, texts: dict[str, str]) -> str:
    lines = [
        START, "", "## Resolved dependency attribution", "",
        "Generated by `scripts/notices.py` from `cargo metadata --locked --format-version 1`.",
        f"Cargo.lock SHA-256: `{manifest['cargo_lock_sha256']}`.", "",
        "The inventory conservatively includes every resolved package for the project's default",
        "features: normal, build, development, and platform-conditional dependencies. Inclusion",
        "does not mean a package is linked into every distributed executable. Original supplied",
        "license alternatives and attribution texts are reproduced without rewriting their terms.",
        "Markdown text blocks normalize line endings; recorded hashes identify the original source bytes.", "",
        f"The `{manifest['rust_standard_library']['compiler'].splitlines()[0]}` distribution's",
        "complete standard-library copyright bundle is reproduced byte-for-byte in",
        "[RUST_STDLIB_NOTICES.html](RUST_STDLIB_NOTICES.html). Its compiler identity, byte count,",
        "and SHA-256 appear in `docs/evidence/dependency-licenses.json`. This separate aggregate",
        "bundle has an 8 MiB extraction limit; individual Cargo license files have a 256 KiB limit.", "",
        "Regenerate after dependency changes:", "",
        "```sh",
        "python3 scripts/notices.py --codex-source /path/to/codex-rust-v0.153.4",
        "python3 scripts/notices.py --codex-source /path/to/codex-rust-v0.153.4 --check",
        "```", "",
        "Use `--offline` when the locked package sources are already in `CARGO_HOME`.",
        "Codex LICENSE/NOTICE hashes are checked against the reviewed source revision. Missing,",
        "oversized, non-UTF-8, or unrecognized package license texts fail generation; none are",
        "silently omitted. Each complete source file is limited to 256 KiB. The SQLite entry",
        "explicitly extracts the checked copyright comment rather than copying its C header.", "",
        "Three reviewed nonstandard cases are recorded in `docs/evidence/licenses/sources.json`:",
        "defmt-parser and difflib omit license files from their Cargo packages, so pinned upstream",
        "copies are included with source identity checks; r-efi places its grant and copyright",
        "notices in the packaged AUTHORS file. These exceptions are exact-version and hash bound.", "",
        "The compatibility schema and fixtures adapt inspected Codex storage definitions; the",
        "Codex executable and its TUI are not included. The source project's complete NOTICE is",
        "retained below for attribution provenance.", "",
        "| Package | Declared license | Supplied texts |",
        "| --- | --- | --- |",
    ]
    for package in manifest["packages"]:
        lines.append(f"| {package['name']} {package['version']} | {package['license'] or 'license-file'} | {references(package['files'])} |")
    lines.extend(["", "### Bundled C libraries and compatibility sources", ""])
    for item in manifest["supplemental"]:
        lines.extend([f"- **{item['name']}** — {item['origin']}; {item['license']}. {references(item['files'])}"])
    lines.extend(["", f"Unmatched package licenses: **{len(manifest['unmatched'])}**.", "", "### License and attribution texts", ""])
    # Only identical full byte sequences share a block; different copyright
    # holders retain their own text even when they use the same SPDX license.
    for sha, text in sorted(texts.items()):
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        fence = "`" * max(4, max((len(m[0]) + 1 for m in re.finditer(r"`+", text)), default=0))
        lines.extend([f'<a id="{block_id(sha)}"></a>', "", f"#### Source text SHA-256 `{sha}`", "", f"{fence}text", text.rstrip("\n"), fence, ""])
    lines.extend([END, ""])
    return "\n".join(lines)


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", default=os.environ.get("CARGO", "cargo"))
    parser.add_argument("--rustc", default=os.environ.get("RUSTC", "rustc"))
    parser.add_argument("--codex-source", required=True, type=Path)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--check", action="store_true", help="compare outputs without modifying files")
    args = parser.parse_args()
    locked = read_bounded(ROOT / "Cargo.lock")
    resolved = metadata(args.cargo, args.offline)
    if locked != read_bounded(ROOT / "Cargo.lock"):
        raise ValueError("Cargo.lock changed during metadata collection; retry after dependency work")
    workspace = set(resolved["workspace_members"])
    packages = sorted(
        (p for p in resolved["packages"] if p["id"] not in workspace),
        key=lambda p: (p["name"], p["version"], p["id"]),
    )
    texts: dict[str, str] = {}
    overrides = json.loads(read_bounded(ROOT / "docs/evidence/licenses/sources.json"))["packages"]
    collected, unmatched = [], []
    for package in packages:
        try:
            collected.append(package_notices(package, texts, overrides))
        except (ValueError, OSError, UnicodeError) as error:
            unmatched.append(f"{package['name']} {package['version']}: {error}")
    if unmatched:
        raise ValueError("unmatched package licenses (outputs preserved):\n  " + "\n  ".join(unmatched))
    compiler_record, compiler_text = compiler_notices(args.rustc)
    manifest = {
        "schema": 1, "generator": "scripts/notices.py",
        "cargo_lock_sha256": digest(locked),
        "scope": "All resolved default-feature packages, including build/dev/platform-conditional dependencies",
        "packages": collected,
        "supplemental": supplemental_notices(packages, args.codex_source, texts),
        "rust_standard_library": compiler_record,
        "unmatched": [],
    }
    notices = ROOT / "THIRD_PARTY_NOTICES.md"
    existing = read_bounded(notices, 8 * 1024 * 1024).decode("utf-8")
    if START in existing:
        before, remainder = existing.split(START, 1)
        if END not in remainder:
            raise ValueError("generated notices end marker missing; preserve file and review")
        _, after = remainder.split(END, 1)
        content = before.rstrip() + "\n\n" + render(manifest, texts) + after.lstrip("\n")
    else:
        content = existing.rstrip() + "\n\n" + render(manifest, texts)
    outputs = {
        notices: content.encode("utf-8"),
        ROOT / "docs/evidence/dependency-licenses.json": (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8"),
        ROOT / "RUST_STDLIB_NOTICES.html": compiler_text,
    }
    if args.check:
        stale = [str(p.relative_to(ROOT)) for p, data in outputs.items() if not p.exists() or p.read_bytes() != data]
        if stale:
            raise ValueError("attribution outputs need regeneration: " + ", ".join(stale))
    else:
        for path, data in outputs.items():
            atomic_write(path, data)
    print(f"{'Checked' if args.check else 'Generated'} {len(collected)} packages, {len(manifest['supplemental'])} supplemental entries, {len(texts)} distinct texts; unmatched=0")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
