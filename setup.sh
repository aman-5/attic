#!/usr/bin/env bash

# Zero-toolchain setup/update for Attic on Linux and macOS.
#
# Downloads a prebuilt Attic binary from GitHub Releases, verifies its
# SHA-256 checksum, and installs it under:
#
#   $ATTIC_HOME
#
# or, when ATTIC_HOME is not set:
#
#   $HOME/.attic
#
# No Rust/Cargo/native compiler is required for normal users.

set -euo pipefail


REPO="aman-5/attic"
VERSION="${ATTIC_SETUP_VERSION:-latest}"


log() {
    printf '%s\n' "$*" >&2
}


fail() {
    log "ERROR: $*"
    exit 1
}


need() {
    command -v "$1" >/dev/null 2>&1 ||
        fail "required tool '$1' not found on PATH"
}


need curl
need tar


# -----------------------------------------------------------------------------
# 1. Resolve ATTIC_HOME
# -----------------------------------------------------------------------------

if [[ "${ATTIC_HOME+x}" == "x" ]]; then

    if [[ -z "${ATTIC_HOME//[[:space:]]/}" ]]; then
        fail "ATTIC_HOME is set but empty. Remove it or provide a valid directory."
    fi

    ATTIC_HOME_DIR="$ATTIC_HOME"

else

    if [[ -z "${HOME:-}" ]]; then
        fail "could not determine user home directory; set ATTIC_HOME explicitly"
    fi

    ATTIC_HOME_DIR="$HOME/.attic"

fi


mkdir -p "$ATTIC_HOME_DIR" ||
    fail "could not create Attic home directory: $ATTIC_HOME_DIR"


log "Attic home:"
log "  $ATTIC_HOME_DIR"
log ""


# -----------------------------------------------------------------------------
# 2. Detect OS + architecture
# -----------------------------------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"


case "$OS" in

    Linux)

        case "$ARCH" in

            x86_64)
                TARGET="x86_64-unknown-linux-gnu"
                ;;

            *)
                fail "no prebuilt Attic binary currently exists for Linux/$ARCH"
                ;;

        esac
        ;;


    Darwin)

        case "$ARCH" in

            x86_64)
                TARGET="x86_64-apple-darwin"
                ;;

            arm64|aarch64)
                TARGET="aarch64-apple-darwin"
                ;;

            *)
                fail "no prebuilt Attic binary currently exists for macOS/$ARCH"
                ;;

        esac
        ;;


    *)

        fail "unsupported operating system: $OS"
        ;;

esac


log "Detected platform:"
log "  $OS/$ARCH -> $TARGET"
log ""


# -----------------------------------------------------------------------------
# 3. Resolve release
# -----------------------------------------------------------------------------

if [[ "$VERSION" == "latest" ]]; then

    API_URL="https://api.github.com/repos/$REPO/releases/latest"

    RESPONSE_FILE="$(mktemp)"
    HTTP_CODE="$(
        curl \
            -sS \
            -L \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: attic-setup" \
            -o "$RESPONSE_FILE" \
            -w "%{http_code}" \
            "$API_URL"
    )" || {
        rm -f "$RESPONSE_FILE"
        fail "could not reach GitHub to resolve the latest Attic release"
    }


    if [[ "$HTTP_CODE" == "404" ]]; then

        rm -f "$RESPONSE_FILE"

        fail "No published Attic release exists yet."

    fi


    if [[ "$HTTP_CODE" -lt 200 || "$HTTP_CODE" -ge 300 ]]; then

        rm -f "$RESPONSE_FILE"

        fail "GitHub release lookup failed with HTTP status $HTTP_CODE"

    fi


    TAG="$(
        grep -m1 '"tag_name"' "$RESPONSE_FILE" |
            sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
    )"


    rm -f "$RESPONSE_FILE"


    if [[ -z "$TAG" ]]; then
        fail "could not resolve latest Attic release tag"
    fi

else

    TAG="$VERSION"

fi


log "Release:"
log "  $TAG"
log ""


# -----------------------------------------------------------------------------
# 4. Artifact names
# -----------------------------------------------------------------------------

NAME="attic-${TAG}-${TARGET}"


case "$TARGET" in

    *windows*)
        ARCHIVE="${NAME}.zip"
        ;;

    *)
        ARCHIVE="${NAME}.tar.gz"
        ;;

esac


CHECKSUM="${ARCHIVE}.sha256"

BASE_URL="https://github.com/$REPO/releases/download/$TAG"


# -----------------------------------------------------------------------------
# 5. Temporary directory
# -----------------------------------------------------------------------------

WORKDIR="$(mktemp -d)"

trap 'rm -rf "$WORKDIR"' EXIT


# -----------------------------------------------------------------------------
# 6. Download archive
# -----------------------------------------------------------------------------

log "Downloading:"
log "  $ARCHIVE"


curl \
    -fsSL \
    -o "$WORKDIR/$ARCHIVE" \
    "$BASE_URL/$ARCHIVE" ||
    fail "download failed: $BASE_URL/$ARCHIVE"


curl \
    -fsSL \
    -o "$WORKDIR/$CHECKSUM" \
    "$BASE_URL/$CHECKSUM" ||
    fail "checksum download failed: $BASE_URL/$CHECKSUM; refusing to install an unverified binary"


# -----------------------------------------------------------------------------
# 7. Verify SHA-256
# -----------------------------------------------------------------------------

(
    cd "$WORKDIR"

    if command -v sha256sum >/dev/null 2>&1; then

        sha256sum -c "$CHECKSUM"

    elif command -v shasum >/dev/null 2>&1; then

        shasum -a 256 -c "$CHECKSUM"

    else

        fail "neither sha256sum nor shasum is available; cannot verify Attic"

    fi

) || fail "checksum verification FAILED for $ARCHIVE"


log "Checksum OK"
log ""


# -----------------------------------------------------------------------------
# 8. Extract
# -----------------------------------------------------------------------------

tar \
    -xzf "$WORKDIR/$ARCHIVE" \
    -C "$WORKDIR"


SOURCE_BINARY="$WORKDIR/$NAME/attic-server"


if [[ ! -f "$SOURCE_BINARY" ]]; then

    fail "release archive does not contain attic-server at the expected location"

fi


# -----------------------------------------------------------------------------
# 9. Install/update
# -----------------------------------------------------------------------------

BIN_PATH="$ATTIC_HOME_DIR/attic-server"


cp "$SOURCE_BINARY" "$BIN_PATH" ||
    fail "could not install Attic to $BIN_PATH"


chmod +x "$BIN_PATH" ||
    fail "could not mark $BIN_PATH executable"


log "Attic installed successfully:"
log "  $BIN_PATH"
log ""


# -----------------------------------------------------------------------------
# 10. Print MCP configuration
# -----------------------------------------------------------------------------

cat <<EOF

Add Attic to your AI client's MCP configuration:

{
  "mcpServers": {
    "attic": {
      "command": "$BIN_PATH",
      "args": []
    }
  }
}

Attic uses MCP over stdio.

No repository configuration is required in the MCP JSON.

After your AI client connects to Attic, tell it:

  Configure Attic to index these repositories:
  /path/to/repo-a
  /path/to/repo-b
  /path/to/repo-c

Attic persists its workspace configuration under:

  $ATTIC_HOME_DIR

Running setup.sh again updates the installed Attic binary to the latest
published release.

EOF