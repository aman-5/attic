<#
.SYNOPSIS
  Zero-toolchain setup/update for Attic on Windows.

.DESCRIPTION
  Downloads the latest prebuilt Attic Windows x86_64 binary from GitHub
  Releases, verifies its SHA-256 checksum, and installs it under ATTIC_HOME.

  Default ATTIC_HOME:

      C:\Users\<user>\.attic

  This does NOT require:
      Rust
      Cargo
      MSVC Build Tools
      administrator privileges

.PARAMETER Version
  Optional release tag such as v0.1.3.

  Default:
      latest

.PARAMETER SkipModels
  Do not download the embedding models now. Attic then downloads them in
  the background the first time it starts.
#>

param(
    [string]$Version = "latest",
    [switch]$SkipModels
)

$ErrorActionPreference = "Stop"

$Repo = "aman-5/attic"


function Fail {
    param([string]$Message)

    Write-Error "ERROR: $Message"
    exit 1
}


# -----------------------------------------------------------------------------
# 1. Resolve ATTIC_HOME
# -----------------------------------------------------------------------------

if (Test-Path Env:ATTIC_HOME) {

    if ([string]::IsNullOrWhiteSpace($env:ATTIC_HOME)) {
        Fail "ATTIC_HOME is set but empty. Remove it or provide a valid directory."
    }

    $AtticHome = $env:ATTIC_HOME

} else {

    if ([string]::IsNullOrWhiteSpace($HOME)) {
        Fail "Could not determine the user home directory. Set ATTIC_HOME explicitly."
    }

    $AtticHome = Join-Path $HOME ".attic"
}


try {
    New-Item `
        -ItemType Directory `
        -Path $AtticHome `
        -Force `
        | Out-Null
} catch {
    Fail "Could not create Attic home directory '$AtticHome': $_"
}


Write-Host "Attic home:"
Write-Host "  $AtticHome"
Write-Host ""


# -----------------------------------------------------------------------------
# 2. Detect Windows architecture
# -----------------------------------------------------------------------------

