use attic_core::config::{PlanningConfig, PlanningMode};
use attic_spec::{Result, SpecError};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

pub(crate) const HELP: &str = "\
attic egm status
attic egm guidance
attic egm enable [--root PATH | --all-roots] [--mode manual|autopilot] [--enforce]
attic egm disable
attic spec --help

Enable preserves existing settings. New configurations default to the current
directory, autopilot mode, and report-only; an omitted mode defaults to autopilot.
These commands change only attic.toml; they never install hooks, index repositories,
start/stop a daemon, or approve a spec. Restart your Attic MCP server after changes.
Autopilot advances agent-owned workflow steps; human approval remains mandatory.
EGM reviewer agents run in your host; Attic supplies their verified review packs.";

struct Options {
    action: String,
    root: Option<PathBuf>,
    all_roots: bool,
    mode: Option<PlanningMode>,
    enforce: Option<bool>,
}

impl Options {
    fn parse(args: Vec<OsString>) -> Result<Self> {
        let mut args = args.into_iter();
        let action = args
            .next()
            .unwrap_or_else(|| "status".into())
            .into_string()
            .map_err(|_| SpecError::new("ERR_INVALID_ARGUMENT", "non-UTF8 action"))?;
        if !matches!(
            action.as_str(),
            "enable" | "disable" | "status" | "guidance"
        ) {
            return Err(SpecError::new(
                "ERR_INVALID_ARGUMENT",
                "expected egm enable, disable, status, or guidance",
            ));
        }
        let mut options = Self {
            action,
            root: None,
            all_roots: false,
            mode: None,
            enforce: None,
        };
        let mut seen = std::collections::HashSet::new();
        while let Some(arg) = args.next() {
            let flag = arg
                .into_string()
                .map_err(|_| SpecError::new("ERR_INVALID_ARGUMENT", "non-UTF8 option"))?;
            if options.action != "enable" || !seen.insert(flag.clone()) {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    format!("unexpected or duplicate option: {flag}"),
                ));
            }
            match flag.as_str() {
                "--root" => {
                    options.root = Some(PathBuf::from(args.next().ok_or_else(|| {
                        SpecError::new("ERR_INVALID_ARGUMENT", "--root requires a path")
                    })?));
                }
                "--all-roots" => options.all_roots = true,
                "--enforce" => options.enforce = Some(true),
                "--mode" => {
                    let mode = args.next().ok_or_else(|| {
                        SpecError::new("ERR_INVALID_ARGUMENT", "--mode requires a value")
                    })?;
                    options.mode = Some(match mode.to_str() {
                        Some("manual") => PlanningMode::Manual,
                        Some("autopilot") => PlanningMode::Autopilot,
                        _ => {
                            return Err(SpecError::new(
                                "ERR_INVALID_ARGUMENT",
                                "--mode must be manual or autopilot",
                            ));
                        }
                    });
                }
                _ => {
                    return Err(SpecError::new(
                        "ERR_INVALID_ARGUMENT",
                        format!("unknown option: {flag}"),
                    ));
                }
            }
        }
        if options.all_roots && options.root.is_some() {
            return Err(SpecError::new(
                "ERR_INVALID_ARGUMENT",
                "--root and --all-roots are mutually exclusive",
            ));
        }
        Ok(options)
    }
}

pub(crate) fn run(args: Vec<OsString>) -> i32 {
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return 0;
    }
    match Options::parse(args).and_then(|options| execute(&options)) {
        Ok(status) => {
            println!("{status}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn planning(text: Option<&str>) -> Result<PlanningConfig> {
    text.map(attic_core::AtticConfig::parse_str)
        .transpose()
        .map(|config| config.unwrap_or_default().planning)
        .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))
}

pub(crate) fn status(config: &PlanningConfig, path: &Path) -> Value {
    json!({
        "enabled": config.enabled,
        "mode": config.mode,
        "enforce": config.fast_path.enforce,
        "roots": config.roots,
        "config_path": path,
        "approval": "human_only",
        "reviewers": "host_driven",
        "review_completion": "host_attested",
        "answer_review": "host_attested",
        "host_review_support": "unknown",
        "automatic_review_guaranteed": false,
        "activation": {
            "enable": "attic egm enable --root PATH",
            "autopilot": "attic egm enable --root PATH --mode autopilot",
            "disable": "attic egm disable",
            "guidance": "attic egm guidance",
            "restart_required_after_change": true
        }
    })
}

