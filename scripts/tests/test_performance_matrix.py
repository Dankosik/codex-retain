from contextlib import closing
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import performance_matrix as matrix


def synthetic_policy(root, codex_bin):
    metadata = (root / "codex/state_5.sqlite").stat()
    return {"codex_home": str(root / "codex"), "owner": str(root / "state"), "codex_bin": codex_bin,
            "database": {"device": metadata.st_dev, "inode": metadata.st_ino},
            "enabled": True, "automatic": False, "paused": False, "retention_days": 30}


class MatrixFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        matrix.fixture.write_json(self.root / matrix.benchmark.BENCHMARK_MARKER,
                                  {"kind": "codex-retain-benchmark", "root": str(self.root)})
        (self.root / "cases").mkdir()
        (self.root / "fixtures").mkdir()
        self.path = self.root / "cases/one-busy-candidate.json"
        self.fixture = self.root / "fixtures" / self.path.stem
        base = matrix.fixture.create(self.fixture, 3, 4096, 1900000000)
        matrix.fixture.write_json(self.fixture / "state/policy.json", synthetic_policy(self.fixture, "/synthetic/codex"))
        records = [dict(record, db_path=record["path"], archived=1) for record in base["records"]]
        self.manifest = {"records": records, "created_at": base["created_at"], "rollout_bytes": 4096,
                         "protected": {path: matrix.digest(self.fixture / path) for path in matrix.PROTECTED_FILES}}
        self.ids = sorted(record["id"] for record in records)
        self.case = {"benchmark_root": str(self.root), "fixture_root": str(self.fixture),
                     "scenario": "one-busy", "binary": "/unused-retain", "busy_ids": [self.ids[1]],
                     "held_locks": [], "codex_bin": "/synthetic/codex", "now": 1900000000}
        self.database = self.fixture / "codex/state_5.sqlite"
        with closing(sqlite3.connect(self.database)) as connection:
            connection.execute("CREATE TABLE codex_retain_epochs("
                               "thread_id TEXT PRIMARY KEY,archived_since INTEGER,codex_archived_at INTEGER)")
            connection.executemany("INSERT INTO codex_retain_epochs VALUES(?,?,?)",
                                   [(ident, 1900000000 - 40 * matrix.fixture.DAY, base["archived_at"])
                                    for ident in self.ids])
            connection.execute("CREATE TABLE codex_retain_owner(singleton INTEGER PRIMARY KEY,owner TEXT)")
            connection.execute("INSERT INTO codex_retain_owner VALUES(1,'synthetic owner')")
            connection.commit()
            connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            with closing(sqlite3.connect(self.fixture / "matrix-seed.sqlite")) as seed:
                connection.backup(seed)
            self.manifest["database"] = matrix.database_snapshot(connection)
        matrix.fixture.write_json(self.fixture / "matrix-manifest.json", self.manifest)
        matrix.fixture.write_json(self.path, self.case)

    def consume_free_members(self):
        removed = {self.ids[0], self.ids[2]}
        with closing(sqlite3.connect(self.database)) as connection:
            for ident in removed:
                connection.execute("DELETE FROM threads WHERE id=?", (ident,))
                connection.execute("DELETE FROM codex_retain_epochs WHERE thread_id=?", (ident,))
            connection.commit()
        for record in self.manifest["records"]:
            if record["id"] in removed:
                (self.fixture / record["path"]).unlink()
        report = {"examined": 3, "eligible": 2, "deleted": 2, "skipped": 1,
                  "logical_bytes_removed": 8192, "warnings": []}
        matrix.fixture.write_json(self.fixture / "state/last-run.json", report)

    def test_reset_restores_exact_database_and_bytes_without_replacing_held_lock_inode(self):
        with matrix.held_locks(self.case):
            matrix.fixture.write_json(self.path, self.case)
            matrix.verify(self.path, before=True)
            original = list(self.case["held_locks"])
            self.consume_free_members()
            result = matrix.verify(self.path)
            self.assertEqual(result["removed_ids"], [self.ids[0], self.ids[2]])
            self.assertEqual(result["remaining_rows"], 1)
            matrix.restore(self.path)
            self.assertFalse((self.fixture / "state/last-run.json").exists())
            restored = matrix.verify(self.path, before=True)
            self.assertEqual(restored["remaining_rows"], 3)
            self.assertEqual(restored["held_locks"], original)

    def test_pending_intent_blocks_reset_before_restoring_database(self):
        self.consume_free_members()
        (self.fixture / "state/pending.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "pending intent"):
            matrix.restore(self.path)
        with closing(sqlite3.connect(self.database)) as connection:
            self.assertEqual(connection.execute("SELECT count(*) FROM threads").fetchone()[0], 1)

    def test_verification_detects_content_changes_and_unexpected_files(self):
        path = self.fixture / self.manifest["records"][0]["path"]
        original = path.read_bytes()
        path.write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "content changed"):
            matrix.verify(self.path, before=True)
        path.write_bytes(original)
        (path.parent / "unowned.jsonl").write_bytes(b"extra")
        with self.assertRaisesRegex(ValueError, "path set"):
            matrix.verify(self.path, before=True)

    def test_descriptor_cannot_redirect_reset_outside_its_marked_fixture(self):
        self.case["fixture_root"] = str(self.root / "foreign")
        matrix.fixture.write_json(self.path, self.case)
        with self.assertRaisesRegex(ValueError, "descriptor"):
            matrix.restore(self.path)

    def test_manifest_cannot_redirect_reset_outside_its_synthetic_rollout_location(self):
        self.consume_free_members()
        foreign = self.root / "foreign"
        self.manifest["records"][0]["path"] = str(foreign)
        matrix.fixture.write_json(self.fixture / "matrix-manifest.json", self.manifest)
        with self.assertRaisesRegex(ValueError, "manifest path"):
            matrix.restore(self.path)
        self.assertFalse(foreign.exists())
        with closing(sqlite3.connect(self.database)) as connection:
            self.assertEqual(connection.execute("SELECT count(*) FROM threads").fetchone()[0], 1)

    def test_released_foreign_lock_is_not_accepted_as_held(self):
        with matrix.held_locks(self.case):
            matrix.fixture.write_json(self.path, self.case)
            matrix.verify_locks(self.case, self.fixture)
        with self.assertRaisesRegex(ValueError, "no longer holds"):
            matrix.verify_locks(self.case, self.fixture)

    def test_existing_output_directory_is_rejected_before_executable_resolution(self):
        with self.assertRaisesRegex(ValueError, "nonexistent scratch"):
            matrix.setup(SimpleNamespace(root=self.root))

    def test_run_eligibility_counts_completed_deletions_and_preview_counts_selected_ids(self):
        for scenario, eligible, deleted in [("one-busy", 2, 2), ("global-busy", 0, 0),
                                             ("all-busy", 0, 0), ("mixed-one", 1, 0),
                                             ("mixed-none", 0, 0), ("headers-plain", 3, 0),
                                             ("cleanup-1000", 3, 3), ("cleanup-10000", 3, 3)]:
            with self.subTest(scenario=scenario):
                counts, removed = matrix.expected(dict(self.case, scenario=scenario), self.manifest)
                self.assertEqual(counts, {"examined": 3, "eligible": eligible,
                                          "deleted": deleted, "skipped": 3 - eligible})
                self.assertEqual(len(removed), deleted)

    def test_only_exact_global_stop_warning_is_accepted_in_global_busy(self):
        self.case.update(scenario="global-busy", busy_ids=[])
        matrix.fixture.write_json(self.path, self.case)
        report = {"examined": 3, "eligible": 0, "deleted": 0, "skipped": 3,
                  "logical_bytes_removed": 0, "warnings": [matrix.GLOBAL_STOP_WARNING]}
        matrix.verify(self.path, report)
        matrix.verify(self.path, dict(report, warnings=[]))
        for warnings in [["pending recovery failed"], [matrix.GLOBAL_STOP_WARNING, "pending recovery failed"],
                         [matrix.GLOBAL_STOP_WARNING, matrix.GLOBAL_STOP_WARNING]]:
            with self.subTest(warnings=warnings), self.assertRaisesRegex(ValueError, "recovery warning"):
                matrix.verify(self.path, dict(report, warnings=warnings))
        self.case["scenario"] = "all-busy"
        matrix.fixture.write_json(self.path, self.case)
        with self.assertRaisesRegex(ValueError, "recovery warning"):
            matrix.verify(self.path, report)

    def test_each_capture_epoch_value_must_remain_unchanged(self):
        for column in ("archived_since", "codex_archived_at"):
            with self.subTest(column=column):
                with closing(sqlite3.connect(self.database)) as connection:
                    connection.execute("UPDATE codex_retain_epochs SET " + column + "=" + column + "+1")
                    connection.commit()
                with self.assertRaisesRegex(ValueError, "epoch values"):
                    matrix.verify(self.path, before=True)
                with closing(sqlite3.connect(self.database)) as connection:
                    connection.execute("UPDATE codex_retain_epochs SET " + column + "=" + column + "-1")
                    connection.commit()

    def test_surviving_thread_columns_beyond_identity_and_path_are_protected(self):
        with closing(sqlite3.connect(self.database)) as connection:
            connection.execute("UPDATE threads SET title='unexpected title' WHERE id=?", (self.ids[1],))
            connection.commit()
        with self.assertRaisesRegex(ValueError, "thread metadata"):
            matrix.verify(self.path, before=True)

    def test_capture_owner_is_protected_metadata(self):
        with closing(sqlite3.connect(self.database)) as connection:
            connection.execute("UPDATE codex_retain_owner SET owner='replacement'")
            connection.commit()
        with self.assertRaisesRegex(ValueError, "protected SQLite"):
            matrix.verify(self.path, before=True)

    def test_policy_bytes_must_remain_unchanged(self):
        (self.fixture / "state/policy.json").write_text('{"enabled":false}\n')
        with self.assertRaisesRegex(ValueError, "protected fixture file"):
            matrix.verify(self.path, before=True)

    def test_new_pair_descriptors_share_exact_scenario_path_and_legacy_path_still_works(self):
        self.assertEqual(matrix.read_case(self.path)["fixture_root"], str(self.fixture))
        shared = str(self.root / "fixtures/one-busy")
        for tool in ("candidate", "baseline"):
            path = self.root / "cases" / ("one-busy-" + tool + ".json")
            case = dict(self.case, fixture_key="one-busy", fixture_root=shared, tool=tool)
            matrix.fixture.write_json(path, case)
            self.assertEqual(matrix.read_case(path)["fixture_root"], shared)
        case["fixture_key"] = "one-busy-baseline"
        case["fixture_root"] = str(self.root / "fixtures/one-busy-baseline")
        matrix.fixture.write_json(path, case)
        with self.assertRaisesRegex(ValueError, "descriptor"):
            matrix.read_case(path)

    def test_pair_populates_once_and_preserves_policy_and_manifest_for_second_binary(self):
        shared = self.root / "fixtures/one-busy"
        first = dict(self.case, fixture_key="one-busy", fixture_root=str(shared), binary="/first-retain")

        def enable(_arguments, **_kwargs):
            # Model the enable boundary without executing a native binary.
            with closing(sqlite3.connect(shared / "codex/state_5.sqlite")) as connection:
                connection.executescript((matrix.fixture.REPOSITORY / "compatibility/capture.sql").read_text())
                connection.execute("INSERT INTO codex_retain_owner VALUES(1,?)", (str(shared / "state"),))
                connection.execute("INSERT INTO codex_retain_epochs SELECT id,1,archived_at FROM threads WHERE archived=1")
                connection.commit()
            matrix.fixture.write_json(shared / "state/policy.json", synthetic_policy(shared, first["codex_bin"]))
            return "{}\n"

        with patch.object(matrix.benchmark, "execute", side_effect=enable) as native:
            self.assertFalse(matrix.prepare_fixture(first))
            policy_hash = matrix.digest(shared / "state/policy.json")
            manifest_hash = matrix.digest(shared / "matrix-manifest.json")
            second = dict(first, binary="/second-retain", busy_ids=[])
            self.assertTrue(matrix.prepare_fixture(second))
            self.assertEqual(native.call_count, 1)
        self.assertEqual(second["busy_ids"], first["busy_ids"])
        self.assertEqual(second["policy_sha256"], policy_hash)
        self.assertEqual(matrix.digest(shared / "matrix-manifest.json"), manifest_hash)

    def test_reused_consumed_fixture_restores_under_reacquired_locks(self):
        with matrix.held_locks(self.case):
            matrix.fixture.write_json(self.path, self.case)
            self.consume_free_members()
            matrix.verify(self.path)
        second = dict(self.case, binary="/second-retain", busy_ids=[])
        with patch.object(matrix, "populate") as populate:
            self.assertTrue(matrix.prepare_fixture(second))
            populate.assert_not_called()
        self.assertEqual(second["busy_ids"], [self.ids[1]])
        with matrix.held_locks(second):
            matrix.fixture.write_json(self.path, second)
            matrix.restore(self.path)
            result = matrix.verify(self.path, before=True)
            self.assertEqual(result["remaining_rows"], 3)
            matrix.guard_policy(second)

    def test_policy_guard_rejects_foreign_home_state_and_binary_before_native_invocation(self):
        original = synthetic_policy(self.fixture, self.case["codex_bin"])
        for key, value in (("codex_home", "/foreign/codex"), ("owner", "/foreign/state"),
                           ("codex_bin", "/foreign/codex-bin")):
            with self.subTest(key=key), patch.object(matrix.subprocess, "run") as native:
                matrix.fixture.write_json(self.fixture / "state/policy.json", dict(original, **{key: value}))
                with self.assertRaisesRegex(ValueError, "must name exactly"):
                    matrix.observe(self.path, "must-not-run")
                native.assert_not_called()

    def test_policy_guard_checks_database_identity_and_policy_fingerprint(self):
        original = synthetic_policy(self.fixture, self.case["codex_bin"])
        matrix.guard_policy(self.case)  # Legacy descriptors derive their policy hash from the manifest.
        changed = dict(original, database=dict(original["database"], inode=original["database"]["inode"] + 1))
        matrix.fixture.write_json(self.fixture / "state/policy.json", changed)
        with self.assertRaisesRegex(ValueError, "database inode"):
            matrix.guard_policy(self.case)
        matrix.fixture.write_json(self.fixture / "state/policy.json", dict(original, additional="changed"))
        with self.assertRaisesRegex(ValueError, "policy bytes"):
            matrix.guard_policy(self.case)

    def test_cleanup_group_selects_fixed_sizes_and_pairs_each_shared_fixture(self):
        executable = str(Path(__file__).resolve())
        args = SimpleNamespace(root=self.root / "new-matrix", runs=5, baseline=executable,
                               candidate=executable, codex_bin=executable, hyperfine=executable,
                               cases=["cleanup"], all_busy=False)
        with patch.object(matrix.benchmark, "executable", return_value=executable), \
                patch.object(matrix.benchmark, "version", return_value="synthetic version"):
            root, paths, _ = matrix.setup(args)
        cases = [matrix.read_case(path) for path in paths]
        self.assertEqual([case["scenario"] for case in cases],
                         ["cleanup-1000", "cleanup-1000", "cleanup-10000", "cleanup-10000"])
        self.assertEqual([case["tool"] for case in cases], ["baseline", "candidate", "candidate", "baseline"])
        for first, second in (cases[:2], cases[2:]):
            self.assertEqual(first["fixture_root"], second["fixture_root"])
            self.assertEqual(first["fixture_root"], str(root / "fixtures" / first["scenario"]))
            self.assertTrue(matrix.requires_reset(first))
            self.assertEqual(matrix.command(first)[-1], "run")
            with patch.object(matrix.fixture, "create", side_effect=RuntimeError("fixture boundary")) as create:
                with self.assertRaisesRegex(RuntimeError, "fixture boundary"):
                    matrix.populate(first)
                self.assertEqual(create.call_args.args[1], int(first["scenario"].removeprefix("cleanup-")))

    def test_cleanup_verifies_all_removals_accepts_exit_zero_and_restores_without_held_locks(self):
        self.case.update(scenario="cleanup-1000", busy_ids=[])
        with matrix.held_locks(self.case):
            matrix.fixture.write_json(self.path, self.case)
            self.assertEqual(self.case["held_locks"], [])
            with closing(sqlite3.connect(self.database)) as connection:
                connection.execute("DELETE FROM threads")
                connection.execute("DELETE FROM codex_retain_epochs")
                connection.commit()
            for record in self.manifest["records"]:
                (self.fixture / record["path"]).unlink()
            report = {"examined": 3, "eligible": 3, "deleted": 3, "skipped": 0,
                      "logical_bytes_removed": 12288, "warnings": [],
                      "entries": [{"id": ident, "reason": "deleted"} for ident in self.ids]}
            last_run = self.fixture / "state/last-run.json"
            matrix.fixture.write_json(last_run, report)
            completed = matrix.subprocess.CompletedProcess([], 0, last_run.read_text(), "")
            with patch.object(matrix.subprocess, "run", return_value=completed):
                receipt = matrix.observe(self.path, "cleanup-proof")
            self.assertEqual(receipt["removed_ids"], self.ids)
            self.assertEqual(receipt["remaining_rows"], 0)
            self.assertEqual(receipt["exit_code"], 0)
            completed.returncode = 3
            with patch.object(matrix.subprocess, "run", return_value=completed):
                with self.assertRaisesRegex(ValueError, "expected 0"):
                    matrix.observe(self.path, "must-reject-exit-three")
            matrix.restore(self.path)
            self.assertEqual(matrix.verify(self.path, before=True)["remaining_rows"], 3)
            self.assertFalse(last_run.exists())


if __name__ == "__main__":
    unittest.main()
