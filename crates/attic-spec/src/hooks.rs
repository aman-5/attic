use crate::{Result, SpecError, git};
use serde_json::{Value, json};
use std::{io::Write, path::Path};
pub const MARKER: &str = "# attic-managed: spec-boundary v1";

pub fn git_script(fail_closed: bool) -> String {
    format!(
        "#!/bin/sh\n{MARKER}\nMISSING_EXIT={}\nHOOK_DIR=$(dirname \"$0\")\nif [ -x \"$HOOK_DIR/pre-commit.attic-chained\" ]; then\n  \"$HOOK_DIR/pre-commit.attic-chained\" \"$@\" || exit $?\nfi\nif ! command -v attic >/dev/null 2>&1; then\n  echo 'attic unavailable; spec boundary was not checked' >&2\n  exit \"$MISSING_EXIT\"\nfi\nattic spec check-commit\nSTATUS=$?\ncase \"$STATUS\" in\n  0|1) exit \"$STATUS\";;\n  *) exit \"$MISSING_EXIT\";;\nesac\n",
        if fail_closed { 1 } else { 0 }
    )
}
pub fn host_config(fail_closed: bool) -> Value {
    let wrapper = |command: &str| {
        format!(
            "if command -v attic >/dev/null 2>&1; then attic spec {command}; else echo 'attic missing: no spec check' >&2; exit {}; fi",
            if fail_closed { 2 } else { 0 }
        )
    };
    json!({"hooks":{
        "SessionStart":[{"hooks":[{"type":"command","command":wrapper("baseline-start --host claude")}]}],
        "PreToolUse":[{"matcher":"Edit|Write|MultiEdit|NotebookEdit|Bash","hooks":[{"type":"command","command":wrapper("check-write --host claude")}]}]
    },"attic_notes":"Merge hooks only; do not copy this attic_notes key into settings. SessionStart resumes preserve existing baselines. Exit 2 blocks PreToolUse. Start failure/timeout enforcement needs a verified host; onFailure=block requires Claude Code >=2.1.295. This snippet does not install or claim that capability."})
}
fn hooks_dir(root: &Path) -> Result<std::path::PathBuf> {
    let output = git::output(root, &["rev-parse", "--git-path", "hooks"])?;
    let text = std::str::from_utf8(&output)
        .map_err(|_| SpecError::new("ERR_GIT", "non-UTF8 hooks directory"))?
        .trim();
    Ok(root.join(text))
}
pub fn install(root: &Path, chain: bool, fail_closed: bool) -> Result<String> {
    let root = git::toplevel(root)?;
    if root.join(".husky").exists() || root.join(".pre-commit-config.yaml").exists() {
        return Ok(format!(
            "Existing hook manager detected. Add this manually:\n{}",
            git_script(fail_closed)
        ));
    }
    let dir = hooks_dir(&root)?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join("pre-commit");
    let chained = dir.join("pre-commit.attic-chained");
    let mut moved = false;
    if target.exists() && !std::fs::read_to_string(&target)?.contains(MARKER) {
        if !chain || chained.exists() {
            return Err(SpecError::new(
                "ERR_HOOK_CONFLICT",
                "existing hook preserved; use --chain only if no chained hook exists",
            ));
        }
        std::fs::rename(&target, &chained)?;
        moved = true;
    }
    let staging = dir.join(format!("pre-commit.attic-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staging)?;
        file.write_all(git_script(fail_closed).as_bytes())?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staging, &target)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(staging);
    if let Err(error) = result {
        if moved {
            let _ = std::fs::rename(chained, target);
        }
        return Err(error);
    }
    Ok(
        "Installed only the explicitly selected Git pre-commit hook. No host settings changed."
            .into(),
    )
}
pub fn uninstall(root: &Path) -> Result<()> {
    let dir = hooks_dir(&git::toplevel(root)?)?;
    let target = dir.join("pre-commit");
    let chained = dir.join("pre-commit.attic-chained");
    if !target.exists() {
        return Ok(());
    }
    if !std::fs::read_to_string(&target)?.contains(MARKER) {
        return Err(SpecError::new(
            "ERR_HOOK_CONFLICT",
            "unmanaged hook preserved",
        ));
    }
    std::fs::remove_file(&target)?;
    if chained.exists() {
        std::fs::rename(chained, target)?;
    }
    Ok(())
}
pub const GUIDANCE: &str = r#"# Attic spec-first workflow (Claude Code pilot)

EGM is installed separately from google/elephant-goldfish-model v0.1.1,
dea7761de06ce5b51c177ebb9bba0469f24af5a4. Do not edit upstream tool allowlists.
Reviewers remain Read/Grep/Glob; the synthesizer cannot use MCP.

Capture a session/root baseline at SessionStart, before any mutating tool.
Repeated SessionStart/resume must keep the original baseline. Without that hook,
run `attic spec baseline-start --root <root> --session <id>` explicitly BEFORE
edits; never use a late baseline to exempt work already performed. Without one,
accounting is conservative. This measures content changes, not authorship.

When uncovered changes exceed the fast-path budget, author
docs/designs/<spec_id>.md (or configured designs_dir), using exactly one block:

```attic-citations
EXISTING_REF repo src/provider.rs Provider
TARGET_MODIFY repo src/consumer.rs Consumer
TARGET_CREATE_FILE repo src/new.rs
TARGET_CREATE_SYMBOL repo src/consumer.rs NewSymbol
CROSS_REPO_DEPENDENCY consumer -> provider
ALLOW_GLOB repo tests/**
```

Paths are relative with / separators; no . or .. segments. Qualified names must
match the analyzer exactly, not a suffix. Missing/old analyzer receipts cannot
prove symbol absence; reindex only through separately authorized normal indexing.
Register -> verify -> review -> HUMAN approval -> begin -> heartbeat and complete
provider-first waves -> stage intended content -> drift. Never run
`attic spec approve` yourself; ask the user to run it in their own terminal.
Approval is never an MCP action and is never automated in autopilot mode.

Before EGM review, request spec action=pack for zoom_out, codebase_details and
implementation_readiness. Pass each returned absolute path to the synthesizer
as explicit context for egm-zoom-out, egm-codebase-details and the pinned
implementation_readiness reviewer respectively. Reviewers read the target spec
themselves. Do not inline full packs into the main context unless file Read
permissions are unavailable; inline fallback has disclosed context cost.

Stage intended content before successful final drift if persistent branch
content receipts are required. Partial staging is checked against the actual
index and cannot borrow approval for different working-tree bytes. Changed-again
content, deletions, renames and modes are rechecked. Old leases cannot resume;
obtain a new lease/fence. Reopen requires a reason and renewed human approval.

Pre-write hooks only cover named Edit/Write/MultiEdit/NotebookEdit targets.
Bash is advisory except approval/--no-verify deny patterns. Hooks can be bypassed;
Git history cannot prove whether hooks ran. A real Claude version/payload/Read
permission pilot remains required; no host configuration is installed implicitly.
Default enforcement is report-only. Disabled checker entrypoints are successful
no-ops without creating workflow state. Do not claim drift proves passing tests.
"#;
