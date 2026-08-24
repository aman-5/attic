# Windows Local Setup — GNU Toolchain (no MSVC)

If you are on Windows without Visual Studio / MSVC Build Tools, use the GNU toolchain variant.

## Install MinGW (no admin required — via Scoop)

```powershell
# Install Scoop (user-local, no admin)
Set-ExecutionPolicy RemoteSigned -Scope CurrentUser
irm get.scoop.sh | iex

# Install MinGW
scoop install mingw
```

## Add GNU Rust target and toolchain

```powershell
rustup target add x86_64-pc-windows-gnu
rustup toolchain install 1.98.0-x86_64-pc-windows-gnu --force-non-host
```

## Configure local Cargo override (NOT committed to the repo)

Create `.cargo/config.toml` in your local clone **or** add to your global `%USERPROFILE%\.cargo\config.toml`:

```toml
[build]
target = "x86_64-pc-windows-gnu"

[target.x86_64-pc-windows-gnu]
# Replace with your actual MinGW gcc path (check: scoop prefix mingw)
linker = "C:\\Users\\<you>\\scoop\\apps\\mingw\\current\\bin\\gcc.exe"
```

> **Do not commit this file.** The repo `.cargo/config.toml` is intentionally kept portable and does not contain machine-specific paths.
