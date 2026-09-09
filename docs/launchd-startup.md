# Why the hourly job uses normal scheduling

Native testing on macOS 26.4 identified a functional startup problem with
`ProcessType=Background` plus `LowPriorityIO=true`. The real `--scheduled` cleanup
entrypoint reached its five-second Codex-version timeout and safely stopped.

Three isolated, uniquely labeled launchd diagnostic jobs then exercised the
installed Codex executable with disposable HOME/CODEX_HOME directories:

| Diagnostic | Observed result |
| --- | --- |
| npm launcher, both throttling settings | Did not finish within 30 seconds |
| Native Codex executable, both settings | Exited successfully after 19.15 seconds |
| Same npm launcher, neither setting | Exited successfully after 0.267 seconds |

Process samples showed dyld startup/fixup work before ordinary Node/Codex business
logic, excluding SQLite or thread-writer lock contention for this reproduction.
These are single diagnostic observations with sampling overhead, not comparable
performance benchmark claims. The raw experiment is identified in the native
launchd validation receipt. Every temporary registration and sampled process was
removed afterward; the production label was never used.

The product therefore omits both flags. A short hourly process still has no
resident footprint between runs. Keeping a low-priority process alive longer,
particularly while holding a SQLite write transaction or Codex coordination lock,
can also increase contention with interactive clients. We kept the version
validation and timeout instead of hiding the failure by accepting an unknown
version or simply extending an unexplained wait.

`codex --version` itself creates helper files, so the utility probes it inside
a temporary, isolated HOME/CODEX_HOME and removes those startup artifacts. Native
launchd testing must pass with the final generated arguments and scheduling
settings before automatic cleanup is described as validated.
