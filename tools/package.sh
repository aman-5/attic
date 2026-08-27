#!/usr/bin/env bash
# Phase 7 release packaging for Attic.
#
# Builds attic-server for a target triple and assembles a clean release
# archive with the canonical layout:
#
#   attic-v<version>-<target>/
#     attic-server[.exe]        the server binary (only executable)
#     README.md                 product documentation
#     LICENSE-MIT               license texts
#     LICENSE-APACHE
#     docs/                     operator documentation (md only)
#
# The archive NEVER contains: target/, Cargo build artifacts, developer
# scripts, local configuration, test databases, logs, or hidden files.
#
# Usage:
#   tools/package.sh --target <triple> [--out <dir>] [--verify <archive-dir>]
#
# Cross-compilation targets:
#   x86_64-pc-windows-msvc      Windows x86_64
#   x86_64-unknown-linux-gnu    Linux x86_64
#   x86_64-apple-darwin         macOS x86_64
#   aarch64-apple-darwin        macOS ARM64
set -euo pipefail

usage() { echo "usage: $0 --target <triple> [--out <dir>] | --verify <dir>" >&2; exit 2; }

MODE=build
TARGET=
OUT=dist
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --verify) MODE=verify; VERIFY_DIR="$2"; shift 2 ;;
    *) usage ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"

SUPPORTED_TARGETS="x86_64-pc-windows-msvc x86_64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"

if [[ "$MODE" == verify ]]; then
  DIR="${VERIFY_DIR:?--verify requires a directory}"
  NAME="$(basename "$DIR")"
  echo "== verifying release archive layout: $NAME"
  # Layout invariants
  test -d "$DIR" || { echo "FAIL: $DIR missing"; exit 1; }
  BIN_COUNT=0
  if [[ -f "$DIR/attic-server" ]]; then BIN_COUNT=$((BIN_COUNT+1)); fi
  if [[ -f "$DIR/attic-server.exe" ]]; then BIN_COUNT=$((BIN_COUNT+1)); fi
  test "$BIN_COUNT" -eq 1 || { echo "FAIL: exactly one attic-server binary expected"; exit 1; }
  test -f "$DIR/README.md" || { echo "FAIL: README.md missing"; exit 1; }
  test -d "$DIR/docs" || { echo "FAIL: docs/ missing"; exit 1; }
  # Exclusion invariants: no developer artifacts, no logs, no DBs, no target/.
  if find "$DIR" \( -name 'target' -o -name '*.db' -o -name '*.db-*' -o -name '*.log' \
      -o -name 'build_errors*' -o -name '__rsfiles*' -o -name '*.tmp' \
      -o -name 'attic.db*' -o -name 'semantic.db*' -o -name '.attic' \) -print -quit | grep -q .; then
    echo "FAIL: developer artifacts or runtime data found in release archive"
    exit 1
  fi
  # No hidden files.
  if find "$DIR" -name '.*' -not -name '.' -print -quit | grep -q .; then
    echo "FAIL: hidden files in release archive"
    exit 1
  fi
  echo "OK: archive layout verified for $NAME"
  exit 0
fi

if [[ -z "$TARGET" ]]; then usage; fi
case " $SUPPORTED_TARGETS " in
  *" $TARGET "*) ;;
  *) echo "unsupported target: $TARGET (supported: $SUPPORTED_TARGETS)" >&2; exit 2 ;;
esac

cd "$REPO_ROOT"
echo "== building attic-server for $TARGET"
cargo build --release --package attic-server --target "$TARGET"

# The Cargo package is named `attic-server` but its `[[bin]]` target is
# named `attic` (see crates/attic-server/Cargo.toml) — cargo therefore
# produces `target/$TARGET/release/attic[.exe]`, NOT `attic-server[.exe]`.
# The release archive renames it to `attic-server` for a clearer end-user
# binary name; this staging copy is the only place that rename happens.
CARGO_EXE="attic"
STAGED_EXE="attic-server"
[[ "$TARGET" == *windows* ]] && CARGO_EXE="attic.exe" && STAGED_EXE="attic-server.exe"
BIN="target/$TARGET/release/$CARGO_EXE"
test -f "$BIN" || { echo "FAIL: binary not found at $BIN" >&2; exit 1; }

NAME="attic-v${VERSION}-${TARGET}"
STAGE="$OUT/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/docs"

cp "$BIN" "$STAGE/$STAGED_EXE"
cp "$REPO_ROOT/README.md" "$STAGE/"
for lic in LICENSE-MIT LICENSE-APACHE; do
  [[ -f "$REPO_ROOT/$lic" ]] && cp "$REPO_ROOT/$lic" "$STAGE/"
done
# Operator docs only (markdown), preserving relative structure under docs/.
(cd "$REPO_ROOT" && find docs -name '*.md' -type f) | while read -r doc; do
  mkdir -p "$STAGE/$(dirname "$doc")"
  cp "$REPO_ROOT/$doc" "$STAGE/$doc"
done

echo "== verifying staged archive"
"$0" --verify "$STAGE"

ARCHIVE="$OUT/${NAME}"
case "$TARGET" in
  *windows*) (cd "$OUT" && command -v zip >/dev/null && zip -qr "${NAME}.zip" "$NAME" || tar -czf "${NAME}.tar.gz" "$NAME") ;;
  *) (cd "$OUT" && tar -czf "${NAME}.tar.gz" "$NAME") ;;
esac

echo "OK: $ARCHIVE (+ compressed archive in $OUT)"
