#!/usr/bin/env python3
"""Build release archives from a tested native binary; no publication or credentials."""

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)
OPTIONAL_NOTICES = ("NOTICE", "THIRD_PARTY_NOTICES", "THIRD_PARTY_NOTICES.md", "RUST_STDLIB_NOTICES.html")


@dataclass(frozen=True)
class Identity:
    binary: str
    version: str
    repository: str
    target_directory: Path

    def archive_root(self, target):
        if target not in TARGETS:
            raise ValueError("binary releases support macOS Apple Silicon and Intel only")
        return f"{self.binary}-{self.version}-{target}"

    def executable(self, target):
        self.archive_root(target)
        return self.binary

    def archive_name(self, target):
        return self.archive_root(target) + ".tar.gz"


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
    if tag != info.version:
        raise ValueError(f"tag must equal {info.version}")
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
    # These commands must work before configuration and without a Codex profile.
    # An empty isolated home also detects accidental initialization in the smoke.
    with tempfile.TemporaryDirectory(prefix="codex-retain-smoke-") as temporary:
        home = Path(temporary)
        env = dict(os.environ, HOME=str(home), CODEX_HOME=str(home / "missing-codex"),
                   CODEX_RETAIN_STATE_DIR=str(home / "missing-policy"), NO_COLOR="1")
        def invoke(*arguments):
            result = subprocess.run(
                [str(executable), *arguments], env=env, stdin=subprocess.DEVNULL,
                capture_output=True, timeout=10, check=True,
            )
            if result.stderr:
                raise ValueError("extracted binary smoke command wrote unexpected stderr")
            return result.stdout

        if invoke("--version").strip() != f"{info.binary} {info.version}".encode():
            raise ValueError("extracted binary has unexpected version output")
        help_output = invoke("--help")
        for command in (b"enable", b"preview", b"run", b"pause", b"disable", b"completions"):
            if not re.search(rb"(?m)^\s+" + command + rb"\s", help_output):
                raise ValueError("extracted binary help is missing a retention command")
        completion = invoke("completions", "bash")
        if info.binary.encode() not in completion or b"complete " not in completion:
            raise ValueError("extracted binary failed the Bash completion smoke test")
        if any(home.iterdir()):
            raise ValueError("configuration-free smoke commands unexpectedly created local state")


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
            if name == executable and not mode & 0o111:
                raise ValueError("archive binary has lost its executable permission")

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
    if not binary.exists():
        # `cargo build --release` uses this native-host location. Cross-target
        # packaging remains forbidden by the native_target check above.
        binary = info.target_directory / "release" / info.executable(target)
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


def homebrew(info, dist, output):
    """Generate a binary formula only from the complete verified archive set."""
    checksums(info, dist)
    digests = dict(line.split()[::-1] for line in (dist / "SHA256SUMS").read_text().splitlines())
    base = f"{info.repository}/releases/download/{info.version}"
    arm, intel = (info.archive_name(target) for target in TARGETS)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(f'''# Generated by scripts/release.py homebrew; do not edit checksums by hand.
class CodexRetain < Formula
  desc "Predictable local retention for archived Codex chats"
  homepage "{info.repository}"
  version "{info.version}"
  license "MIT"

  depends_on :macos
  depends_on macos: :sequoia

  on_arm do
    url "{base}/{arm}"
    sha256 "{digests[arm]}"
  end
  on_intel do
    url "{base}/{intel}"
    sha256 "{digests[intel]}"
  end

  def install
    bin.install "codex-retain"
    generate_completions_from_executable(bin/"codex-retain", "completions")
  end

  def caveats
    <<~EOS
      Installation does not enable retention. Run codex-retain doctor first.
      Enable using codex-retain on PATH so the schedule follows brew upgrades.
      Before brew uninstall, run codex-retain uninstall to remove the schedule.
    EOS
  end

  test do
    ENV["CODEX_HOME"] = testpath/"missing-codex"
    ENV["CODEX_RETAIN_STATE_DIR"] = testpath/"missing-policy"
    assert_equal "codex-retain #{{version}}\\n", shell_output("#{{bin}}/codex-retain --version")
    assert_match "preview", shell_output("#{{bin}}/codex-retain --help")
    refute_path_exists testpath/"missing-codex"
    refute_path_exists testpath/"missing-policy"
  end
end
''', encoding="utf-8")
    print(output)


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
    brew = commands.add_parser("homebrew", help="generate a formula from verified release archives")
    brew.add_argument("--dist", type=Path, default=Path("dist"))
    brew.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        info = identity()
        if args.command == "check-tag":
            check_tag(info, args.tag, args.repository)
        elif args.command == "package":
            package(info, args.target, args.dist.resolve())
        elif args.command == "verify":
            verify_archive(args.archive.resolve(), info, args.target)
        elif args.command == "homebrew":
            homebrew(info, args.dist.resolve(), args.output.resolve())
        else:
            checksums(info, args.dist.resolve())
    except (ValueError, OSError, subprocess.SubprocessError, tarfile.TarError) as error:
        parser.exit(1, f"release: {error}\n")


if __name__ == "__main__":
    main()
