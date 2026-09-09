# Native releases

The adapted template produces native macOS archives for
`aarch64-apple-darwin` and `x86_64-apple-darwin`. Native CI runners build and test
their own target. Linux is a core-test/preview experiment, not a deletion release;
Windows is deferred because the current filesystem adapter is Unix-specific.

Build and package locally:

```sh
cargo build --release --locked
python3 scripts/release.py package --target aarch64-apple-darwin --dist dist
```

Packaging validates the exact archive inventory and executes the extracted
binary's version, help and Bash completions in an isolated environment. These
commands must not read Codex state or create policy state. The archive contains
the executable, README, MIT license and available third-party notices.

The release workflow requires all applicable CI jobs, exact tag/version/source
identity, both native archives and a SHA256SUMS manifest before publication.
It uses pinned action revisions and no release authority from a local test.
A local archive smoke test establishes only that target's covered behavior;
an unrun remote job or unpublished GitHub release must not be reported as passed.

Package-source publication to crates.io is disabled. A maintainer should first
create/configure the intended repository, review reporting channels and release
ownership, then push an authorized tag only after its exact candidate is green.

Installed users update at the existing executable path. `cargo install --path .
--locked --force` replaces the utility without changing its policy. Before
uninstalling, run `codex-retain uninstall`; then use the package manager or remove
the installed binary. There is no hidden copied worker executable and no
self-updating network process.
