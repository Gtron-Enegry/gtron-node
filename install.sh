#!/bin/sh
# gtron-node installer.
#
#   curl -fsSL https://gtron.network/install-node.sh | sh
#
# Downloads the gtron-node release for this machine from
# https://github.com/Gtron-Enegry/gtron-node/releases, checks it against the release's
# SHA256SUMS, installs the binary and starts `gtron-node setup`. Run it again to upgrade:
# an installed service gets the new binary and a restart, its settings and state stay.
#
# Options (after `sh -s --`):
#   --version X.Y.Z   install that release instead of the latest
#   --no-setup        only install the binary
#   --dir DIR         where to put the binary (default /usr/local/bin, or ~/.local/bin)
#   GTRON_NODE_BASE_URL=https://…  download from a mirror of the release files
#
# Read it first if you like: everything below is plain POSIX sh.

set -eu

REPO="Gtron-Enegry/gtron-node"

# Everything runs from main, called on the last line: a download cut short runs nothing.
main() {
    version=""
    setup=1
    dir=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --version) version="${2:?--version needs a value}"; shift 2 ;;
            --version=*) version="${1#*=}"; shift ;;
            --no-setup) setup=0; shift ;;
            --dir) dir="${2:?--dir needs a value}"; shift 2 ;;
            --dir=*) dir="${1#*=}"; shift ;;
            -h | --help) sed -n '2,18p' "$0" 2>/dev/null || true; exit 0 ;;
            *) fail "unknown option: $1" ;;
        esac
    done

    colors
    printf '\n  %s gtron-node installer %s\n\n' "$BOLD$CYAN" "$RESET"

    target="$(detect_target)"
    step "Platform" "$target"

    if [ -n "${GTRON_NODE_BASE_URL:-}" ]; then
        # A mirror of the release files (still https only, still checked against SHA256SUMS).
        base="${GTRON_NODE_BASE_URL%/}"
    elif [ -n "$version" ]; then
        base="https://github.com/$REPO/releases/download/v${version#v}"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi
    asset="gtron-node-$target.tar.gz"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    fetch "$base/$asset" "$tmp/$asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
    verify "$tmp" "$asset"
    ok "Checksum matches SHA256SUMS"

    tar -xzf "$tmp/$asset" -C "$tmp"
    bin="$tmp/gtron-node-$target/gtron-node"
    [ -x "$bin" ] || fail "the archive has no gtron-node binary"
    new_version="$("$bin" --version 2>/dev/null | awk '{print $2}')"
    [ -n "$new_version" ] || fail "the downloaded binary does not run on this machine"
    ok "Downloaded gtron-node $new_version"

    sudo=""
    if [ -z "$dir" ]; then
        if [ "$(id -u)" -eq 0 ] || [ -w /usr/local/bin ]; then
            dir=/usr/local/bin
        elif is_linux_service_host && have sudo; then
            # A system service needs root anyway; put the binary where the unit expects it.
            dir=/usr/local/bin
            sudo="sudo"
        else
            dir="$HOME/.local/bin"
        fi
    fi
    if [ -z "$sudo" ] && [ -d "$dir" ] && [ ! -w "$dir" ]; then
        have sudo || fail "$dir is not writable; use --dir"
        sudo="sudo"
    fi
    [ -n "$sudo" ] && note "Installing to $dir needs sudo"
    $sudo mkdir -p "$dir"
    $sudo install -m 0755 "$bin" "$dir/gtron-node.new"
    $sudo mv -f "$dir/gtron-node.new" "$dir/gtron-node"
    ok "Installed $dir/gtron-node"
    case ":$PATH:" in
        *":$dir:"*) ;;
        *) warn "$dir is not on your PATH; add it, or call $dir/gtron-node" ;;
    esac

    # Already running as a service: upgrade in place, keep settings and state.
    if upgrade_running "$dir"; then
        printf '\n  %s Upgraded to %s.%s\n\n' "$GREEN●$RESET" "$new_version" ""
        exit 0
    fi

    if [ "$setup" -eq 0 ]; then
        printf '\n  Next: %s%s setup%s\n\n' "$BOLD" "$dir/gtron-node" "$RESET"
        exit 0
    fi
    if [ ! -r /dev/tty ]; then
        printf '\n  No terminal to ask questions in. Run: %s%s setup%s\n\n' "$BOLD" "$dir/gtron-node" "$RESET"
        exit 0
    fi
    printf '\n'
    # `curl | sh` gives the script the download as stdin; setup reads the terminal.
    if is_linux_service_host && [ "$(id -u)" -ne 0 ] && have sudo; then
        note "Setup runs with sudo to install the system service"
        exec sudo "$dir/gtron-node" setup </dev/tty
    fi
    exec "$dir/gtron-node" setup </dev/tty
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) fail "unsupported CPU: $arch (x86_64 and arm64 are built)" ;;
    esac
    case "$os" in
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin) echo "$arch-apple-darwin" ;;
        *) fail "unsupported system: $os (Linux and macOS are built; see the README to build it)" ;;
    esac
}

