#!/usr/bin/env python3
"""Preview or apply a pinned skill revision from an explicit local Git checkout."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def git(source, *args):
    return subprocess.check_output(["git", "-C", str(source), *args])


def safe_path(root, relative):
    path = root
    parts = Path(relative).parts
    for index, part in enumerate(parts):
        path = path / part
        if path.is_symlink():
            raise ValueError("symlink in managed skill path: " + relative)
        if path.exists() and ((index < len(parts) - 1 and not path.is_dir()) or (index == len(parts) - 1 and not path.is_file())):
            raise ValueError("unexpected file type in managed skill path: " + relative)
    return path


def update_file(path, data):
    if data is None:
        path.unlink()
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)


def source_files(source, revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("revision must be a full immutable Git commit SHA")
    resolved = git(source, "rev-parse", revision + "^{commit}").decode().strip()
    if resolved != revision:
        raise ValueError("revision must identify the commit itself")
    records = git(source, "ls-tree", "-r", revision, "--", "skills").decode().splitlines()
    files = {}
    for record in records:
        metadata, path = record.split("\t", 1)
        if not re.fullmatch(r"skills/[a-z0-9-]+/SKILL\.md", path):
            raise ValueError("source is not a standalone SKILL.md-only pack: " + path)
        if metadata.split()[0] not in {"100644", "100755"}:
            raise ValueError("source skill must be a regular file")
        data = git(source, "show", revision + ":" + path)
        name = path.split("/")[1]
        if len(data) > 1_000_000 or not data.startswith(("---\nname: " + name + "\n").encode()):
            raise ValueError("unexpected skill structure: " + path)
        files[".agents/" + path] = data
    if not files:
        raise ValueError("source commit contains no skills")
    return files


def sync(root, source, revision, apply=False):
    manifest_path = safe_path(root, ".agents/skills-source.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    current_paths = {p.relative_to(root).as_posix() for p in (root / ".agents/skills").rglob("*") if p.is_file()}
    if current_paths != set(manifest["files"]):
        raise ValueError("local skill inventory has changed; preserve and reconcile those edits first")
    for relative, digest in manifest["files"].items():
        if not re.fullmatch(r"\.agents/skills/[a-z0-9-]+/SKILL\.md", relative):
            raise ValueError("unsafe local skill path")
        path = safe_path(root, relative)
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError("local skill changed; refusing to overwrite: " + relative)
    origin = git(source, "remote", "get-url", "origin").decode().strip()
    origin = re.sub(r"^git@github\.com:", "https://github.com/", origin).removesuffix(".git")
    if origin.lower() != manifest["source"].lower():
        raise ValueError("source repository differs from the pinned pack")
    files = source_files(source, revision)
    for relative in files:
        safe_path(root, relative)
    removed = sorted(set(manifest["files"]) - set(files))
    changed = sorted(path for path, data in files.items() if manifest["files"].get(path) != hashlib.sha256(data).hexdigest())
    print("Revision: " + manifest["revision"] + " -> " + revision)
    for path in changed:
        print("update " + path)
    for path in removed:
        print("remove " + path)
    drift = bool(changed or removed or manifest["revision"] != revision)
    if not apply:
        return 1 if drift else 0
    # All local content is verified before the first mutation. Only the pinned
    # skill inventory can be written or removed; application files are untouched.
    manifest.update(revision=revision, files={path: hashlib.sha256(data).hexdigest() for path, data in sorted(files.items())})
    updates = {relative: files[relative] for relative in changed}
    updates.update({relative: None for relative in removed})
    updates[".agents/skills-source.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
    backups = {relative: (root / relative).read_bytes() if (root / relative).exists() else None for relative in updates}
    attempted = []
    try:
        for relative, data in updates.items():
            attempted.append(relative)
            update_file(root / relative, data)
    except OSError:
        for relative in reversed(attempted):
            path = root / relative
            if backups[relative] is not None or path.exists():
                update_file(path, backups[relative])
        raise
    for relative in removed:
        parent = (root / relative).parent
        if not any(parent.iterdir()):
            parent.rmdir()
    print("Pinned skills updated. Review the diff and run make verify.")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true")
    mode.add_argument("--check", action="store_true", help="exit 1 when the selected revision differs (default)")
    args = parser.parse_args()
    try:
        return sync(Path(__file__).absolute().parent.parent, args.source, args.revision, args.apply)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print("Skill synchronization failed: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
