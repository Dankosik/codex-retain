#!/usr/bin/env python3
"""Initialize template identity without changing Git state or dependency versions."""

import argparse
import difflib
import json
from pathlib import Path
import re
import sys


RESERVED = {
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
    "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
    "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static",
    "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    "abstract", "become", "box", "do", "final", "gen", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield", "union",
    "con", "prn", "aux", "nul", "clock", "test", "build", "deps", "examples",
    *{"com" + str(i) for i in range(10)},
    *{"lpt" + str(i) for i in range(10)},
}


def validate_identity(name, repository, description):
    if not re.fullmatch(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)*", name) or len(name) > 64:
        raise ValueError("name must be at most 64 lowercase letters, digits, and single hyphens, starting with a letter")
    if name in RESERVED:
        raise ValueError("name is reserved by Rust, Cargo, or a supported operating system")
    if not re.fullmatch(r"https://github\.com/[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})/[A-Za-z0-9_.-]+", repository):
        raise ValueError("repository must be an HTTPS GitHub repository URL without a trailing slash")
    if repository.rsplit("/", 1)[1] in {".", ".."}:
        raise ValueError("repository name is invalid")
    if not description.strip() or any(ord(c) < 32 or ord(c) == 127 for c in description):
        raise ValueError("description must be one nonempty line without control characters")


def read_file(root, relative):
    path = root / relative
    current = root
    for part in Path(relative).parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("refusing symlink in identity path: " + relative)
    if not path.is_file():
        raise ValueError("missing identity file: " + relative)
    return path.read_text(encoding="utf-8")


def write_file(path, text):
    with path.open("w", encoding="utf-8", newline="") as stream:
        stream.write(text)


def plan(root, name, repository, description):
    validate_identity(name, repository, description)
    state_path = ".template/identity.json"
    state_text = read_file(root, state_path)
    state = json.loads(state_text)
    if state.get("schema") != 1:
        raise ValueError("unsupported template identity schema")
    old_name = state["name"]
    old_repository = state["repository"]
    old_description = state["description"]
    old_prefix = state["environment_prefix"]
    new_prefix = name.replace("-", "_").upper()
    required = ["Cargo.toml", "Cargo.lock", "README.md", "src/main.rs", "tests/cli.rs"]
    texts = {relative: read_file(root, relative) for relative in required}
    readme_template = read_file(root, ".template/README.md")
    manifest = texts["Cargo.toml"]
    for expected in (
        'name = ' + json.dumps(old_name, ensure_ascii=False),
        'repository = ' + json.dumps(old_repository, ensure_ascii=False),
        'description = ' + json.dumps(old_description, ensure_ascii=False),
    ):
        if expected not in manifest:
            raise ValueError("Cargo.toml identity drift: missing " + expected)

    # Only the source-less root package name changes. Registry packages and
    # checksums remain byte-for-byte intact; Cargo validates the resulting lock.
    lock = texts["Cargo.lock"]
    root_entry = re.compile(r'(\[\[package\]\]\nname = ")' + re.escape(old_name) + r'("\nversion = "[^"\n]+"\n)(?=dependencies =|\n|$)')
    if len(root_entry.findall(lock)) != 1:
        raise ValueError("Cargo.lock must contain exactly one source-less template package")
    if state.get("initialized"):
        if (state["name"], state["repository"], state["description"]) == (name, repository, description):
            return {}
        raise ValueError("this checkout is already initialized; use an intentional project rename instead")

    candidates = set(required + ["AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md", "SECURITY.md", "examples/config.toml"])
    for directory, pattern in [("src", "*.rs"), ("tests", "*.rs"), ("docs", "*.md")]:
        candidates.update(p.relative_to(root).as_posix() for p in (root / directory).rglob(pattern))
    replacements = {
        old_repository: repository,
        old_prefix: new_prefix,
        old_name.replace("-", "_"): name.replace("-", "_"),
        old_name: name,
    }
    pattern = re.compile("|".join(re.escape(value) for value in sorted(replacements, key=len, reverse=True)))
    def replace(text):
        return pattern.sub(lambda match: replacements[match[0]], text)
    changes = {}
    for relative in sorted(candidates):
        path = root / relative
        if not path.exists() and relative not in required:
            continue
        before = texts.get(relative)
        if before is None:
            before = read_file(root, relative)
        if relative == "Cargo.lock":
            after = root_entry.sub(lambda match: match[1] + name + match[2], before)
        else:
            after = replace(before)
            if relative == "Cargo.toml":
                after = after.replace(replace('description = ' + json.dumps(old_description, ensure_ascii=False)), 'description = ' + json.dumps(description, ensure_ascii=False))
            if relative == "README.md":
                values = {"name": name, "repository": repository, "repository_name": repository.rsplit("/", 1)[1], "description": description, "environment_prefix": new_prefix}
                after = re.sub(r"\{\{([a-z_]+)\}\}", lambda match: values[match[1]], readme_template)
        if before != after:
            changes[relative] = (before, after)
    state.update(name=name, repository=repository, description=description,
                 environment_prefix=new_prefix, initialized=True)
    changes[state_path] = (state_text, json.dumps(state, indent=2, ensure_ascii=False) + "\n")
    return changes


def initialize(root, name, repository, description, dry_run=False):
    root = Path(root).absolute()
    changes = plan(root, name, repository, description)
    if dry_run:
        for relative, (before, after) in changes.items():
            sys.stdout.writelines(difflib.unified_diff(before.splitlines(True), after.splitlines(True), fromfile=relative, tofile=relative))
        return len(changes)
    written = []
    try:
        for relative, (_, after) in changes.items():
            written.append(relative)
            write_file(root / relative, after)
    except OSError:
        for relative in reversed(written):
            write_file(root / relative, changes[relative][0])
        raise
    return len(changes)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--description", default="A fast, small Rust command-line utility.")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    try:
        count = initialize(Path(__file__).absolute().parent.parent, args.name, args.repository, args.description, args.dry_run)
    except (OSError, ValueError, KeyError) as error:
        print("Initialization failed: " + str(error), file=sys.stderr)
        return 2
    if not args.dry_run:
        print("Initialized " + args.name + " (" + str(count) + " files changed)." if count else "Identity already matches; no files changed.")
        print("Run cargo test --locked, then review and commit the changes.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
