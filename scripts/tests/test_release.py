import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.info = release.Identity("renamed-cli", "2.3.4", "https://github.com/example/renamed-cli", self.root / "target")

    def archive(self, target, *, extra=None, executable=True):
        path = self.root / self.info.archive_name(target)
        prefix = self.info.archive_root(target) + "/"
        contents = [(prefix + self.info.executable(target), b"binary payload"),
                    (prefix + "README.md", b"usage"), (prefix + "LICENSE", b"license")]
        if extra is not None:
            contents.append((extra, b"unexpected"))
        if "windows" in target:
            with zipfile.ZipFile(path, "w") as archive:
                for name, data in contents:
                    archive.writestr(name, data)
        else:
            with tarfile.open(path, "w:gz") as archive:
                for name, data in contents:
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    member.mode = 0o755 if executable else 0o644
                    archive.addfile(member, io.BytesIO(data))
        return path

    def test_extracts_only_expected_binary_for_all_formats(self):
        for target in release.TARGETS:
            with self.subTest(target=target):
                archive = self.archive(target)
                destination = self.root / "extracted"
                release.inspect_archive(archive, self.info, target, destination)
                self.assertEqual(destination.read_bytes(), b"binary payload")
                self.assertFalse((self.root / "README.md").exists())

    def test_rejects_parent_paths_without_writing_outside_destination(self):
        for target in (release.TARGETS[0], release.TARGETS[-1]):
            with self.subTest(target=target):
                archive = self.archive(target, extra="../outside")
                destination = self.root / "extracted"
                with self.assertRaisesRegex(ValueError, "unexpected paths"):
                    release.inspect_archive(archive, self.info, target, destination)
                self.assertFalse(destination.exists())

    def test_rejects_duplicate_binary_entries(self):
        target = release.TARGETS[0]
        name = self.info.archive_root(target) + "/" + self.info.executable(target)
        archive = self.archive(target, extra=name)
        with self.assertRaisesRegex(ValueError, "duplicate"):
            release.inspect_archive(archive, self.info, target)

    def test_accepts_each_supported_notice_filename(self):
        for target in (release.TARGETS[0], release.TARGETS[-1]):
            for notice in release.OPTIONAL_NOTICES:
                with self.subTest(target=target, notice=notice):
                    archive = self.archive(target, extra=self.info.archive_root(target) + "/" + notice)
                    release.inspect_archive(archive, self.info, target)

    def test_rejects_missing_unix_execute_permission(self):
        target = release.TARGETS[0]
        archive = self.archive(target, executable=False)
        with self.assertRaisesRegex(ValueError, "executable permission"):
            release.inspect_archive(archive, self.info, target)

    def test_rejects_a_symlink_in_place_of_binary(self):
        target = release.TARGETS[0]
        path = self.root / self.info.archive_name(target)
        prefix = self.info.archive_root(target) + "/"
        with tarfile.open(path, "w:gz") as archive:
            for name in [self.info.executable(target), "README.md", "LICENSE"]:
                member = tarfile.TarInfo(prefix + name)
                member.mode = 0o755
                if name == self.info.executable(target):
                    member.type = tarfile.SYMTYPE
                    member.linkname = "../outside"
                archive.addfile(member, io.BytesIO())
        with self.assertRaisesRegex(ValueError, "regular file"):
            release.inspect_archive(path, self.info, target)

    def test_incomplete_release_cannot_produce_checksums(self):
        self.archive(release.TARGETS[0])
        with self.assertRaisesRegex(ValueError, "incomplete release set"):
            release.checksums(self.info, self.root)
        self.assertFalse((self.root / "SHA256SUMS").exists())

    def test_full_release_gets_exact_archive_inventory(self):
        for target in release.TARGETS:
            self.archive(target)
        release.checksums(self.info, self.root)
        lines = (self.root / "SHA256SUMS").read_text().splitlines()
        self.assertEqual({line.split("  ", 1)[1] for line in lines},
                         {self.info.archive_name(target) for target in release.TARGETS})
        self.assertTrue(all(len(line.split("  ", 1)[0]) == 64 for line in lines))

    def test_foreign_target_does_not_execute_archive(self):
        with patch.object(release, "native_target", return_value=release.TARGETS[0]), patch.object(release, "smoke") as smoke:
            with self.assertRaisesRegex(ValueError, "native runner"):
                release.verify_archive(self.root / "foreign.zip", self.info, release.TARGETS[-1])
            smoke.assert_not_called()


class TagTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.info = release.Identity("tool", "0.1.0", "https://github.com/example/tool", self.root / "target")
        self.git("init", "-q")
        (self.root / "source").write_text("first\n")
        self.git("add", "source")
        self.commit()
        self.git("tag", "v0.1.0")
        self.root_patch = patch.object(release, "ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def commit(self):
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")

    def test_accepts_matching_tag_and_clean_source(self):
        release.check_tag(self.info, "v0.1.0", "example/tool")

    def test_rejects_version_and_repository_mismatch(self):
        with self.assertRaisesRegex(ValueError, "tag must equal"):
            release.check_tag(self.info, "v9.9.9", "example/tool")
        with self.assertRaisesRegex(ValueError, "repository does not match"):
            release.check_tag(self.info, "v0.1.0", "someone/else")

    def test_rejects_source_modified_after_checkout(self):
        (self.root / "source").write_text("modified\n")
        with self.assertRaisesRegex(ValueError, "source files changed"):
            release.check_tag(self.info, "v0.1.0", "example/tool")

    def test_rejects_tag_pointing_to_a_different_commit(self):
        (self.root / "source").write_text("next\n")
        self.git("add", "source")
        self.commit()
        with self.assertRaisesRegex(ValueError, "checked-out commit"):
            release.check_tag(self.info, "v0.1.0", "example/tool")


if __name__ == "__main__":
    unittest.main()
