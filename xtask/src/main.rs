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
                run_install(options.skip_build)
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
        "Attic workspace tasks\n\nUSAGE:\n    cargo xtask <COMMAND>\n\nCOMMANDS:\n    check      Run fmt, clippy, and tests\n    install    Build and install the local attic-server binary\n\nRun `cargo xtask <COMMAND> --help` for command-specific options."
    );
}

fn print_check_help() {
    println!(
        "Run pre-commit checks in order, stopping at the first failure.\n\nUSAGE:\n    cargo xtask check\n\nSTEPS:\n    cargo fmt --all --check\n    cargo clippy --workspace --all-targets -- -D warnings\n    cargo test --workspace"
    );
}

fn print_install_help() {
    println!(
        "Build and install the local attic-server binary.\n\nUSAGE:\n    cargo xtask install [--skip-build]\n\nOPTIONS:\n        --skip-build    Install the existing release artifact without rebuilding\n    -h, --help          Print help"
    );
}

fn has_help_flag(args: &[OsString]) -> bool {
    args.iter()
        .any(|arg| arg == OsStr::new("-h") || arg == OsStr::new("--help"))
}

struct InstallOptions {
    help: bool,
    skip_build: bool,
}

fn parse_install_options(args: Vec<OsString>) -> XtaskResult<InstallOptions> {
    let mut options = InstallOptions {
        help: false,
        skip_build: false,
    };

    for arg in args {
        match arg.to_string_lossy().as_ref() {
            "-h" | "--help" => options.help = true,
            "--skip-build" => options.skip_build = true,
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

fn run_install(skip_build: bool) -> XtaskResult<()> {
    if !skip_build {
        run_cargo_step(
            "release build",
            &["build", "--release", "-p", "attic-server"],
        )?;
    }

    let release_dir = release_dir()?;
    let source_binary = release_dir.join(binary_file_name());
    if !source_binary.is_file() {
        return Err(format!(
            "expected release binary was not found: {}",
            source_binary.display()
        ));
    }

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

    Ok(())
}

fn run_cargo_step(step_name: &str, args: &[&str]) -> XtaskResult<()> {
    println!("Running `{}`...", command_line(cargo(), args));
    let status = Command::new(cargo())
        .args(args)
        .current_dir(workspace_root())
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly under the workspace root")
        .to_path_buf()
}

fn release_dir() -> XtaskResult<PathBuf> {
    let target_dir = match env::var_os("CARGO_TARGET_DIR") {
        Some(path) if !path.is_empty() => absolutize_workspace_path(path),
        _ => workspace_root().join("target"),
    };

    let release_dir = match env::var_os("CARGO_BUILD_TARGET") {
        Some(target) if !target.is_empty() => target_dir.join(target).join("release"),
        _ => target_dir.join("release"),
    };

    Ok(release_dir)
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
        match fs::copy(source, destination) {
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
            fs::copy(&path, &destination).map_err(|error| {
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
