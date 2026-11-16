use attic_core::config::PlanningConfig;
use attic_spec::{
    Result, SpecError, boundary, git, hooks,
    service::{ApprovalPrompt, SpecService, SystemClock},
    store::SpecStore,
    verifier,
};
use serde_json::Value;
use std::{
    ffi::OsString,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

struct Args {
    values: Vec<String>,
}
impl Args {
    fn flag(&self, name: &str) -> bool {
        self.values.iter().any(|s| s == name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .windows(2)
            .find(|p| p[0] == name)
            .map(|p| p[1].as_str())
    }
    fn positional(&self, n: usize) -> Option<&str> {
        self.values
            .get(n)
            .map(String::as_str)
            .filter(|s| !s.starts_with('-'))
    }
}
pub(crate) fn run(args: Vec<OsString>) -> i32 {
    let args = Args {
        values: match args
            .into_iter()
            .map(|s| s.into_string())
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(_) => {
                eprintln!("ERR_INVALID_ARGUMENT: non-UTF8 argument");
                return 2;
            }
        },
    };
    match execute(&args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            match error.code.as_str() {
                "ERR_APPROVAL_REQUIRES_TTY" => 3,
                "ERR_APPROVAL_STALE" => 4,
                "ERR_APPROVAL_CANCELLED" => 5,
                "ERR_STATE_CHANGED_DURING_APPROVAL" => 6,
                "ERR_COMMIT_DENIED" => 1,
                _ => 2,
            }
        }
    }
}
fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str> {
    value.ok_or_else(|| SpecError::new("ERR_INVALID_ARGUMENT", format!("{name} required")))
}
fn input() -> Result<Value> {
    let mut text = String::new();
    std::io::stdin().take(2_097_153).read_to_string(&mut text)?;
    if text.len() > 2_097_152 {
        return Err(SpecError::new(
            "ERR_INVALID_ARGUMENT",
            "hook input too large",
        ));
    }
    serde_json::from_str(&text).map_err(|e| SpecError::new("ERR_INVALID_ARGUMENT", e.to_string()))
}
fn roots(config: &Path, cfg: &PlanningConfig) -> Result<Vec<PathBuf>> {
    if std::env::var_os("ATTIC_CONFIG").is_some()
        && std::env::var_os("ATTIC_WORKSPACE_ROOT").is_some()
    {
        return Err(SpecError::new("ERR_CONFIG", "ambiguous workspace sources"));
    }
    let file = std::env::var_os("ATTIC_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.to_path_buf());
    let roots = if file.exists() {
        crate::parse_repositories_config(&std::fs::read_to_string(file)?)
            .map_err(|e| SpecError::new("ERR_CONFIG", e))?
    } else {
        std::env::var_os("ATTIC_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .into_iter()
            .collect()
    };
    roots
        .into_iter()
        .map(|r| r.canonicalize().map_err(Into::into))
        .collect::<Result<Vec<_>>>()
        .map(|r| {
            r.into_iter()
                .filter(|r| {
                    cfg.roots.is_empty()
                        || cfg.roots.iter().any(|p| {
                            Path::new(p)
                                .canonicalize()
                                .is_ok_and(|p| verifier::identity(&p) == verifier::identity(r))
                        })
                })
                .collect()
        })
}
fn service_read(catalog: &Path, cfg: &PlanningConfig) -> Result<Option<SpecService>> {
    Ok(
        SpecStore::open_read_only(&SpecStore::path_for_catalog(catalog))?.map(|store| {
            SpecService {
                store,
                config: cfg.clone(),
                data_dir: catalog.parent().unwrap_or(Path::new(".")).into(),
                packs: Arc::new(tokio::sync::Semaphore::new(cfg.packs.max_concurrent)),
                clock: Arc::new(SystemClock),
            }
        }),
    )
}
fn execute(args: &Args) -> Result<()> {
    let command = args.positional(0).unwrap_or("status");
    if args.flag("--help") {
        println!(
            "attic spec status|approve|abandon|check-write|check-commit|baseline-start|hooks|skill\n--root PATH --session ID --host claude\nEnable/disable EGM: attic egm enable|disable|status\nApproval is human-only. Hooks are opt-in and bypassable."
        );
        return Ok(());
    }
    if command == "skill" && args.positional(1) == Some("print") {
        println!("{}", hooks::GUIDANCE);
        return Ok(());
    }
    let paths = attic_core::AtticPaths::resolve_read_only()
        .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?;
    let catalog = paths.db_path();
    let structured = std::env::var_os("ATTIC_DB_PATH").is_none();
    let runtime = attic_core::sibling_in(catalog, "attic.toml", structured);
    let config = attic_core::sibling_in(catalog, "config.toml", structured);
    let cfg = if runtime.exists() {
        attic_core::AtticConfig::parse_str(&std::fs::read_to_string(runtime)?)
            .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?
            .planning
    } else {
        PlanningConfig::default()
    };
    if !cfg.enabled {
        if matches!(command, "check-write" | "check-commit" | "baseline-start") {
            return Ok(());
        }
        return Err(SpecError::new(
            "ERR_PLANNING_DISABLED",
            "EGM planning is disabled; run attic egm enable --root PATH, then restart your Attic MCP server",
        ));
    }
    if command == "hooks" {
        match args.positional(1) {
            Some("print") => {
                if args.value("--host") == Some("claude") {
                    println!("{}", hooks::host_config(cfg.hooks.fail_closed));
                } else {
                    print!("{}", hooks::git_script(cfg.hooks.fail_closed));
                }
                return Ok(());
            }
            Some("install") if args.flag("--git") => {
                println!(
                    "{}",
                    hooks::install(
                        Path::new(required(args.value("--root"), "--root")?),
                        args.flag("--chain"),
                        cfg.hooks.fail_closed
                    )?
                );
                return Ok(());
            }
            Some("uninstall") if args.flag("--git") => {
                hooks::uninstall(Path::new(required(args.value("--root"), "--root")?))?;
                return Ok(());
            }
            _ => {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    "only explicit --git install/uninstall is supported; print host settings for manual integration",
                ));
            }
        }
    }
    let active = roots(&config, &cfg)?;
    let cwd = std::env::current_dir()?;
    if matches!(command, "check-write" | "check-commit" | "baseline-start") {
        let result = check(args, command, &cfg, &active, &cwd, catalog);
        return match result {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.code.as_str(), "ERR_WRITE_DENIED" | "ERR_COMMIT_DENIED") => Err(e),
            Err(error) => {
                eprintln!("spec checker internal error: {error}");
                if cfg.hooks.fail_closed && cfg.fast_path.enforce {
                    Err(SpecError::new(
                        if command == "check-commit" {
                            "ERR_COMMIT_DENIED"
                        } else {
                            "ERR_WRITE_DENIED"
                        },
                        "checker failed closed",
                    ))
                } else {
                    Ok(())
                }
            }
        };
    }
    if command == "approve"
        && !(std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal())
    {
        return Err(SpecError::new(
            "ERR_APPROVAL_REQUIRES_TTY",
            "approval requires stdin/stdout/stderr terminals",
        ));
    }
    let root = args
        .value("--root")
        .map(|r| Path::new(r).canonicalize().map(|p| verifier::identity(&p)))
        .transpose()?;
    if command == "status" {
        let mut found = false;
        if let Some(service) = service_read(catalog, &cfg)? {
            for doc in service.store.documents()?.into_iter().filter(|d| {
                active
                    .iter()
                    .any(|r| verifier::identity(r) == d.owner_root_path)
                    && root.as_ref().is_none_or(|r| r == &d.owner_root_path)
                    && args.positional(1).is_none_or(|id| id == d.spec_id)
            }) {
                found = true;
                println!("{}", service.status(&doc)?);
            }
        }
        if args.positional(1).is_some() && !found {
            return Err(SpecError::new(
                "ERR_SPEC_NOT_FOUND",
                "spec not registered in selected active roots",
            ));
        }
        return Ok(());
    }
    let service = SpecService::open(catalog, cfg)?
        .ok_or_else(|| SpecError::new("ERR_PLANNING_DISABLED", "planning disabled"))?;
    let doc = service.resolve(required(args.positional(1), "spec_id")?, root.as_deref())?;
    if !active
        .iter()
        .any(|r| verifier::identity(r) == doc.owner_root_path)
    {
        return Err(SpecError::new(
            "ERR_ROOT_NOT_ACTIVE",
            "spec root not active",
        ));
    }
    match command {
        "approve" => {
            let approved = service.approve(&doc, &mut TerminalPrompt)?;
            println!(
                "Approved {} rev {}. Agent may begin.",
                approved.spec_id, approved.revision
            );
        }
        "abandon" => {
            let current = service.reconciled(&doc)?;
            let abandoned = service.store.abandon(
                &current,
                required(args.value("--reason"), "--reason")?,
                "CLI",
                service.clock.now()?,
            )?;
            service.invalidate_packs(&abandoned)?;
            println!("Abandoned {}", abandoned.spec_id);
        }
        _ => {
            return Err(SpecError::new(
                "ERR_INVALID_ARGUMENT",
                "unknown spec command",
            ));
        }
    }
    Ok(())
}
fn check(
    args: &Args,
    command: &str,
    cfg: &PlanningConfig,
    active: &[PathBuf],
    cwd: &Path,
    catalog: &Path,
) -> Result<()> {
    let host = if args.value("--host") == Some("claude") {
        Some(input()?)
    } else {
        None
    };
    let work = host
        .as_ref()
        .and_then(|v| v.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if matches!(command, "baseline-start" | "check-commit") {
                args.value("--root")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf())
            } else {
                cwd.to_path_buf()
            }
        })
        .canonicalize()?;
    let session = host
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(Value::as_str)
        .or_else(|| args.value("--session"));
    if command == "check-write"
        && let Some(host) = &host
    {
        let tool = required(host.get("tool_name").and_then(Value::as_str), "tool_name")?;
        if tool == "Bash" {
            let top = match git::toplevel(&work) {
                Ok(root) => root,
                Err(_) => return Ok(()),
            };
            let related = active.iter().any(|root| {
                git::toplevel(root).is_ok_and(|r| r == top)
                    || git::common_dir(root)
                        .ok()
                        .zip(git::common_dir(&top).ok())
                        .is_some_and(|(a, b)| a == b)
            });
            if !related {
                return Ok(());
            }
            let command = required(
                host.pointer("/tool_input/command").and_then(Value::as_str),
                "Bash command",
            )?;
            let blocked = regex::Regex::new(
                r"(?is)\battic(?:\.exe)?\s+spec\s+approve\b|\bgit\s+commit\b[^\r\n]*--no-verify\b",
            )
            .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?
            .is_match(command);
            if blocked && cfg.fast_path.enforce {
                return Err(SpecError::new(
                    "ERR_WRITE_DENIED",
                    "agent approval and --no-verify bypass are denied",
                ));
            }
            return Ok(());
        }
        if !matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
            return Ok(());
        }
    }
    let target = if command == "check-write" {
        let raw = if let Some(host) = &host {
            let field = if host["tool_name"] == "NotebookEdit" {
                "/tool_input/notebook_path"
            } else {
                "/tool_input/file_path"
            };
            required(host.pointer(field).and_then(Value::as_str), field)?
        } else {
            required(args.value("--path"), "--path")?
        };
        work.join(raw)
    } else {
        args.value("--root")
            .map(PathBuf::from)
            .unwrap_or_else(|| work.clone())
    };
    let target = lexical_absolute(&work.join(target))?;
    let target_root = (command == "check-write")
        .then(|| {
            active
                .iter()
                .filter(|r| attic_discovery::security::path_is_within(&target, r))
                .max_by_key(|r| r.components().count())
                .cloned()
        })
        .flatten();
    let top = match git::toplevel(target_root.as_deref().unwrap_or(&work)) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let mut roots = if let Some(root) = target_root {
        vec![root]
    } else {
        active
            .iter()
            .filter(|r| git::toplevel(r).is_ok_and(|p| p == top))
            .cloned()
            .collect::<Vec<_>>()
    };
    if roots.is_empty() {
        let common = git::common_dir(&top)?;
        if !active
            .iter()
            .any(|r| git::common_dir(r).is_ok_and(|p| p == common))
        {
            return Ok(());
        }
        eprintln!(
            "Related linked checkout is unregistered; no approval is inherited. Register this checkout for spec coverage."
        );
        roots.push(top.clone());
    }
    if command == "baseline-start" {
        let root = roots
            .iter()
            .filter(|r| attic_discovery::security::path_is_within(&target, r))
            .max_by_key(|r| r.components().count())
            .ok_or_else(|| SpecError::new("ERR_ROOT_NOT_ACTIVE", "baseline root not active"))?;
        if !active.contains(root) {
            return Err(SpecError::new(
                "ERR_ROOT_NOT_ACTIVE",
                "register checkout before baseline capture",
            ));
        }
        let service = SpecService::open(catalog, cfg.clone())?
            .ok_or_else(|| SpecError::new("ERR_PLANNING_DISABLED", "planning disabled"))?;
        boundary::capture_baseline(
            &service.store,
            root,
            required(session, "session_id/--session")?,
        )?;
        return Ok(());
    }
    let store = SpecStore::open_read_only(&SpecStore::path_for_catalog(catalog))?;
    let outcome = if command == "check-commit" {
        boundary::check_commit(store.as_ref(), &roots, &work, cfg)?
    } else {
        let root = roots
            .iter()
            .filter(|r| attic_discovery::security::path_is_within(&target, r))
            .max_by_key(|r| r.components().count());
        let Some(root) = root else { return Ok(()) };
        let relative = match target.strip_prefix(root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) if cfg!(windows) && attic_discovery::security::path_is_within(&target, root) => {
                target
                    .components()
                    .skip(root.components().count())
                    .collect()
            }
            Err(_) => {
                return Err(SpecError::new(
                    "ERR_PATH_OUTSIDE_ROOT",
                    "hook target outside root",
                ));
            }
        };
        let rel = relative.to_string_lossy().replace('\\', "/");
        let target = verifier::safe_path(root, &rel)?;
        if let Some(size) = host
            .as_ref()
            .and_then(|h| h.pointer("/tool_input/content"))
            .and_then(Value::as_str)
            .map(str::len)
            && rel.starts_with(&format!("{}/", cfg.designs_dir))
            && rel.ends_with(".md")
            && size > 1_048_576
            && cfg.fast_path.enforce
        {
            return Err(SpecError::new(
                "ERR_WRITE_DENIED",
                "spec authoring bootstrap is limited to 1 MiB",
            ));
        }
        boundary::check_write(store.as_ref(), root, &target, session, cfg)?
    };
    eprintln!("{}", outcome.message);
    if outcome.allowed {
        Ok(())
    } else {
        Err(SpecError::new(
            if command == "check-commit" {
                "ERR_COMMIT_DENIED"
            } else {
                "ERR_WRITE_DENIED"
            },
            outcome.message,
        ))
    }
}
struct TerminalPrompt;

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    return Err(SpecError::new(
                        "ERR_PATH_OUTSIDE_ROOT",
                        "path traverses above root",
                    ));
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

