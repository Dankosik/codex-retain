#!/usr/bin/env python3
"""Build release archives from a tested native binary; no publication or credentials."""

import argparse
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
OPTIONAL_NOTICES = ("NOTICE", "THIRD_PARTY_NOTICES", "THIRD_PARTY_NOTICES.md")


@dataclass(frozen=True)
class Identity:
    binary: str
    version: str
    repository: str
    target_directory: Path

    def archive_root(self, target):
        return f"{self.binary}-{self.version}-{target}"

    def executable(self, target):
        return self.binary + (".exe" if "windows" in target else "")

    def archive_name(self, target):
        suffix = ".zip" if "windows" in target else ".tar.gz"
        return self.archive_root(target) + suffix


def run_text(command):
    return subprocess.run(
        command, cwd=ROOT, check=True, text=True, capture_output=True
    ).stdout.strip()


def identity():
    metadata = json.loads(
        run_text(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"])
    )
    packages = [
        package for package in metadata["packages"]
        if Path(package["manifest_path"]).resolve() == ROOT / "Cargo.toml"
    ]
    if len(packages) != 1:
        raise ValueError("release requires one root Cargo package")
    package = packages[0]
    binaries = [target["name"] for target in package["targets"] if "bin" in target["kind"]]
    if len(binaries) != 1:
        raise ValueError("release requires exactly one binary target")
    if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]{0,63}", binaries[0]):
        raise ValueError("binary name is not safe for release filenames")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?", package["version"]):
        raise ValueError("invalid release version")
    return Identity(
        binaries[0], package["version"], package.get("repository") or "",
        Path(metadata["target_directory"]),
    )


def native_target():
    for line in run_text(["rustc", "-vV"]).splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise ValueError("rustc did not report a host target")


