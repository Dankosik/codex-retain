# Working with agents

[AGENTS.md](../AGENTS.md) owns repository rules. The Rust skills provide focused
engineering methods. This document explains how to organize work without
introducing a second set of requirements.

## A normal change

Read the user's requirement and the affected code. Resolve routine technical
choices from the current design, implement the change, run checks that cover
its behavior, and repair discovered defects. Keep the work direct when its
scope and ownership are clear. Do not create a specification just to edit a
command or fix a bug.

For a command change, identify arguments and input, success output, expected
errors, exit status, and any file or subprocess effects. For a refactor, identify
which observable behavior must stay stable. For performance work, select a
representative workload and metric before changing the implementation.

## Larger work

Use a short document under `specs/<topic>/` when decisions must survive multiple
sessions or several people need the same contract. Record the desired behavior,
constraints, accepted decisions, open questions, and completion evidence. Add
a small dependency-ordered plan only when it helps coordinate implementation.
An explicit request for only research, design, or planning stops at that boundary.

The agent owns unresolved technical choices within the task. Ask the user about
meaning, priority, external inputs, or effects when those cannot be resolved
from the request. A missing implementation detail alone is not an approval gate.

## Delegation

Delegate concrete independent work when it can save time. Give each agent the
outcome, relevant files, accepted decisions, writable scope, constraints, and
expected result. Keep writers on disjoint files; sequence overlapping edits and
shared build or benchmark resources. Read-only reviewers may share the checkout.

The coordinating agent integrates the results and resolves disagreements.
After substantial parallel changes, review the assembled behavior and run the
relevant final checks together. A subagent's success report is evidence to
inspect, not a substitute for integration. Do not add a proof or approval round
at every subtask boundary.

## Useful handoffs

Leave enough information to resume: current outcome, accepted decisions, files
changed, commands and results, and the next action or concrete blocker. Distinguish
implementation, local verification, CI, and publication. An unavailable optional
tool does not block unrelated work or justify silently weakening a claim.

Vendored skills remain identifiable upstream content. Adapt project behavior
in the repository instructions and code; update the skill snapshot deliberately
through its recorded source revision so future comparisons remain meaningful.
