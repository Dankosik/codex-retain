# Contributing

Issues and pull requests are welcome. For a bug, include the command, relevant
input, expected and observed output, exit status, operating system, and tool
version. Remove secrets and personal data from reproductions. For an improvement,
explain the user need and the behavior that would change.

Use the toolchain declared by `rust-toolchain.toml`. Start with the README's
quickstart and [first-command guide](docs/first-command.md). Contributors using
coding agents should read [AGENTS.md](AGENTS.md).

Keep changes focused and preserve existing CLI contracts unless the change
explicitly revises them. Prefer the standard library and existing dependencies.
Include focused regression coverage for changed behavior; exercise the actual
executable when arguments, streams, status, or effects change. Choose fixtures
that expose a plausible defect rather than merely repeating the implementation.

Before opening a code pull request, run the applicable standard checks with
`make check`:

```sh
make check
```

The [Makefile](Makefile) runs formatting, Clippy, all-target tests, and doctests.
Those Cargo commands can also be run individually for focused development.
Use `make verify` when changing template initialization or maintenance scripts;
it adds the template checks and Python tests. Use `make template-smoke` to
initialize, build, and test a disposable consumer after identity changes.
Explain any check you could not run and retain required CI gates.
Do not broaden a documentation-only edit into an unrelated runtime test campaign.

Describe the problem, resulting behavior, and verification in the pull request.
For performance changes, include a reproducible workload and comparable release
measurements. Dependency changes should explain the current capability they
provide and account for feature, license, platform, and maintenance implications.

Report suspected vulnerabilities through [SECURITY.md](SECURITY.md) rather than
public issue details. Contributions are distributed under the repository's
[MIT license](LICENSE).
