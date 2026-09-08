import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("initializer", Path(__file__).absolute().parents[1] / "init.py")
initializer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(initializer)


class InitializationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        files = {
            ".template/identity.json": json.dumps({"schema": 1, "name": "rust-cli-template", "repository": "https://github.com/Dankosik/rust-cli-template", "description": "Original description.", "environment_prefix": "RUST_CLI_TEMPLATE", "initialized": False}),
            "Cargo.toml": '[package]\nname = "rust-cli-template"\nversion = "0.1.0"\nrepository = "https://github.com/Dankosik/rust-cli-template"\ndescription = "Original description."\n',
            "Cargo.lock": 'version = 4\n\n[[package]]\nname = "dependency"\nversion = "1.2.3"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "untouched"\n\n[[package]]\nname = "rust-cli-template"\nversion = "0.1.0"\ndependencies = [\n "dependency",\n]\n',
            "README.md": '# Rust CLI Template\nRun rust-cli-template.\nhttps://github.com/Dankosik/rust-cli-template\n',
            ".template/README.md": '# {{name}}\n{{description}}\n{{repository}}\n{{environment_prefix}}_FORMAT\n',
            "src/main.rs": 'fn main() { rust_cli_template::run(); }\n',
            "tests/cli.rs": 'env!("CARGO_BIN_EXE_rust-cli-template"); // RUST_CLI_TEMPLATE_FORMAT\n',
            ".agents/skills/rust-example/SKILL.md": 'upstream rust-cli-template must remain untouched\n',
        }
        for name, contents in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents.encode("utf-8"))

    def snapshot(self):
        return {p.relative_to(self.root).as_posix(): p.read_bytes() for p in self.root.rglob("*") if p.is_file()}

    def initialize(self, **overrides):
        args = dict(root=self.root, name="file-tool", repository="https://github.com/example/file-tool", description='Counts "files" safely.')
        args.update(overrides)
        return initializer.initialize(**args)

    def test_complete_identity_and_idempotence_without_dependency_drift(self):
        before = self.snapshot()
        self.assertGreater(self.initialize(), 0)
        after = self.snapshot()
        self.assertIn(b"file_tool::run", after["src/main.rs"])
        self.assertIn(b"CARGO_BIN_EXE_file-tool", after["tests/cli.rs"])
        self.assertIn(b"FILE_TOOL_FORMAT", after["tests/cli.rs"])
        self.assertTrue(after["README.md"].startswith(b"# file-tool\n"))
        self.assertNotIn(b"{{", after["README.md"])
        self.assertIn(b'Counts \\"files\\" safely.', after["Cargo.toml"])
        self.assertEqual(after["Cargo.lock"], before["Cargo.lock"].replace(b'name = "rust-cli-template"', b'name = "file-tool"'))
        self.assertEqual(after[".agents/skills/rust-example/SKILL.md"], before[".agents/skills/rust-example/SKILL.md"])
        self.assertEqual(self.initialize(), 0)
        self.assertEqual(self.snapshot(), after)
        with self.assertRaisesRegex(ValueError, "already initialized"):
            self.initialize(name="different-tool")
        self.assertEqual(self.snapshot(), after)

    def test_invalid_inputs_never_mutate(self):
        before = self.snapshot()
        for name in ["", "9tool", "SomeTool", "../outside", "two--parts", "fn", "con", "com1", "x" * 65, "a$(command)"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.initialize(name=name)
        for repository in ["http://github.com/me/tool", "https://example.org/me/tool", "https://github.com/me/..", "https://github.com/me/tool?x=1"]:
            with self.subTest(repository=repository), self.assertRaises(ValueError):
                self.initialize(repository=repository)
        with self.assertRaises(ValueError):
            self.initialize(description="two\nlines")
        self.assertEqual(self.snapshot(), before)

    def test_dry_run_reports_patch_without_mutating(self):
        before = self.snapshot()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.initialize(dry_run=True)
        self.assertIn("+name = \"file-tool\"", output.getvalue())
        self.assertEqual(self.snapshot(), before)

    def test_unicode_description_remains_idempotent(self):
        self.initialize(description="Подсчёт файлов.")
        self.assertEqual(self.initialize(description="Подсчёт файлов."), 0)

    def test_new_identity_values_are_not_rewritten_again(self):
        repository = "https://github.com/example/rust-cli-template-custom"
        self.initialize(repository=repository)
        self.assertIn(repository, (self.root / "Cargo.toml").read_text())
        self.assertEqual(json.loads((self.root / ".template/identity.json").read_text())["repository"], repository)
        self.assertEqual(self.initialize(repository=repository), 0)

    def test_drift_is_rejected_before_writes(self):
        (self.root / "Cargo.toml").write_text('[package]\nname = "other"\n')
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "identity drift"):
            self.initialize()
        self.assertEqual(self.snapshot(), before)

    def test_write_failure_restores_preexisting_content(self):
        before = self.snapshot()
        original = initializer.write_file
        calls = 0

        def failing_write(path, text):
            nonlocal calls
            calls += 1
            if calls == 2:
                path.write_text("partial")
                raise OSError("simulated write failure")
            original(path, text)

        with mock.patch.object(initializer, "write_file", side_effect=failing_write):
            with self.assertRaises(OSError):
                self.initialize()
        self.assertEqual(self.snapshot(), before)

    def test_symlinked_identity_file_is_refused(self):
        outside = self.root / "outside.rs"
        outside.write_text("original")
        link = self.root / "src/main.rs"
        link.unlink()
        try:
            link.symlink_to(outside)
        except OSError:
            self.skipTest("creating symlinks is unavailable on this platform")
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.initialize()
        self.assertEqual(outside.read_text(), "original")


if __name__ == "__main__":
    unittest.main()
