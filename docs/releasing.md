# Distribution and releases

Version tags match Cargo.toml exactly: `0.1.4`, without a `v` prefix. GitHub
Releases supplies native `aarch64-apple-darwin` and `x86_64-apple-darwin` archives.
Both target macOS 15.0 and run tests on native macOS 15 runners. Linux remains
experimental for core/preview; Windows is unsupported.

## Installation channels

| Channel | Prerequisites | Update |
| --- | --- | --- |
| Release installer | macOS 15+, system curl/tar/shasum | Rerun at the same installation directory |
| Homebrew tap in this repository | Homebrew, macOS 15+ | `brew update` then `brew upgrade dankosik/codex-retain/codex-retain` |
| GitHub archive | macOS 15+, manual SHA-256 verification | Replace binary at the same path |
| Cargo from Git tag | Rust 1.98.1, C compiler/linker, Git | Reinstall new tag with `--locked --force` |

Installation never enables retention. No registry credentials, self-updater,
background network process, or hidden worker executable is needed. Cargo registry
publication remains disabled. SQLite and zstd are built into the executable;
release linkage is inspected with `otool -L`. Archives include the README,
MIT license and available third-party notices; no external runtime files are
required. Binaries are not Developer ID signed or notarized. SHA-256 checks
integrity against the GitHub-hosted manifest; it is not independent signing.

## Manual installation

Download your architecture's archive and `SHA256SUMS` from the same release.
For Apple Silicon, in a directory containing those two downloads:

```sh
grep '  codex-retain-0.1.4-aarch64-apple-darwin.tar.gz$' SHA256SUMS > selected.sha256
test -s selected.sha256 && shasum -a 256 -c selected.sha256
```

Continue only after the checksum succeeds:

```sh
tar -xzf codex-retain-0.1.4-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 codex-retain-0.1.4-aarch64-apple-darwin/codex-retain "$HOME/.local/bin/codex-retain"
"$HOME/.local/bin/codex-retain" --version
```

Intel users substitute `x86_64-apple-darwin`. If macOS blocks a browser-downloaded
binary, review it and use the system Privacy & Security approval flow. Prefer
the installer for atomic replacement during updates.

## Release procedure

1. Update Cargo version/lockfile and add `docs/releases/VERSION.md`. Stable tags
   use plain `MAJOR.MINOR.PATCH`. Run `make verify`, `shellcheck install.sh`, and
   `actionlint` for the candidate. All profiles must be synthetic.
2. Commit and push; wait for CI success for that exact commit. Release's manual
   `workflow_dispatch` builds and tests archives without publishing.
3. Create an annotated tag at the checked commit and push that exact tag. Release
   runs CI and native release tests for both architectures. It verifies the
   tag/version/repository identity and checks the extracted binary's version,
   help and completions without reading a profile or creating state.
4. Publication waits for all checks and both archives, validates the complete
   inventory, generates SHA256SUMS and a Homebrew formula, uploads a draft, then
   publishes it. Existing releases are never overwritten automatically. If an
   upload fails, inspect the retained draft and assets before completing it;
   do not move the tag or blindly recreate a public release.
5. Download and verify the published archives. Install into an isolated directory
   and exercise version/help/completions. Download `codex-retain.rb` from that
   release into `Formula/codex-retain.rb`, run Homebrew style/install/test, then
   commit and push the formula to main. This advances the tap only after the
   release artifacts exist. No second repository or cross-repository token is
   needed: tap this repository using its explicit Git URL.

The tag describes the release source; the later formula-only commit advertises
its verified binary digests to Homebrew. Future updates use the same sequence.
Do not hand-edit generated checksums or replace an existing version's artifacts.

## Updates and removal

Use the same installation method and path. The scheduler preserves a symlink
invocation only after checking it resolves to the running executable. Homebrew
can switch its bin link to a new Cellar version without resetting grace periods.
Enabling directly from a versioned Cellar or temporary build path cannot provide
this guarantee. Disable before changing paths and enable using the stable command.

Run `codex-retain uninstall` before removing the binary. The schedule is removed
and the policy is disabled; package removal does not erase extra conversations.

## Research behind the choice (2026-09-10)

[RTK's installation guide](https://www.rtk-ai.app/docs/getting-started/installation/)
offers a shell installer, Homebrew tap, Cargo from Git, and binary archives.
Its [installer](https://github.com/rtk-ai/rtk/blob/develop/install.sh) selects a
target, resolves a release, downloads the binary and checksum, and installs to
a user directory. Retain follows this pattern with mandatory checksums and
same-filesystem replacement, preserving its narrower macOS support.

[Homebrew taps](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap) support an
explicit repository URL and Formula directory, so this repository hosts its tap.
[GitHub runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
provide separate macOS ARM64 and Intel labels. Each archive is built and run on
its own architecture; this does not certify a different Codex storage version.