is_linux_service_host() {
    [ "$(uname -s)" = Linux ] && [ -d /run/systemd/system ]
}

upgrade_running() {
    if is_linux_service_host && [ -f /etc/systemd/system/gtron-node.service ]; then
        s=""
        [ "$(id -u)" -eq 0 ] || s="sudo"
        if [ "$1/gtron-node" != /usr/local/bin/gtron-node ]; then
            $s install -m 0755 "$1/gtron-node" /usr/local/bin/gtron-node
        fi
        note "Restarting the gtron-node service (the job in hand finishes first)"
        $s systemctl restart gtron-node
        ok "Service restarted: journalctl -u gtron-node -f"
        return 0
    fi
    plist="$HOME/Library/LaunchAgents/network.gtron.node.plist"
    if [ "$(uname -s)" = Darwin ] && [ -f "$plist" ]; then
        note "Restarting the gtron-node background service"
        launchctl kickstart -k "gui/$(id -u)/network.gtron.node"
        ok "Service restarted: tail -f ~/.gtron-node/node.log"
        return 0
    fi
    return 1
}

fetch() {
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1" || fail "download failed: $1"
    elif have wget; then
        wget -q --https-only -O "$2" "$1" || fail "download failed: $1"
    else
        fail "needs curl or wget"
    fi
}

verify() {
    expected="$(awk -v f="$2" '$2 == f || $2 == "*"f {print $1}' "$1/SHA256SUMS")"
    [ -n "$expected" ] || fail "$2 is not listed in SHA256SUMS"
    if have sha256sum; then
        actual="$(sha256sum "$1/$2" | awk '{print $1}')"
    elif have shasum; then
        actual="$(shasum -a 256 "$1/$2" | awk '{print $1}')"
    else
        fail "needs sha256sum or shasum to check the download"
    fi
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $2: not installing it"
}

have() { command -v "$1" >/dev/null 2>&1; }

colors() {
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
        BOLD="$(printf '\033[1m')"; DIM="$(printf '\033[2m')"; RESET="$(printf '\033[0m')"
        GREEN="$(printf '\033[32m')"; YELLOW="$(printf '\033[33m')"; RED="$(printf '\033[31m')"
        CYAN="$(printf '\033[36m')"
    else
        BOLD=""; DIM=""; RESET=""; GREEN=""; YELLOW=""; RED=""; CYAN=""
    fi
}

step() { printf '  %s%-9s%s %s\n' "$DIM" "$1" "$RESET" "$2"; }
ok() { printf '  %s✓%s %s\n' "$GREEN" "$RESET" "$1"; }
note() { printf '  %s•%s %s\n' "$CYAN" "$RESET" "$1"; }
warn() { printf '  %s!%s %s\n' "$YELLOW" "$RESET" "$1"; }
fail() {
    colors 2>/dev/null || true
    printf '  %s✗ %s%s\n' "$RED" "$1" "$RESET" >&2
    exit 1
}

main "$@"
