<#
.SYNOPSIS
  Zero-toolchain setup for Attic (Windows).

.DESCRIPTION
  Normal-user path: downloads the prebuilt Attic binary for Windows x86_64
  from the project's GitHub Releases, verifies its SHA-256 checksum,
  installs it locally (no administrator privileges), and prints
  ready-to-paste MCP client configuration.

  This does NOT compile Attic and does NOT require Rust/Cargo/MSVC Build
  Tools. Contributors who want to build from source should use
  `cargo build --release --package attic-server` instead — see
  docs/PLAYBOOK.md.

.PARAMETER Version
  Release tag to install (e.g. "v0.1.0"). Defaults to the latest release.
#>
param(
    [string]$Version = "latest"
)

$ErrorActionPreference = "Stop"
$Repo = "aman-5/attic"

function Fail($msg) {
    Write-Error "ERROR: $msg"
    exit 1
}

# ── 1. Detect architecture → release target triple ──────────────────────────
$arch = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
if ($arch -ne [System.Runtime.InteropServices.Architecture]::X64) {
    Fail "no prebuilt Attic binary for Windows/$arch yet — build from source (docs/PLAYBOOK.md)"
}
$target = "x86_64-pc-windows-msvc"
Write-Host "detected platform: Windows/x64 -> $target"

# ── 2. Resolve the release tag ───────────────────────────────────────────────
if ($Version -eq "latest") {
    $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "attic-setup" }
    } catch {
        Fail "could not reach $apiUrl to resolve the latest release: $_"
    }
    $tag = $release.tag_name
    if (-not $tag) { Fail "could not resolve latest release tag from $apiUrl" }
} else {
    $tag = $Version
}
Write-Host "release tag: $tag"

$name = "attic-$tag-$target"
$archive = "$name.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"

# ── 3. Download archive + published checksum over HTTPS ─────────────────────
$workDir = Join-Path $env:TEMP ("attic-setup-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $workDir -Force | Out-Null
try {
    $archivePath = Join-Path $workDir $archive
    $checksumPath = Join-Path $workDir "$archive.sha256"

    Write-Host "downloading $archive ..."
    try {
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath -UseBasicParsing
    } catch {
        Fail "download failed: $baseUrl/$archive"
    }
    try {
        Invoke-WebRequest -Uri "$baseUrl/$archive.sha256" -OutFile $checksumPath -UseBasicParsing
    } catch {
        Fail "checksum download failed: $baseUrl/$archive.sha256 (refusing to install an unverified binary)"
    }

    # ── 4. Verify integrity BEFORE extracting anything ──────────────────────
    $expected = (Get-Content $checksumPath | Select-Object -First 1) -split '\s+' | Select-Object -First 1
    $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash
    if ($expected.ToLower() -ne $actual.ToLower()) {
        Fail "checksum verification FAILED for $archive (expected $expected, got $actual) — refusing to install"
    }
    Write-Host "checksum OK"

    # ── 5. Extract and install (no admin; user-local install directory) ─────
    Expand-Archive -Path $archivePath -DestinationPath $workDir -Force

    $installRoot = if ($env:ATTIC_DATA_DIR) { $env:ATTIC_DATA_DIR } else { Join-Path $env:LOCALAPPDATA "attic" }
    $binDir = Join-Path $installRoot "bin"
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null

    $srcExe = Join-Path (Join-Path $workDir $name) "attic-server.exe"
    $binPath = Join-Path $binDir "attic-server.exe"
    Copy-Item -Path $srcExe -Destination $binPath -Force

    Write-Host "installed: $binPath"

    # ── 6. Print ready-to-paste MCP configuration ────────────────────────────
    $binPathJson = $binPath.Replace('\', '\\')
    Write-Host ""
    Write-Host "Attic is installed at:"
    Write-Host "  $binPath"
    Write-Host ""
    Write-Host "Add this to your MCP client's server configuration, then set"
    Write-Host "ATTIC_WORKSPACE_ROOT to the repository (or multi-repo workspace root) you"
    Write-Host "want Attic to index:"
    Write-Host ""
    Write-Host "{"
    Write-Host "  ""mcpServers"": {"
    Write-Host "    ""attic"": {"
    Write-Host "      ""command"": ""$binPathJson"","
    Write-Host "      ""args"": [],"
    Write-Host "      ""env"": {"
    Write-Host "        ""ATTIC_WORKSPACE_ROOT"": ""C:\\absolute\\path\\to\\your\\repo"""
    Write-Host "      }"
    Write-Host "    }"
    Write-Host "  }"
    Write-Host "}"
    Write-Host ""
    Write-Host "See docs/PLAYBOOK.md for troubleshooting and docs/ARCHITECTURE.md for how"
    Write-Host "Attic works."
} finally {
    Remove-Item -Path $workDir -Recurse -Force -ErrorAction SilentlyContinue
}
