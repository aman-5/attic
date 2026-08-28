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
#>

param(
    [string]$Version = "latest"
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

    $ApiUrl =
        "https://api.github.com/repos/$Repo/releases/latest"

    try {

        $Release =
            Invoke-RestMethod `
                -Uri $ApiUrl `
                -Headers @{
                    "User-Agent" = "attic-setup"
                    "Accept"     = "application/vnd.github+json"
                }

    } catch {

        $StatusCode = $null

        if ($_.Exception.Response) {
            try {
                $StatusCode =
                    [int]$_.Exception.Response.StatusCode
            } catch {
                $StatusCode = $null
            }
        }

        if ($StatusCode -eq 404) {
            Fail "No published Attic release exists yet."
        }

        Fail "Could not reach GitHub to resolve the latest Attic release: $_"
    }


    $Tag = $Release.tag_name

    if ([string]::IsNullOrWhiteSpace($Tag)) {
        Fail "GitHub returned a release without a tag."
    }

} else {

    $Tag = $Version
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


    if ([string]::IsNullOrWhiteSpace($Expected)) {
        Fail "Downloaded checksum file is invalid."
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


    Write-Host "Attic installed successfully:"
    Write-Host "  $BinPath"
    Write-Host ""


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