# Windows Local Development Setup

This document covers Windows-specific setup for developing and building the Attic project.

Visual Studio IDE and VS Code are **not** required. Any editor or terminal works.

---

## Recommended setup (MSVC toolchain)

### Prerequisites

1. **Rust toolchain** — install via [rustup](https://rustup.rs/).  
   The MSVC host (`x86_64-pc-windows-msvc`) is the default on Windows and is recommended.

2. **Microsoft C++ Build Tools** — required by the MSVC linker.  
   Download "Build Tools for Visual Studio" from  
   <https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio>  
   During installation select:
   - **C++ build tools** workload
   - **Windows SDK** (included by default in the C++ workload)

   Visual Studio IDE is **not** required — the standalone Build Tools are sufficient.

3. No other tools are needed for Bootstrap or Phase 0.

### Build

```powershell
cargo build
cargo test
```

---

## Optional: GNU toolchain (no MSVC)

If you cannot install the MSVC Build Tools, the GNU/MinGW toolchain is an alternative.
This requires a **local, untracked** Cargo configuration — do not commit it.

### Install MinGW (no admin required — via Scoop)

```powershell
# Install Scoop (user-local, no admin)
Set-ExecutionPolicy RemoteSigned -Scope CurrentUser
irm get.scoop.sh | iex

# Install MinGW
scoop install mingw
```

### Add the GNU Rust target

```powershell
rustup target add x86_64-pc-windows-gnu
```

### Configure a local Cargo override (NOT committed)

Add to your global `%USERPROFILE%\.cargo\config.toml` (create if absent):

```toml
[build]
target = "x86_64-pc-windows-gnu"

[target.x86_64-pc-windows-gnu]
# Replace with your actual MinGW gcc path (check: scoop prefix mingw)
linker = "C:\\Users\\<you>\\scoop\\apps\\mingw\\current\\bin\\gcc.exe"
```

> **Do not commit this file.** The repo `.cargo/config.toml` is intentionally kept
> portable and contains no machine-specific paths or platform targets.

### Build (GNU)

```powershell
cargo build --target x86_64-pc-windows-gnu
```
