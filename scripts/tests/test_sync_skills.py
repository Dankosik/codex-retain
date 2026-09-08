import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("skill_sync", Path(__file__).absolute().parents[1] / "sync_skills.py")
skill_sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(skill_sync)


class SkillSynchronizationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "consumer"
        self.source = Path(self.temporary.name) / "source"
        self.source.mkdir()
        self.git("init", "-q")
        self.git("remote", "add", "origin", "https://github.com/example/skills.git")
        self.skill = self.source / "skills/rust-example/SKILL.md"
        self.skill.parent.mkdir(parents=True)
        self.skill.write_bytes(b'---\nname: rust-example\ndescription: "An example."\n---\nOriginal.\n')
        self.git("add", ".")
        self.git("commit", "-qm", "original")
        self.initial = self.git("rev-parse", "HEAD").strip()
        self.relative = ".agents/skills/rust-example/SKILL.md"
        self.local = self.root / self.relative
        self.local.parent.mkdir(parents=True)
        self.local.write_bytes(self.skill.read_bytes())
        self.manifest = self.root / ".agents/skills-source.json"
        self.manifest.write_text(json.dumps({"source": "https://github.com/example/skills", "revision": self.initial, "license": "MIT", "files": {self.relative: hashlib.sha256(self.local.read_bytes()).hexdigest()}}))
        (self.root / "application.rs").write_text("user code")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.source), "-c", "user.name=Template test", "-c", "user.email=template@example.invalid", "-c", "commit.gpgsign=false", *args], text=True, stderr=subprocess.DEVNULL)

    def next_revision(self):
        self.skill.write_bytes(self.skill.read_bytes().replace(b"Original.", b"Updated."))
        self.git("add", ".")
        self.git("commit", "-qm", "update")
        return self.git("rev-parse", "HEAD").strip()

    def sync(self, revision, apply=False):
        with contextlib.redirect_stdout(io.StringIO()):
            return skill_sync.sync(self.root, self.source, revision, apply)

    def test_preview_then_apply_uses_committed_blobs_only(self):
        old_local = self.local.read_bytes()
        revision = self.next_revision()
        committed = self.git("show", revision + ":skills/rust-example/SKILL.md").encode("utf-8")
        self.skill.write_text("uncommitted source work")
        self.assertEqual(self.sync(revision), 1)
        self.assertEqual(self.local.read_bytes(), old_local)
        self.assertEqual(self.sync(revision, apply=True), 0)
        self.assertEqual(self.local.read_bytes(), committed)
        self.assertEqual(json.loads(self.manifest.read_text())["revision"], revision)
        self.assertEqual((self.root / "application.rs").read_text(), "user code")
        self.assertEqual(self.sync(revision), 0)

    def test_local_edits_are_preserved(self):
        revision = self.next_revision()
        self.local.write_text("local edits")
        old_manifest = self.manifest.read_bytes()
        with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
            self.sync(revision, apply=True)
        self.assertEqual(self.local.read_text(), "local edits")
        self.assertEqual(self.manifest.read_bytes(), old_manifest)

    def test_untracked_skill_file_is_preserved(self):
        extra = self.local.parent / "notes.md"
        extra.write_text("private local notes")
        with self.assertRaisesRegex(ValueError, "inventory"):
            self.sync(self.initial, apply=True)
        self.assertEqual(extra.read_text(), "private local notes")

    def test_mutable_revision_and_wrong_repository_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "immutable"):
            self.sync("HEAD", apply=True)
        self.git("remote", "set-url", "origin", "https://github.com/example/different.git")
        with self.assertRaisesRegex(ValueError, "differs"):
            self.sync(self.initial, apply=True)

    def test_ancestor_symlink_is_refused(self):
        managed = self.root / ".agents/skills"
        external = Path(self.temporary.name) / "external-skills"
        managed.rename(external)
        try:
            managed.symlink_to(external, target_is_directory=True)
        except OSError:
            self.skipTest("creating symlinks is unavailable on this platform")
        old = (external / "rust-example/SKILL.md").read_bytes()
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.sync(self.initial, apply=True)
        self.assertEqual((external / "rust-example/SKILL.md").read_bytes(), old)

    def test_new_target_type_conflict_fails_before_any_write(self):
        self.next_revision()
        new_skill = self.source / "skills/rust-zed/SKILL.md"
        new_skill.parent.mkdir()
        new_skill.write_bytes(b'---\nname: rust-zed\ndescription: "New."\n---\nNew.\n')
        self.git("add", ".")
        self.git("commit", "-qm", "new skill")
        revision = self.git("rev-parse", "HEAD").strip()
        (self.root / ".agents/skills/rust-zed/SKILL.md").mkdir(parents=True)
        before = self.local.read_bytes(), self.manifest.read_bytes()
        with self.assertRaisesRegex(ValueError, "file type"):
            self.sync(revision, apply=True)
        self.assertEqual((self.local.read_bytes(), self.manifest.read_bytes()), before)

    def test_late_write_failure_restores_skills_and_manifest(self):
        revision = self.next_revision()
        before = self.local.read_bytes(), self.manifest.read_bytes()
        original = skill_sync.update_file
        calls = 0
        def fail_once(path, data):
            nonlocal calls
            calls += 1
            if calls == 2:
                path.write_bytes(b"partial")
                raise OSError("simulated write failure")
            original(path, data)
        with mock.patch.object(skill_sync, "update_file", side_effect=fail_once):
            with self.assertRaises(OSError):
                self.sync(revision, apply=True)
        self.assertEqual((self.local.read_bytes(), self.manifest.read_bytes()), before)


if __name__ == "__main__":
    unittest.main()
