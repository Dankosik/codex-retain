"""Exercise the actual installer offline; every profile and destination is synthetic."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "tools"
        self.bin.mkdir()
        self.home = self.root / "home"
        self.home.mkdir()
        self.destination = self.home / "install with spaces"
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.env = dict(os.environ, HOME=str(self.home), PATH=f"{self.bin}:{os.environ['PATH']}",
                        CODEX_HOME=str(self.home / "codex"), CODEX_RETAIN_STATE_DIR=str(self.home / "policy"),
                        CODEX_RETAIN_INSTALL_DIR=str(self.destination), CODEX_RETAIN_VERSION="0.1.0",
                        FIXTURE_ASSETS=str(self.assets), FIXTURE_ARCH="arm64", FIXTURE_OS="Darwin")
        self.tool("uname", '#!/bin/sh\nif [ "$1" = -s ]; then echo "$FIXTURE_OS"; else echo "$FIXTURE_ARCH"; fi\n')
        self.tool("sw_vers", '#!/bin/sh\necho 15.0\n')
        self.tool("curl", f'''#!{sys.executable}
import os, pathlib, shutil, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith('https://'))
if url.endswith('/latest'):
    print('https://github.com/Dankosik/codex-retain/releases/tag/0.1.0', end='')
else:
    shutil.copyfile(pathlib.Path(os.environ['FIXTURE_ASSETS']) / url.rsplit('/', 1)[1], args[args.index('-o')+1])
''')

    def tool(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def archive(self, target="aarch64-apple-darwin", version="0.1.0"):
        name = f"codex-retain-0.1.0-{target}"
        archive = self.assets / (name + ".tar.gz")
        with tarfile.open(archive, "w:gz") as out:
            data = f'#!/bin/sh\nprintf "codex-retain {version}\\n"\n'.encode()
            member = tarfile.TarInfo(name + "/codex-retain")
            member.size, member.mode = len(data), 0o755
            out.addfile(member, io.BytesIO(data))
        (self.assets / "SHA256SUMS").write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        return archive

    def run_installer(self):
        return subprocess.run(["sh", str(release.ROOT / "install.sh")], env=self.env,
                              capture_output=True, text=True, timeout=15)

    def test_fresh_install_latest_and_repeat_update_for_both_architectures(self):
        for arch, target in (("arm64", "aarch64-apple-darwin"), ("x86_64", "x86_64-apple-darwin")):
            self.env["FIXTURE_ARCH"] = arch
            self.env.pop("CODEX_RETAIN_VERSION", None)
            self.archive(target)
            result = self.run_installer()
            self.assertEqual(result.returncode, 0, result.stderr)
            installed = self.destination / "codex-retain"
            self.assertTrue(os.access(installed, os.X_OK))
            self.assertEqual(subprocess.check_output([str(installed)], text=True), "codex-retain 0.1.0\n")
            self.assertEqual(list(self.destination.iterdir()), [installed])
        self.assertFalse((self.home / "codex").exists())
        self.assertFalse((self.home / "policy").exists())

    def test_corrupt_archive_wrong_version_and_missing_digest_preserve_existing_binary(self):
        self.destination.mkdir()
        installed = self.destination / "codex-retain"
        installed.write_text("old binary")
        for failure in ("corrupt", "version", "missing", "duplicate", "download"):
            archive = self.archive(version="9.9.9" if failure == "version" else "0.1.0")
            sums = self.assets / "SHA256SUMS"
            if failure == "corrupt":
                archive.write_bytes(b"broken")
            elif failure == "missing":
                sums.write_text("")
            elif failure == "duplicate":
                sums.write_text(sums.read_text() * 2)
            elif failure == "download":
                archive.unlink()
            result = self.run_installer()
            self.assertNotEqual(result.returncode, 0, failure)
            self.assertEqual(installed.read_text(), "old binary")

    def test_symlink_destination_is_not_replaced_or_followed(self):
        self.destination.mkdir()
        original = self.root / "managed-binary"
        original.write_text("preserved")
        installed = self.destination / "codex-retain"
        installed.symlink_to(original)
        self.assertNotEqual(self.run_installer().returncode, 0)
        self.assertTrue(installed.is_symlink())
        self.assertEqual(original.read_text(), "preserved")

    def test_unsupported_platform_and_invalid_version_fail_before_install(self):
        self.env["FIXTURE_OS"] = "Linux"
        self.assertNotEqual(self.run_installer().returncode, 0)
        self.env["FIXTURE_OS"] = "Darwin"
        self.env["CODEX_RETAIN_VERSION"] = "../../unexpected"
        self.assertNotEqual(self.run_installer().returncode, 0)
        self.assertFalse(self.destination.exists())


if __name__ == "__main__":
    unittest.main()
