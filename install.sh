#!/bin/sh
# cupel installer - downloads the latest release binary for this platform.
#
#   curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
#
# No Rust toolchain required. Everything lives in ONE home directory
# (cargo-style), so backup and uninstall are a single path:
#
#   ~/.cupel/
#   ├── bin/cupel      the binary (this dir goes on PATH)
#   ├── AGENTS.md      global context, loaded into every session (optional)
#   ├── prompts/*.md   global /command prompt templates
#   └── memory/        reserved for the future memory feature
#
# Override the home with CUPEL_HOME, or just the binary location with
# CUPEL_INSTALL_DIR. POSIX sh (not bash) so it runs on stock Debian,
# Alpine, and macOS alike.
#
# Nice side effect on macOS: curl downloads don't get the Gatekeeper
# quarantine attribute, so the unsigned binary runs without any
# "unidentified developer" ceremony (browser downloads DO get quarantined).

set -eu

REPO="rommeld/cupel"
CUPEL_HOME="${CUPEL_HOME:-$HOME/.cupel}"
INSTALL_DIR="${CUPEL_INSTALL_DIR:-$CUPEL_HOME/bin}"

# ---- Pick the release asset for this platform ------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin)
        # One universal binary covers Apple Silicon and Intel.
        asset="cupel-macos-universal"
        ;;
    Linux)
        case "$arch" in
            x86_64) asset="cupel-linux-x86_64" ;;
            aarch64 | arm64) asset="cupel-linux-aarch64" ;;
            *)
                echo "error: unsupported Linux architecture: $arch" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "error: unsupported platform: $os (Windows is not supported yet)" >&2
        exit 1
        ;;
esac

# `releases/latest/download/` always points at the newest release - no API
# call or JSON parsing needed.
url="https://github.com/$REPO/releases/latest/download/$asset.tar.gz"

# ---- Download and verify ----------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $url"
curl -fsSL "$url" -o "$tmp/$asset.tar.gz"

# Verify against the published checksums when a checksum tool exists;
# a missing tool downgrades to a warning rather than blocking the install.
if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
    curl -fsSL "https://github.com/$REPO/releases/latest/download/sha256sums.txt" \
        -o "$tmp/sha256sums.txt"
    expected="$(grep "$asset.tar.gz" "$tmp/sha256sums.txt" | cut -d' ' -f1)"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/$asset.tar.gz" | cut -d' ' -f1)"
    else
        actual="$(shasum -a 256 "$tmp/$asset.tar.gz" | cut -d' ' -f1)"
    fi
    if [ "$expected" != "$actual" ]; then
        echo "error: checksum mismatch for $asset.tar.gz" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        exit 1
    fi
else
    echo "warning: no sha256 tool found; skipping checksum verification" >&2
fi

# ---- Install ----------------------------------------------------------------
tar -xzf "$tmp/$asset.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/cupel" "$INSTALL_DIR/cupel"
echo "installed cupel to $INSTALL_DIR/cupel"

# Scaffold the config side of the home (mkdir -p never overwrites anything).
mkdir -p "$CUPEL_HOME/prompts"
echo "cupel home: $CUPEL_HOME (global AGENTS.md and prompts/*.md live here)"

# Releases before v0.1.4-beta installed to ~/.local/bin; a stale binary
# there would shadow or be shadowed depending on PATH order.
if [ "$INSTALL_DIR" != "$HOME/.local/bin" ] && [ -x "$HOME/.local/bin/cupel" ]; then
    echo "warning: an older cupel install exists at ~/.local/bin/cupel - consider removing it" >&2
fi

# ---- Make `cupel` work as a bare command ------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        echo "run 'cupel' to get started"
        ;;
    *)
        echo ""
        echo "note: $INSTALL_DIR is not on your PATH. Add it with:"
        echo ""
        echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.$(basename "${SHELL:-sh}")rc"
        echo ""
        echo "then restart your shell and run 'cupel'."
        ;;
esac
