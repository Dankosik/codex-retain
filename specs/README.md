# Task notes

Use this directory when a larger change needs decisions or coordination that
must survive a session. Small changes can proceed directly.

One `specs/<topic>/README.md` is often enough. Record the desired behavior,
constraints, accepted decisions, unresolved questions, and evidence needed for
completion. Add a short plan only when dependencies or parallel owners make it
useful. Keep implementation status and validation results accurate.

Respect the user's named phase boundary. A research-only request produces
findings; a design-only request produces a design. Existing authorization does
not expire merely because work moves into another phase.

Task notes describe the selected product change. They do not override Cargo,
the parser, generated-source ownership, or current user instructions. Move
lasting product decisions into maintained documentation when the change is
complete, and remove stale task notes when they no longer help contributors.
