use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

type XtaskResult<T> = Result<T, String>;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> XtaskResult<()> {
    let mut args = env::args_os();
    let _binary = args.next();
    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.to_string_lossy().as_ref() {
        "check" => {
            let rest = args.collect::<Vec<_>>();
            if has_help_flag(&rest) {
                print_check_help();
                Ok(())
            } else if rest.is_empty() {
                run_check()
            } else {
                Err(format!(
                    "unknown argument for `check`: {}",
                    rest[0].to_string_lossy()
                ))
            }
        }
        "install" => {
            let options = parse_install_options(args.collect())?;
            if options.help {
                print_install_help();
                Ok(())
            } else {
                run_install(&options)
            }
        }
        "all" => {
            let options = parse_install_options(args.collect())?;
            if options.help {
                print_all_help();
                Ok(())
            } else if options.skip_build {
                Err("`all` always builds; use `install --skip-build` instead".into())
            } else {
                run_cargo_step("fmt", &["fmt", "--all"])?;
                run_check()?;
                run_install(&options)
            }
        }
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

fn print_help() {
    println!(
        "Attic workspace tasks\n\nUSAGE:\n    cargo xtask <COMMAND>\n\nCOMMANDS:\n    check      Run fmt, clippy, and tests\n    install    Build and install attic-server, then download the models\n    all        Format, check, then install\n\nOn Windows the MSVC target (DirectML GPU) is used unless CARGO_BUILD_TARGET is set.\n\nRun `cargo xtask <COMMAND> --help` for command-specific options."
    );
}

fn print_check_help() {
    println!(
        "Run pre-commit checks in order, stopping at the first failure.\n\nUSAGE:\n    cargo xtask check\n\nSTEPS:\n    cargo fmt --all --check\n    cargo clippy --workspace --all-targets -- -D warnings\n    cargo test --workspace"
    );
}

fn print_install_help() {
    println!(
        "Build and install the local attic-server binary, then run\n`attic-server setup-models` (downloads missing models, removes duplicate model files).\n\nUSAGE:\n    cargo xtask install [--skip-build] [--skip-models]\n\nOPTIONS:\n        --skip-build     Install the existing release artifact without rebuilding\n        --skip-models    Do not run setup-models after installing\n    -h, --help           Print help"
    );
}

fn print_all_help() {
    println!(
        "Format, check, then install, stopping at the first failure.\n\nUSAGE:\n    cargo xtask all [--skip-models]\n\nSTEPS:\n    cargo fmt --all\n    cargo xtask check\n    cargo xtask install"
    );
}

fn has_help_flag(args: &[OsString]) -> bool {
    args.iter()
        .any(|arg| arg == OsStr::new("-h") || arg == OsStr::new("--help"))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct InstallOptions {
    help: bool,
    skip_build: bool,
    skip_models: bool,
}

fn parse_install_options(args: Vec<OsString>) -> XtaskResult<InstallOptions> {
    let mut options = InstallOptions::default();

    for arg in args {
        match arg.to_string_lossy().as_ref() {
            "-h" | "--help" => options.help = true,
            "--skip-build" => options.skip_build = true,
            "--skip-models" => options.skip_models = true,
            other => return Err(format!("unknown argument for `install`: {other}")),
        }
    }

    Ok(options)
}

fn run_check() -> XtaskResult<()> {
    let steps: &[(&str, &[&str])] = &[
        ("fmt", &["fmt", "--all", "--check"]),
        (
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("test", &["test", "--workspace"]),
    ];

    for (name, args) in steps {
        run_cargo_step(name, args)?;
    }

    Ok(())
}

fn run_install(options: &InstallOptions) -> XtaskResult<()> {
    // The artifact path comes from cargo itself, so `[build] target` or
    // `target-dir` set in any Cargo config file is honoured. Guessing it from
    // environment variables alone installed a stale binary whenever a user
    // config forced a different target.
    let source_binary = if options.skip_build {
        newest_existing_release_binary()?
    } else {
        build_release_binary()?
    };
    let release_dir = source_binary
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("binary has no parent: {}", source_binary.display()))?;

    stop_attic_processes()?;
    thread::sleep(Duration::from_secs(2));

    let install_dir = install_dir()?;
    fs::create_dir_all(&install_dir).map_err(|error| {
        format!(
            "failed to create install directory {}: {error}",
            install_dir.display()
        )
    })?;

    let installed_binary = install_dir.join(installed_binary_file_name());
    copy_binary_with_retry(&source_binary, &installed_binary)?;
    set_executable_permissions(&installed_binary)?;

    let copied_libs = copy_runtime_libraries(&release_dir, &install_dir)?;
    let size = fs::metadata(&installed_binary)
        .map_err(|error| {
            format!(
                "failed to inspect installed binary {}: {error}",
                installed_binary.display()
            )
        })?
        .len();

    println!("Installed: {}", installed_binary.display());
    println!("Size: {size} bytes");
    if copied_libs.is_empty() {
        println!("Copied libraries: none");
    } else {
        println!("Copied libraries ({}):", copied_libs.len());
        for library in copied_libs {
            println!("  {}", library.display());
        }
    }

    if !options.skip_models {
        run_setup_models(&installed_binary);
    }

    Ok(())
}

/// Download missing models and remove duplicate model files. A failure is a
/// warning, not an install error: the server retries the download itself.
fn run_setup_models(installed_binary: &Path) {
    println!("Running `{} setup-models`...", installed_binary.display());
    match Command::new(installed_binary)
        .arg("setup-models")
        .stdin(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "warning: setup-models exited with {status}; Attic downloads missing models on first start"
        ),
        Err(error) => eprintln!("warning: could not run setup-models: {error}"),
    }
}

fn run_cargo_step(step_name: &str, args: &[&str]) -> XtaskResult<()> {
    println!("Running `{}`...", command_line(cargo(), args));
    let status = cargo_command()
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to start `{step_name}`: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("step `{step_name}` failed with {status}"))
    }
}

