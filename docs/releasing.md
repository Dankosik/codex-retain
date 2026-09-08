# Releases

The template builds native executable archives for Linux x86_64, macOS Apple
Silicon, macOS Intel, and Windows x86_64. A `v` tag starts the release workflow.
It runs the same required CI as a pull request, validates the tag and repository
against Cargo metadata, builds on each native runner, and tests the executable
after extracting it from its final archive. A single final job publishes the
complete set and SHA-256 checksums as a GitHub Release.

To verify the native packaging pipeline before making a release, open the
**Release** workflow in GitHub Actions and choose **Run workflow** on a branch.
This runs CI, builds and tests all four native archives at the selected commit,
and uploads them as workflow artifacts. Branch dispatches do not validate a
version tag or create a GitHub Release. The publication job runs only for
`refs/tags/v...`; use a branch when requesting verification without publication.

The workflow uses the repository's `GITHUB_TOKEN`; no personal token, crates.io
credentials, package manager account, or signing service is required. Creating a
tag is an intentional publication action. Nothing publishes from ordinary
branch commits, branch dispatches, or pull requests. `publish = false` prevents accidental registry
publication and does not prevent binary releases.

## Before tagging

Initialize the repository's package name and repository URL. Update
`package.version` in `Cargo.toml` and let Cargo refresh the root package entry in
`Cargo.lock`. Run the documented validation commands and review compatibility
changes to arguments, stdout, stderr, exit status, and configuration. Commit
the release changes, wait for required CI, then create and push the matching
tag, such as `v0.1.0`. The tag must identify the exact checked-out commit and
its version must equal Cargo's version.

The publication step refuses to overwrite an existing release. If a run fails
before publication, repair the cause and rerun it where appropriate. If a
release already exists or publication partly succeeded, inspect its assets and
the run before deciding whether to finish that release or issue a new version.
Do not move a tag that users may already have downloaded.

## Local packaging

Rust and Python 3.9+ are sufficient for packaging on a supported host. Replace
the target below with the `host` reported by `rustc -vV`:

```sh
cargo build --release --locked --target aarch64-apple-darwin
python3 scripts/release.py package --target aarch64-apple-darwin --dist dist
```

Packaging derives the binary name and version from `cargo metadata`. It
includes the binary, `README.md`, and `LICENSE`, plus `NOTICE`,
`THIRD_PARTY_NOTICES`, and `THIRD_PARTY_NOTICES.md` when present. Keep required dependency notices current
when changing the distribution's dependencies. The default smoke test runs
`--version` and the sample `stats` command against bytes containing both
terminated and unterminated lines. Replace that smoke case when replacing the
sample command; retain a deterministic check of the real packaged command.

Packaging never cross-compiles or claims to test a foreign executable.
`checksums` validates the full four-target inventory before writing
`SHA256SUMS`; individual local packages do not satisfy that release inventory.

## Supported artifacts

| Target | Build runner | Archive |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Ubuntu 22.04 x86_64 | `.tar.gz` |
| `aarch64-apple-darwin` | macOS 15 Apple Silicon | `.tar.gz` |
| `x86_64-apple-darwin` | macOS 15 Intel | `.tar.gz` |
| `x86_64-pc-windows-msvc` | Windows 2025 x86_64 | `.zip` |

Linux output dynamically links the GNU C runtime and requires glibc 2.35 or
newer with the default build. It is not a musl/static build. The initial macOS
compatibility claim is the native runner version, macOS 15; test any older
deployment target before promising it. macOS binaries are not signed or
notarized, and Windows binaries are not Authenticode signed. Owners can add
signing when their distribution requirements and credentials are known.

Verify a downloaded archive before extracting it. On Linux, run
`sha256sum --check --ignore-missing SHA256SUMS`; on macOS, run
`shasum -a 256 --check --ignore-missing SHA256SUMS`. On Windows, compare
`Get-FileHash -Algorithm SHA256 <archive.zip>` with the matching checksum line.
Checksums detect download corruption; they are not an independent publisher
signature. The initial workflow does not claim signed provenance.

When changing a target, update the release workflow matrix, `TARGETS` in
`scripts/release.py`, this table, and the corresponding verification evidence.
Keep the aggregate `required` CI check as the branch protection target.
