# Local builds and publication policy

Codex Retain is currently an unreleased source project on GitHub. There are no
version tags, GitHub Releases, registry packages, deployments, or automated
publication workflows. The version in Cargo.toml is required build metadata;
it does not announce an official release. Cargo registry publication remains
disabled with `publish = false`.

The retained packaging helper is available for local development and validation.
It supports native macOS archives for `aarch64-apple-darwin` and
`x86_64-apple-darwin`. Linux is a core-test/preview experiment; Windows is deferred.

```sh
cargo build --release --locked
python3 scripts/release.py package --target aarch64-apple-darwin --dist dist
```

Packaging validates the exact archive inventory and executes the extracted
binary's version, help and Bash completions in an isolated environment. These
commands must not read Codex state or create policy state. The archive contains
the executable, README, MIT license and available third-party notices. Creating
one locally does not upload it or create a public release.

CI checks formatting, static diagnostics, tests and dependency policy. It has
read-only repository permissions and no publication job. Any future versioning,
registry distribution, signed release or deployment requires a separate decision
and an explicit publication workflow.

Installed users update at the existing executable path. `cargo install --path .
--locked --force` replaces the utility without changing its policy. Before
uninstalling, run `codex-retain uninstall`; then use the package manager or remove
the installed binary. There is no hidden copied worker executable and no
self-updating network process.