pub(crate) fn guidance(config: &PlanningConfig, path: &Path) -> Value {
    let mut response = status(config, path);
    response["workflow"] = json!({
        "advance": match config.mode {
            PlanningMode::Manual => "Ask the user before advancing each workflow step.",
            PlanningMode::Autopilot => "Advance agent-owned steps automatically; stop for human approval.",
        },
        "steps": [
            "Author a bounded spec with an attic-citations fence.",
            "Call spec register, then verify. Resolve verification failures before review.",
            "Call spec review_start, then review_prepare. Use the pinned upstream persona definitions in fresh host reviewer contexts for every required stage.",
            "Submit each round with spec review_submit. Resolve findings and reverify as needed; two clean rounds are required before approval.",
            "Ask the user to run attic spec approve SPEC_ID in their own terminal. Never automate approval.",
            "Call begin, heartbeat, and provider-first wave_complete using the returned lease and fence.",
            "Run affected checks, stage intended content, then call drift. Drift verifies citation consistency, not test success."
        ],
        "reviewers": [
            {
                "persona": "zoom_out",
                "pack": "spec action=pack persona=zoom_out delivery=file",
                "mission": "Read the spec and supplied pack. Review architecture, scope, dependencies and unintended system effects. Report evidenced blockers or explicitly no blockers."
            },
            {
                "persona": "codebase_details",
                "pack": "spec action=pack persona=codebase_details delivery=file",
                "mission": "Read the spec and supplied pack. Verify named APIs, call sites, source assumptions and regressions against evidence. Report file/line-backed mismatches."
            },
            {
                "persona": "implementation_readiness",
                "pack": "spec action=pack persona=implementation_readiness delivery=file",
                "mission": "Read the spec and supplied pack. Check execution order, edge cases, acceptance criteria, tests, recovery and remaining implementation ambiguity."
            }
        ],
        "isolation": "The host creates separate reviewer contexts; Attic records host attestations but cannot prove isolation, launch agents, or install a host plugin.",
        "pack_fallback": "Use inline delivery only when the host cannot read private pack files; it consumes caller context.",
        "answer_review": "For each codebase answer, call answer_review prepare with the draft and cited files; review the exact sanitized evidence returned, never raw values or a truncated copy. Review-result JSON is limited to 512 KiB; the server also caps encoded MCP results. For redacted sources, give the reviewer each limitation and require redaction_acknowledged=true and claims_assessable=true only when all claims remain assessable. Finish with the returned hashes and report. If evidence is incomplete, a claim depends on hidden text, or review is unavailable, explicitly label the answer unreviewed and include the reason. Limited reviews have status reviewed_host_attested_limited; host attestations do not prove reviewer independence, complete access, or correctness."
    });
    response
}

fn execute(options: &Options) -> Result<Value> {
    let paths = attic_core::AtticPaths::resolve_read_only()
        .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?;
    let path = &paths.runtime_config;
    if matches!(options.action.as_str(), "status" | "guidance") {
        let config = planning(read_config(path)?.as_deref())?;
        return Ok(if options.action == "guidance" {
            guidance(&config, path)
        } else {
            status(&config, path)
        });
    }
    let explicit_root = options
        .root
        .as_ref()
        .map(|root| -> Result<PathBuf> {
            let root = root.canonicalize()?;
            if !root.is_dir() {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    "EGM root must be an existing directory",
                ));
            }
            Ok(root)
        })
        .transpose()?;
    let parent = path
        .parent()
        .ok_or_else(|| SpecError::new("ERR_CONFIG", "configuration has no parent directory"))?;
    attic_core::ensure_private_dir(parent)?;
    let lock_path = path.with_extension("toml.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    attic_core::set_private_file_permissions(&lock_path)?;
    lock.try_lock()
        .map_err(|e| SpecError::new("ERR_CONFIG_BUSY", e.to_string()))?;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(SpecError::new(
            "ERR_CONFIG",
            "refusing to replace a symlinked attic.toml; edit its target explicitly",
        ));
    }
    let original = read_config(path)?;
    planning(original.as_deref())?;
    let mut document = original
        .as_deref()
        .unwrap_or(attic_core::ATTIC_TOML_TEMPLATE)
        .parse::<DocumentMut>()
        .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?;
    if document.get("planning").is_none() {
        document["planning"] = Item::Table(Table::new());
    }
    document["planning"]["enabled"] = value(options.action == "enable");
    if options.action == "enable" {
        if let Some(mode) = options.mode {
            document["planning"]["mode"] = value(match mode {
                PlanningMode::Manual => "manual",
                PlanningMode::Autopilot => "autopilot",
            });
        }
        if options.all_roots {
            document["planning"]["roots"] = value(toml_edit::Array::new());
        } else if options.root.is_some() || document["planning"].get("roots").is_none() {
            let root = explicit_root
                .clone()
                .map(Ok)
                .unwrap_or_else(std::env::current_dir)?
                .canonicalize()?;
            if !root.is_dir() {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    "EGM root must be an existing directory",
                ));
            }
            let mut roots = toml_edit::Array::new();
            roots.push(
                root.to_str().ok_or_else(|| {
                    SpecError::new("ERR_INVALID_ARGUMENT", "EGM root must be UTF-8")
                })?,
            );
            document["planning"]["roots"] = value(roots);
        }
        if let Some(enforce) = options.enforce {
            if document["planning"].get("fast_path").is_none() {
                document["planning"]["fast_path"] = Item::Table(Table::new());
            }
            document["planning"]["fast_path"]["enforce"] = value(enforce);
        }
    }
    let updated = document.to_string();
    let config = planning(Some(&updated))?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.write_all(updated.as_bytes())?;
    staging.as_file().sync_all()?;
    attic_core::set_private_file_permissions(staging.path())?;
    if read_config(path)? != original {
        return Err(SpecError::new(
            "ERR_CONFIG_CHANGED",
            "attic.toml changed concurrently; retry without overwriting the external edit",
        ));
    }
    staging
        .persist(path)
        .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?;
    let mut response = status(&config, path);
    response["changed"] = json!(true);
    response["message"] = json!(
        "Configuration saved. Restart your Attic MCP server to apply it. No hooks or reviewers were installed; no spec was approved."
    );
    Ok(response)
}