fn command_line(program: OsString, args: &[&str]) -> String {
    let mut parts = vec![program.to_string_lossy().into_owned()];
    parts.extend(args.iter().map(|arg| (*arg).to_owned()));
    parts.join(" ")
}

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// A cargo command for the workspace. On Windows it targets MSVC unless
/// `CARGO_BUILD_TARGET` is already set: only the MSVC build has the DirectML
/// GPU backend, and a user `~/.cargo/config.toml` may default to GNU.
fn cargo_command() -> Command {
    let mut command = Command::new(cargo());
    command.current_dir(workspace_root());
    if let Some(target) = default_build_target(env::var_os("CARGO_BUILD_TARGET"), cfg!(windows)) {
        command.env("CARGO_BUILD_TARGET", target);
    }
    command
}

fn default_build_target(current: Option<OsString>, windows: bool) -> Option<&'static str> {
    let unset = current.is_none_or(|value| value.is_empty());
    (windows && unset).then_some("x86_64-pc-windows-msvc")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly under the workspace root")
        .to_path_buf()
}

/// Build the release binary and return the executable path cargo reports.
///
/// On Apple Silicon the Metal GPU backend is enabled automatically, matching
/// the release packages (`tools/package.sh`); Windows gets DirectML from the
/// MSVC target, so every platform keeps the same generic command.
fn build_release_binary() -> XtaskResult<PathBuf> {
    let mut args = vec![
        "build",
        "--release",
        "-p",
        "attic-server",
        "--message-format=json-render-diagnostics",
    ];
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        args.extend(["--features", "candle-metal"]);
    }
    println!("Running `{}`...", command_line(cargo(), &args));
    let output = cargo_command()
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("failed to start `release build`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "step `release build` failed with {}",
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| line.contains("\"reason\":\"compiler-artifact\""))
        .filter_map(|line| json_string_field(line, "executable"))
        .map(PathBuf::from)
        .rfind(|path| path.file_name().and_then(OsStr::to_str) == Some(binary_file_name()))
        .ok_or_else(|| "cargo reported no `attic` executable for attic-server".to_string())
}

/// Minimal JSON string-field reader for cargo's one-object-per-line output.
fn json_string_field(line: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let start = line.find(&pattern)? + pattern.len();
    let mut out = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// `--skip-build`: the most recently built release binary under the target
/// directory, whichever target triple produced it.
fn newest_existing_release_binary() -> XtaskResult<PathBuf> {
    let target_dir = match env::var_os("CARGO_TARGET_DIR") {
        Some(path) if !path.is_empty() => absolutize_workspace_path(path),
        _ => workspace_root().join("target"),
    };
    let mut candidates = vec![target_dir.join("release").join(binary_file_name())];
    if let Ok(entries) = fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("release").join(binary_file_name()));
        }
    }
    candidates
        .into_iter()
        .filter_map(|path| {
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| {
            println!("Using existing build: {}", path.display());
            path
        })
        .ok_or_else(|| {
            format!(
                "no release build of {} found under {}; run without --skip-build",
                binary_file_name(),
                target_dir.display()
            )
        })
}

fn absolutize_workspace_path(path: OsString) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workspace_root().join(path)
    }
}

fn binary_file_name() -> &'static str {
    if cfg!(windows) { "attic.exe" } else { "attic" }
}

fn installed_binary_file_name() -> &'static str {
    if cfg!(windows) {
        "attic-server.exe"
    } else {
        "attic-server"
    }
}

