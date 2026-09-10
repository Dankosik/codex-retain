#!/usr/bin/env python3
"""Inspect captured Retain schemas in memory; never open a user profile."""
import argparse
import hashlib
import json
import sqlite3
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--repo', type=Path, required=True)
args = parser.parse_args()
schema_path = args.repo / 'compatibility/schema.json'
capture_path = args.repo / 'compatibility/capture.sql'
schema_bytes = schema_path.read_bytes()
capture_bytes = capture_path.read_bytes()
objects = json.loads(schema_bytes)
c = sqlite3.connect(':memory:')
for obj in sorted(objects, key=lambda item: {'table': 0, 'index': 1, 'trigger': 2}[item['type']]):
    if obj['sql']:
        c.execute(obj['sql'])
c.executescript(capture_bytes.decode())
c.execute('PRAGMA foreign_keys=ON')
queries = {
    'rollout_owners': ('SELECT id,rollout_path FROM threads WHERE rollout_path IN (?,?,?,?)', ['a', 'a.zst', 'b', 'b.zst']),
    'archived_selection': ('SELECT t.id FROM threads t WHERE t.archived<>0 OR t.archived IS NULL ORDER BY t.id LIMIT 100001', []),
    'spawn_edge_membership': ('SELECT EXISTS(SELECT 1 FROM thread_spawn_edges s WHERE s.parent_thread_id=? OR s.child_thread_id=?)', ['x', 'x']),
    'delete_thread_with_foreign_keys': ('DELETE FROM threads WHERE id=?', ['x']),
    'capture_epoch_delete_old_and_new': ('DELETE FROM codex_retain_epochs WHERE thread_id=? OR thread_id=?', ['old', 'new']),
}
plans = {name: {'sql': sql, 'plan': list(c.execute('EXPLAIN QUERY PLAN ' + sql, params))} for name, (sql, params) in queries.items()}
foreign_keys = {}
for obj in objects:
    if obj['type'] == 'table':
        table = obj['name'].replace('"', '""')
        keys = list(c.execute(f'PRAGMA foreign_key_list("{table}")'))
        if keys:
            foreign_keys[obj['name']] = keys
thread_indexes = list(c.execute('PRAGMA index_list(threads)'))
result = {
    'scope': 'Fresh in-memory SQLite reconstructed from repository captured schema; EXPLAIN only, no production files or timing.',
    'limitation': 'Python SQLite planner, not the Rust executable bundled SQLite; empty data, no ANALYZE statistics.',
    'sqlite_version': sqlite3.sqlite_version,
    'sources': {str(path): hashlib.sha256(data).hexdigest() for path, data in [(schema_path, schema_bytes), (capture_path, capture_bytes)]},
    'plans': plans,
    'foreign_keys': foreign_keys,
    'thread_indexes': thread_indexes,
    'thread_index_count_including_primary_key': len(thread_indexes),
    'base_delete_triggers': [obj for obj in objects if obj['type'] == 'trigger' and 'DELETE' in obj['sql'].upper()],
    'capture_sql': capture_bytes.decode(),
}
print(json.dumps(result, indent=2))
