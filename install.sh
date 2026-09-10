#!/bin/sh
# Install or update a published macOS binary without enabling retention.
set -eu

fail() { printf 'codex-retain installer: %s\n' "$*" >&2; exit 1; }
download() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 --max-time 180 "$@"; }

main() {
    [ "$#" -eq 0 ] || fail 'Use CODEX_RETAIN_VERSION and CODEX_RETAIN_INSTALL_DIR environment variables; no arguments are accepted.'
    [ "$(uname -s)" = Darwin ] || fail 'Binary releases require macOS 15 or later.'
    major=$(sw_vers -productVersion | cut -d. -f1)
    [ "$major" -ge 15 ] || fail 'Binary releases require macOS 15 or later.'
    case "$(uname -m)" in
        arm64|aarch64) target=aarch64-apple-darwin ;;
        x86_64) target=x86_64-apple-darwin ;;
        *) fail 'Unsupported CPU architecture.' ;;
    esac
    for tool in curl tar shasum awk mktemp; do
        command -v "$tool" >/dev/null 2>&1 || fail "Required command is missing: $tool"
    done
    repository=https://github.com/Dankosik/codex-retain
    version=${CODEX_RETAIN_VERSION:-}
    if [ -z "$version" ]; then
        latest=$(download -o /dev/null -w '%{url_effective}' "$repository/releases/latest")
        case "$latest" in
            "$repository"/releases/tag/*) version=${latest##*/} ;;
            *) fail 'Cannot resolve the latest release; set CODEX_RETAIN_VERSION.' ;;
        esac
    fi
    printf '%s\n' "$version" | LC_ALL=C grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || fail 'Version must be a stable tag such as 0.1.0 (without v).'
    install_dir=${CODEX_RETAIN_INSTALL_DIR:-"$HOME/.local/bin"}
    case "$install_dir" in /*) ;; *) fail 'Installation directory must be absolute.' ;; esac
    destination=$install_dir/codex-retain
    [ ! -L "$destination" ] || fail 'Destination is a symlink; update it through its package manager or choose another directory.'
    [ ! -e "$destination" ] || [ -f "$destination" ] || fail 'Destination is not a regular file.'
    temporary=$(mktemp -d)
    staging=
    trap 'rm -rf "$temporary"; if [ -n "$staging" ]; then rm -rf "$staging"; fi' EXIT
    trap 'exit 1' HUP INT TERM
    archive=codex-retain-$version-$target.tar.gz
    base=$repository/releases/download/$version
    printf 'Downloading codex-retain %s (%s)...\n' "$version" "$target"
    download "$base/$archive" -o "$temporary/$archive"
    download "$base/SHA256SUMS" -o "$temporary/SHA256SUMS"
    expected=$(awk -v name="$archive" '$2 == name && NF == 2 {print $1}' "$temporary/SHA256SUMS")
    [ "${#expected}" -eq 64 ] || fail 'Missing, duplicate, or invalid SHA-256 entry.'
    case "$expected" in *[!0-9a-f]*) fail 'Invalid SHA-256 digest.' ;; esac
    actual=$(shasum -a 256 "$temporary/$archive" | awk '{print $1}')
    [ "$expected" = "$actual" ] || fail 'SHA-256 mismatch; existing installation was preserved.'
    # Extract one member to stdout, never archive-controlled filesystem paths.
    tar -xOzf "$temporary/$archive" "codex-retain-$version-$target/codex-retain" > "$temporary/codex-retain"
    chmod 755 "$temporary/codex-retain"
    mkdir "$temporary/home"
    observed=$(HOME="$temporary/home" CODEX_HOME="$temporary/home/codex" CODEX_RETAIN_STATE_DIR="$temporary/home/policy" "$temporary/codex-retain" --version)
    [ "$observed" = "codex-retain $version" ] || fail 'Downloaded binary has an unexpected version.'
    mkdir -p "$install_dir"
    staging=$(mktemp -d "$install_dir/.codex-retain-install.XXXXXXXX")
    cp "$temporary/codex-retain" "$staging/codex-retain"
    chmod 755 "$staging/codex-retain"
    # Same-filesystem rename keeps the old executable intact until replacement.
    mv -f "$staging/codex-retain" "$destination"
    printf 'Installed %s at %s\n' "$observed" "$destination"
    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *) printf 'Add this directory to your shell PATH: %s\n' "$install_dir" ;;
    esac
    printf 'Run codex-retain doctor to check compatibility. Installation does not enable retention.\n'
}

# Keep execution last so a truncated download cannot run a partial installer.
main "$@"