impl ApprovalPrompt for TerminalPrompt {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
    }
    fn confirm(
        &mut self,
        doc: &attic_spec::state::Document,
        run: &attic_spec::verifier::Run,
        code: &str,
    ) -> Result<bool> {
        println!(
            "{}: {}\npath: {}/{}\nrevision: {} run: {}\ndoc_sha256: {}\nfingerprint: {}\nchecked_at_us: {}",
            doc.spec_id,
            doc.title,
            doc.owner_root_path,
            doc.canonical_rel_path,
            doc.revision,
            run.run_id,
            run.doc_sha256,
            run.citation_fingerprint,
            run.checked_at
        );
        for finding in &run.citations {
            println!(
                "{} {} {:?} {:?}: {} {}",
                finding.citation.kind.as_str(),
                finding.repository_root,
                finding.citation.path,
                finding.citation.symbol,
                finding.tier,
                finding.message.as_deref().unwrap_or("")
            );
        }
        println!(
            "Current non-inferred consumer -> provider edges: {:?}",
            run.dependency_edges
        );
        for wave in attic_spec::waves::plan(&run.citations, &run.dependency_edges)? {
            println!(
                "Wave {}: {} basis={} cycle={} paths={:?} globs={:?}",
                wave.wave_number,
                wave.root,
                wave.ordering_basis,
                wave.cycle,
                wave.allowed_paths,
                wave.allowed_globs
            );
        }
        println!("Ordering is rechecked at begin. This is not cryptographic human identity.");
        print!("Type {code} to approve, anything else cancels: ");
        std::io::stdout().flush()?;
        let mut response = String::new();
        std::io::stdin().read_line(&mut response)?;
        Ok(response.trim() == code)
    }
}
