#!/usr/bin/env bash
# Zero-toolchain setup for Attic (Linux / macOS).
#
# Normal-user path: downloads the prebuilt Attic binary for this machine from
# the project's GitHub Releases, verifies its SHA-256 checksum, installs it
# locally (no sudo/root), and prints ready-to-paste MCP client configuration.
#
# This does NOT compile Attic and does NOT require Rust/Cargo/a C compiler.
# Contributors who want to build from source should use `cargo build
# --release --package attic-server` instead — see docs/PLAYBOOK.md.
set -euo pipefail

REPO="aman-5/attic"
VERSION="${ATTIC_SETUP_VERSION:-latest}"

log()  { printf '%s\n' "$*" >&2; }
fail() { log "ERROR: $*"; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "required tool '$1' not found on PATH"; }
need curl
need tar

# ── 1. Detect OS + architecture → release target triple ─────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
      *) fail "no prebuilt Attic binary for Linux/$ARCH yet — build from source (docs/PLAYBOOK.md)" ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      x86_64) TARGET="x86_64-apple-darwin" ;;
      arm64)  TARGET="aarch64-apple-darwin" ;;
      *) fail "unrecognized macOS architecture: $ARCH" ;;
    esac
    ;;
  *) fail "unsupported OS: $OS — use setup.ps1 on Windows, or build from source" ;;
esac
log "detected platform: $OS/$ARCH -> $TARGET"

# ── 2. Resolve the release tag ────────────────────────────────────────────────
if [[ "$VERSION" == "latest" ]]; then
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
  TAG="$(curl -fsSL "$API_URL" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  [[ -n "$TAG" ]] || fail "could not resolve latest release tag from $API_URL"
else
  TAG="$VERSION"
fi
log "release tag: $TAG"

NAME="attic-${TAG}-${TARGET}"
ARCHIVE="${NAME}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

# ── 3. Download archive + published checksum over HTTPS ─────────────────────
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

log "downloading $ARCHIVE ..."
curl -fsSL -o "$WORKDIR/$ARCHIVE" "$BASE_URL/$ARCHIVE" \
  || fail "download failed: $BASE_URL/$ARCHIVE"
curl -fsSL -o "$WORKDIR/$ARCHIVE.sha256" "$BASE_URL/$ARCHIVE.sha256" \
  || fail "checksum download failed: $BASE_URL/$ARCHIVE.sha256 (refusing to install an unverified binary)"

# ── 4. Verify integrity BEFORE extracting anything ───────────────────────────
( cd "$WORKDIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$ARCHIVE.sha256"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$ARCHIVE.sha256"
  else
    fail "neither sha256sum nor shasum found — cannot verify archive integrity"
  fi
) || fail "checksum verification FAILED for $ARCHIVE — refusing to install"
log "checksum OK"

# ── 5. Extract and install (no sudo; user-local install directory) ──────────
tar -xzf "$WORKDIR/$ARCHIVE" -C "$WORKDIR"

INSTALL_ROOT="${ATTIC_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/attic}"
BIN_DIR="$INSTALL_ROOT/bin"
mkdir -p "$BIN_DIR"
cp "$WORKDIR/$NAME/attic-server" "$BIN_DIR/attic-server"
chmod +x "$BIN_DIR/attic-server"

BIN_PATH="$BIN_DIR/attic-server"
log "installed: $BIN_PATH"

# ── 6. Print ready-to-paste MCP configuration ────────────────────────────────
cat <<EOF

Attic is installed at:
  $BIN_PATH

Add this to your MCP client's server configuration, then set
ATTIC_WORKSPACE_ROOT to the repository (or multi-repo workspace root) you
want Attic to index:

{
  "mcpServers": {
    "attic": {
      "command": "$BIN_PATH",
      "args": [],
      "env": {
        "ATTIC_WORKSPACE_ROOT": "/absolute/path/to/your/repo"
      }
    }
  }
}

See docs/PLAYBOOK.md for troubleshooting and docs/ARCHITECTURE.md for how
Attic works.
EOF