$Architecture =
    [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture


if ($Architecture -ne [System.Runtime.InteropServices.Architecture]::X64) {

    Fail "No prebuilt Attic binary currently exists for Windows/$Architecture."
}


$Target = "x86_64-pc-windows-msvc"

Write-Host "Detected platform:"
Write-Host "  Windows/x64 -> $Target"
Write-Host ""


# -----------------------------------------------------------------------------
# 3. Resolve GitHub release
# -----------------------------------------------------------------------------

if ($Version -eq "latest") {

    # Avoid api.github.com here: unauthenticated REST requests are rate-limited.
    # The normal GitHub URL redirects to /releases/tag/<tag> and requires no token.
    $LatestUrl = "https://github.com/$Repo/releases/latest"

    try {
        $Response = Invoke-WebRequest `
            -Uri $LatestUrl `
            -UseBasicParsing `
            -ErrorAction Stop

        $FinalUrl = $null

        # Windows PowerShell 5.1
        if ($Response.BaseResponse -and $Response.BaseResponse.ResponseUri) {
            $FinalUrl = $Response.BaseResponse.ResponseUri.AbsoluteUri
        }
        # PowerShell 7+
        elseif (
            $Response.BaseResponse -and
            $Response.BaseResponse.RequestMessage -and
            $Response.BaseResponse.RequestMessage.RequestUri
        ) {
            $FinalUrl = $Response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
        }

        if ([string]::IsNullOrWhiteSpace($FinalUrl)) {
            Fail "GitHub resolved the latest release, but the final release URL could not be determined."
        }

    } catch {
        Fail "Could not reach GitHub to resolve the latest Attic release: $_"
    }

    if ($FinalUrl -notmatch '/releases/tag/(v[^/?#]+)') {
        Fail "Could not determine the latest Attic release tag from '$FinalUrl'."
    }

    $Tag = $Matches[1]

} else {

    $Tag = $Version
}


if ([string]::IsNullOrWhiteSpace($Tag)) {
    Fail "Could not determine the Attic release tag."
}


Write-Host "Release:"
Write-Host "  $Tag"
Write-Host ""


# -----------------------------------------------------------------------------
# 4. Determine artifact names
# -----------------------------------------------------------------------------

$Name =
    "attic-$Tag-$Target"

$Archive =
    "$Name.zip"

$Checksum =
    "$Archive.sha256"

$BaseUrl =
    "https://github.com/$Repo/releases/download/$Tag"


# -----------------------------------------------------------------------------
# 5. Create temporary download directory
# -----------------------------------------------------------------------------

$WorkDir =
    Join-Path `
        $env:TEMP `
        ("attic-setup-" + [Guid]::NewGuid().ToString("N"))


New-Item `
    -ItemType Directory `
    -Path $WorkDir `
    -Force `
    | Out-Null


try {

    $ArchivePath =
        Join-Path $WorkDir $Archive

    $ChecksumPath =
        Join-Path $WorkDir $Checksum


    # -------------------------------------------------------------------------
    # 6. Download archive
    # -------------------------------------------------------------------------

    Write-Host "Downloading:"
    Write-Host "  $Archive"

    try {

        Invoke-WebRequest `
            -Uri "$BaseUrl/$Archive" `
            -OutFile $ArchivePath `
            -UseBasicParsing

    } catch {

        Fail "Could not download $BaseUrl/$Archive"
    }


    # -------------------------------------------------------------------------
    # 7. Download checksum
    # -------------------------------------------------------------------------

    try {

        Invoke-WebRequest `
            -Uri "$BaseUrl/$Checksum" `
            -OutFile $ChecksumPath `
            -UseBasicParsing

    } catch {

        Fail "Could not download checksum $BaseUrl/$Checksum. Refusing to install an unverified binary."
    }


    # -------------------------------------------------------------------------
    # 8. Verify SHA-256
    # -------------------------------------------------------------------------

    $Expected =
        (
            Get-Content $ChecksumPath |
            Select-Object -First 1
        ) -split '\s+' |
        Select-Object -First 1


    if ([string]::IsNullOrWhiteSpace($Expected) -or $Expected -notmatch '^[0-9A-Fa-f]{64}$') {
        Fail "Downloaded checksum file is invalid (expected a 64-character SHA-256 hex digest)."
    }


    $Actual =
        (
            Get-FileHash `
                -Path $ArchivePath `
                -Algorithm SHA256
        ).Hash


    if ($Expected.ToLowerInvariant() -ne $Actual.ToLowerInvariant()) {

        Fail "Checksum verification FAILED for $Archive. Expected $Expected but got $Actual."
    }


    Write-Host "Checksum OK"
    Write-Host ""


    # -------------------------------------------------------------------------
    # 9. Extract
    # -------------------------------------------------------------------------

    Expand-Archive `
        -Path $ArchivePath `
        -DestinationPath $WorkDir `
        -Force


    $SourceExe =
        Join-Path `
            (Join-Path $WorkDir $Name) `
            "attic-server.exe"


    if (-not (Test-Path $SourceExe)) {

        Fail "Release archive does not contain attic-server.exe at the expected location."
    }


    # -------------------------------------------------------------------------
    # 10. Install/update
    # -------------------------------------------------------------------------

    $BinPath =
        Join-Path $AtticHome "attic-server.exe"


    try {

        Copy-Item `
            -Path $SourceExe `
            -Destination $BinPath `
            -Force

    } catch {

        Fail "Could not install Attic to '$BinPath'. If Attic is currently running, stop the MCP server and run setup again."
    }


    # Runtime libraries shipped beside the exe (DirectML.dll for the GPU
    # backend). Windows loads them from the exe's folder first.
    $Libraries =
        Get-ChildItem `
            -Path (Join-Path $WorkDir $Name) `
            -Filter "*.dll" `
            -File `
            -ErrorAction SilentlyContinue

    foreach ($Library in $Libraries) {

        try {

            Copy-Item `
                -Path $Library.FullName `
                -Destination (Join-Path $AtticHome $Library.Name) `
                -Force

        } catch {

            Fail "Could not install $($Library.Name) to '$AtticHome'. If Attic is currently running, stop the MCP server and run setup again."
        }
    }


    Write-Host "Attic installed successfully:"
    Write-Host "  $BinPath"
    foreach ($Library in $Libraries) {
        Write-Host "  $(Join-Path $AtticHome $Library.Name)"
    }
    Write-Host ""


    # -------------------------------------------------------------------------
    # 10b. Download embedding models now (GPU model first when eligible), so
    #      the first session starts on the right device. Never fails setup:
    #      Attic retries the download itself on first start.
    # -------------------------------------------------------------------------

    if (-not $SkipModels) {
        Write-Host "Downloading embedding models (about 1.2 GB per model, two on a GPU machine; download leftovers are removed afterwards; run with -SkipModels to defer)..."
        & $BinPath setup-models
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Model download did not complete (exit $LASTEXITCODE). Attic will download the models automatically when it first starts."
        }
        Write-Host ""
    }


    # -------------------------------------------------------------------------
    # 11. Print MCP configuration
    # -------------------------------------------------------------------------

    $BinPathJson =
        $BinPath.Replace('\', '\\')


    Write-Host "Add Attic to your AI client's MCP configuration:"
    Write-Host ""

    Write-Host "{"
    Write-Host '  "mcpServers": {'
    Write-Host '    "attic": {'
    Write-Host "      `"command`": `"$BinPathJson`","
    Write-Host '      "args": []'
    Write-Host "    }"
    Write-Host "  }"
    Write-Host "}"

    Write-Host ""
    Write-Host "Attic uses MCP over stdio."
    Write-Host ""
    Write-Host "No repository configuration is required in the MCP JSON."
    Write-Host ""
    Write-Host "After your AI client connects to Attic, tell it:"
    Write-Host ""
    Write-Host '  Configure Attic to index these repositories:'
    Write-Host '  C:\path\repo-a'
    Write-Host '  D:\path\repo-b'
    Write-Host '  E:\path\repo-c'
    Write-Host ""
    Write-Host "Attic will persist the workspace configuration under:"
    Write-Host "  $AtticHome"
    Write-Host ""
    Write-Host "Running setup.ps1 again updates the installed Attic binary"
    Write-Host "to the latest published release."

} finally {

    Remove-Item `
        -Path $WorkDir `
        -Recurse `
        -Force `
        -ErrorAction SilentlyContinue
}