def check_tag(info, tag, repository):
    if tag != "v" + info.version:
        raise ValueError(f"tag must equal v{info.version}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("repository must be an owner/name pair")
    if info.repository.rstrip("/") != "https://github.com/" + repository:
        raise ValueError("Cargo package repository does not match the release repository")
    head = run_text(["git", "rev-parse", "HEAD"])
    tagged = run_text(["git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"])
    if tagged != head:
        raise ValueError("release tag does not identify the checked-out commit")
    if run_text(["git", "status", "--porcelain", "--untracked-files=no"]):
        raise ValueError("tracked source files changed after checkout")
    print(f"Verified {repository} {tag} at {head}")


def smoke(executable, info):
    result = subprocess.run(
        [str(executable), "--version"], capture_output=True, timeout=10, check=True
    )
    if result.stdout.strip() != f"{info.binary} {info.version}".encode() or result.stderr:
        raise ValueError("extracted binary has unexpected version output")
    result = subprocess.run(
        [str(executable), "--format", "json", "stats", "-"],
        input=b"one\ntwo\nlast", capture_output=True, timeout=10, check=True,
    )
    if result.stdout != b'{"bytes":12,"lines":2}\n' or result.stderr:
        raise ValueError("extracted binary failed the streaming CLI smoke test")


def inspect_archive(archive, info, target, destination=None):
    """Check an exact archive inventory and optionally extract only its executable."""
    if archive.name != info.archive_name(target):
        raise ValueError("archive name does not match package, version, and target")
    prefix = info.archive_root(target) + "/"
    executable = prefix + info.executable(target)
    required = {executable, prefix + "README.md", prefix + "LICENSE"}
    allowed = required | {prefix + item for item in OPTIONAL_NOTICES}

    def check_members(members):
        names = [name for name, _, _ in members]
        if len(names) != len(set(names)) or not required.issubset(names):
            raise ValueError("archive contains duplicate entries or lacks required files")
        if not set(names).issubset(allowed):
            raise ValueError("archive contains unexpected paths")
        for name, regular, mode in members:
            if not regular:
                raise ValueError(f"archive member is not a regular file: {name}")
            if name == executable and "windows" not in target and not mode & 0o111:
                raise ValueError("archive binary has lost its executable permission")

    if "windows" in target:
        with zipfile.ZipFile(archive) as source:
            members = source.infolist()
            check_members([
                (member.filename,
                 not member.is_dir() and stat.S_IFMT(member.external_attr >> 16) in (0, stat.S_IFREG),
                 member.external_attr >> 16)
                for member in members
            ])
            if destination:
                with source.open(executable) as content, destination.open("wb") as output:
                    shutil.copyfileobj(content, output)
    else:
        with tarfile.open(archive, "r:gz") as source:
            members = source.getmembers()
            check_members([(member.name, member.isfile(), member.mode) for member in members])
            if destination:
                with source.extractfile(executable) as content, destination.open("wb") as output:
                    shutil.copyfileobj(content, output)
    if destination:
        destination.chmod(0o755)


def verify_archive(archive, info, target):
    if native_target() != target:
        raise ValueError("a native runner is required to verify the extracted binary")
    with tempfile.TemporaryDirectory(prefix="cli-release-") as temporary:
        executable = Path(temporary) / info.executable(target)
        inspect_archive(archive, info, target, executable)
        smoke(executable, info)
    print(f"Verified extracted binary: {archive.name}")


def package(info, target, dist):
    if native_target() != target:
        raise ValueError("release packaging must run on the target's native runner")
    binary = info.target_directory / target / "release" / info.executable(target)
    files = [(binary, info.executable(target)), (ROOT / "README.md", "README.md"), (ROOT / "LICENSE", "LICENSE")]
    files.extend((ROOT / name, name) for name in OPTIONAL_NOTICES if (ROOT / name).exists())
    if any(path.is_symlink() or not path.is_file() for path, _ in files):
        raise ValueError("release binary, README, and license must be regular files")
    dist.mkdir(parents=True, exist_ok=True)
    archive = dist / info.archive_name(target)
    if archive.exists() or archive.is_symlink():
        raise ValueError(f"archive already exists: {archive}")
    with tempfile.TemporaryDirectory(prefix="cli-package-", dir=dist) as temporary:
        candidate = Path(temporary) / archive.name
        prefix = info.archive_root(target) + "/"
        if "windows" in target:
            with zipfile.ZipFile(candidate, "w", compression=zipfile.ZIP_DEFLATED) as output:
                for path, name in files:
                    output.write(path, prefix + name)
        else:
            with tarfile.open(candidate, "w:gz") as output:
                for path, name in files:
                    output.add(path, arcname=prefix + name, recursive=False)
        verify_archive(candidate, info, target)
        candidate.replace(archive)
    print(archive)


def checksums(info, dist):
    expected = {info.archive_name(target): target for target in TARGETS}
    actual = {path.name for path in dist.iterdir() if path.name != "SHA256SUMS"}
    if actual != set(expected):
        raise ValueError(f"incomplete release set; missing={sorted(set(expected) - actual)}, unexpected={sorted(actual - set(expected))}")
    lines = []
    for name in sorted(expected):
        path = dist / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"release artifact is not a regular file: {name}")
        inspect_archive(path, info, expected[name])
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(65536), b""):
                digest.update(chunk)
        lines.append(f"{digest.hexdigest()}  {name}\n")
    destination = dist / "SHA256SUMS"
    if destination.is_symlink():
        raise ValueError("checksum destination must not be a symlink")
    destination.write_text("".join(lines), encoding="utf-8")
    print(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    tag = commands.add_parser("check-tag", help="check tag, repository, commit, and tracked source")
    tag.add_argument("--tag", required=True)
    tag.add_argument("--repository", required=True)
    pack = commands.add_parser("package", help="archive and smoke-test an already built native binary")
    pack.add_argument("--target", choices=TARGETS, required=True)
    pack.add_argument("--dist", type=Path, default=Path("dist"))
    verify = commands.add_parser("verify", help="smoke-test the binary extracted from an archive")
    verify.add_argument("--target", choices=TARGETS, required=True)
    verify.add_argument("--archive", type=Path, required=True)
    sums = commands.add_parser("checksums", help="validate all release targets and write SHA256SUMS")
    sums.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args()
    try:
        info = identity()
        if args.command == "check-tag":
            check_tag(info, args.tag, args.repository)
        elif args.command == "package":
            package(info, args.target, args.dist.resolve())
        elif args.command == "verify":
            verify_archive(args.archive.resolve(), info, args.target)
        else:
            checksums(info, args.dist.resolve())
    except (ValueError, OSError, subprocess.SubprocessError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.exit(1, f"release: {error}\n")


if __name__ == "__main__":
    main()