fn stop_attic_processes() -> XtaskResult<()> {
    if cfg!(windows) {
        stop_windows_process("attic-server.exe")?;
        stop_windows_process("attic.exe")?;
    } else {
        stop_unix_process("attic-server")?;
        stop_unix_process("attic")?;
    }
    Ok(())
}

fn stop_windows_process(image_name: &str) -> XtaskResult<()> {
    let output = Command::new("taskkill")
        .args(["/F", "/IM", image_name])
        .output()
        .map_err(|error| format!("failed to start taskkill for {image_name}: {error}"))?;

    if output.status.success() || output.status.code() == Some(128) || process_not_found(&output) {
        Ok(())
    } else {
        Err(format!(
            "taskkill failed for {image_name} with {}:\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn process_not_found(output: &std::process::Output) -> bool {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    combined.contains("not found")
}

fn stop_unix_process(process_name: &str) -> XtaskResult<()> {
    let status = Command::new("pkill")
        .args(["-x", process_name])
        .status()
        .map_err(|error| format!("failed to start pkill for {process_name}: {error}"))?;

    if status.success() || status_code(&status) == Some(1) {
        Ok(())
    } else {
        Err(format!("pkill failed for {process_name} with {status}"))
    }
}

fn status_code(status: &ExitStatus) -> Option<i32> {
    status.code()
}

fn install_dir() -> XtaskResult<PathBuf> {
    if let Some(path) = env::var_os("ATTIC_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(home_var)
        .filter(|path| !path.is_empty())
        .map(|home| PathBuf::from(home).join(".attic"))
        .ok_or_else(|| format!("cannot determine install directory: {home_var} is not set"))
}

fn copy_binary_with_retry(source: &Path, destination: &Path) -> XtaskResult<()> {
    let attempts = 5;
    for attempt in 1..=attempts {
        match replace_file(source, destination) {
            Ok(_) => return Ok(()),
            Err(error) if attempt < attempts => {
                eprintln!(
                    "copy attempt {attempt}/{attempts} failed for {}: {error}; retrying...",
                    destination.display()
                );
                thread::sleep(Duration::from_millis(500));
            }
            Err(error) => {
                return Err(format!(
                    "failed to copy {} to {}: {error}",
                    source.display(),
                    destination.display()
                ));
            }
        }
    }

    Ok(())
}

/// Copy `source` to a temporary sibling of `destination`, then rename it into
/// place. Overwriting in place keeps the old inode, and on macOS the kernel's
/// cached code signature for it no longer matches — the reinstalled binary is
/// then killed on launch ("Killed: 9"). A rename gives it a fresh inode.
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut temp_name = destination
        .file_name()
        .map(OsString::from)
        .unwrap_or_default();
    temp_name.push(".xtask-new");
    let temp = destination.with_file_name(temp_name);
    fs::copy(source, &temp)?;
    fs::rename(&temp, destination).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> XtaskResult<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to chmod 755 {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_executable_permissions(_path: &Path) -> XtaskResult<()> {
    Ok(())
}

fn copy_runtime_libraries(release_dir: &Path, install_dir: &Path) -> XtaskResult<Vec<PathBuf>> {
    let mut copied = Vec::new();
    for entry in fs::read_dir(release_dir).map_err(|error| {
        format!(
            "failed to read release directory {}: {error}",
            release_dir.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read an entry from {}: {error}",
                release_dir.display()
            )
        })?;
        let path = entry.path();
        if path.is_file() && is_runtime_library(&path) {
            let Some(file_name) = path.file_name() else {
                continue;
            };
            let destination = install_dir.join(file_name);
            replace_file(&path, &destination).map_err(|error| {
                format!(
                    "failed to copy {} to {}: {error}",
                    path.display(),
                    destination.display()
                )
            })?;
            copied.push(destination);
        }
    }

    copied.sort();
    Ok(copied)
}

fn is_runtime_library(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };

    if cfg!(windows) {
        file_name.ends_with(".dll")
    } else if cfg!(target_os = "macos") {
        file_name.ends_with(".dylib")
    } else {
        file_name.contains(".so")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn install_options_parse_and_reject_unknown() {
        let options = parse_install_options(args(&["--skip-build", "--skip-models"])).unwrap();
        assert!(options.skip_build && options.skip_models && !options.help);
        assert_eq!(
            parse_install_options(args(&[])).unwrap(),
            InstallOptions::default()
        );
        assert!(parse_install_options(args(&["--bogus"])).is_err());
    }

    #[test]
    fn msvc_is_the_windows_default_unless_a_target_is_set() {
        assert_eq!(
            default_build_target(None, true),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            default_build_target(Some(OsString::new()), true),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            default_build_target(Some("x86_64-pc-windows-gnu".into()), true),
            None
        );
        assert_eq!(default_build_target(None, false), None);
    }
}
