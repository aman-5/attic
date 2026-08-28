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

usage() { echo "usage: $0 --target <triple> [--out <dir>] [--stage-only] | --verify <dir>" >&2; exit 2; }

MODE=build
STAGE_ONLY=false
TARGET=
OUT=dist
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --verify) MODE=verify; VERIFY_DIR="$2"; shift 2 ;;
    --stage-only) STAGE_ONLY=true; shift ;;
    *) usage ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${ATTIC_RELEASE_VERSION:-$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')}"

SUPPORTED_TARGETS="x86_64-pc-windows-msvc x86_64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"

if [[ "$MODE" == verify ]]; then
  DIR="${VERIFY_DIR:?--verify requires a directory}"
  NAME="$(basename "$DIR")"
  echo "== verifying release archive layout: $NAME"
  # Layout invariants
  test -d "$DIR" || { echo "FAIL: $DIR missing"; exit 1; }
  # NOTE: `[[ -f "$DIR/attic-server" ]]` is NOT safe here — on Windows/MSYS
  # bash (exactly what GitHub's windows-latest runner uses for `run: bash
  # ...` steps), that stat call resolves through PATHEXT-style suffix
  # matching and reports true even when only `attic-server.exe` exists,
  # double-counting a single binary. `find` lists real directory entries
  # and isn't subject to that resolution.
  BIN_COUNT=$(find "$DIR" -maxdepth 1 -type f \( -name 'attic-server' -o -name 'attic-server.exe' \) | wc -l)
  test "$BIN_COUNT" -eq 1 || { echo "FAIL: exactly one attic-server binary expected (found $BIN_COUNT)"; exit 1; }
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
# FINAL_VALIDATION_TODO.md is an internal pre-release validation checklist,
# not end-user documentation — deliberately excluded from release archives.
(cd "$REPO_ROOT" && find docs -name '*.md' -type f ! -name 'FINAL_VALIDATION_TODO.md') | while read -r doc; do
  mkdir -p "$STAGE/$(dirname "$doc")"
  cp "$REPO_ROOT/$doc" "$STAGE/$doc"
done

echo "== verifying staged archive"
bash "$0" --verify "$STAGE"

# Windows uses --stage-only. The GitHub workflow creates the ZIP in native
# PowerShell, avoiding Git Bash/MSYS path rewriting at the Bash -> PowerShell
# process boundary.
if [[ "$STAGE_ONLY" == true ]]; then
  echo "OK: staged release directory: $STAGE"
  exit 0
fi

ARCHIVE_BASE="$OUT/${NAME}"

# Remove stale archives for this exact release/target so checksum selection is
# deterministic even when dist/ is reused locally.
rm -f "${ARCHIVE_BASE}.zip" "${ARCHIVE_BASE}.zip.sha256" \
      "${ARCHIVE_BASE}.tar.gz" "${ARCHIVE_BASE}.tar.gz.sha256"

case "$TARGET" in
  *windows*)
    echo "FAIL: Windows archive creation must run in the native PowerShell release step; use --stage-only" >&2
    exit 1
    ;;
  *)
    (cd "$OUT" && tar -czf "${NAME}.tar.gz" "$NAME")
    COMPRESSED="${ARCHIVE_BASE}.tar.gz"
    ;;
esac
test -f "$COMPRESSED" || { echo "FAIL: expected archive not created: $COMPRESSED" >&2; exit 1; }

# Checksum the exact compressed archive the installer will download.
if command -v sha256sum >/dev/null; then
  (cd "$OUT" && sha256sum "$(basename "$COMPRESSED")" > "$(basename "$COMPRESSED").sha256")
elif command -v shasum >/dev/null; then
  (cd "$OUT" && shasum -a 256 "$(basename "$COMPRESSED")" > "$(basename "$COMPRESSED").sha256")
else
  echo "FAIL: no sha256sum/shasum found; release archives must have checksums" >&2
  exit 1
fi

test -f "${COMPRESSED}.sha256" || { echo "FAIL: checksum not created" >&2; exit 1; }

echo "OK: $COMPRESSED (+ ${COMPRESSED}.sha256)"