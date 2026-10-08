use crate::*;

/// A directory node in the derived `repo_map` tree. Directories are never a
/// persisted entity — this tree is rebuilt at read time from the current
/// generation's active file paths, so an empty directory (or one left empty
/// by a `file_type` filter) simply never gets a node here.
///
/// `dirs`/`files` are kept in separate maps (rather than one map keyed by
/// name) so serialization can enforce "directories before files, then
/// lexicographic" regardless of how directory and file names interleave;
/// `BTreeMap` gives deterministic lexicographic order within each group.
#[derive(Default)]
pub(crate) struct RepoMapDirNode {
    dirs: std::collections::BTreeMap<String, RepoMapDirNode>,
    files: std::collections::BTreeMap<String, String>,
}

impl RepoMapDirNode {
    pub(crate) fn insert(&mut self, components: &[&str], file_type: &str) {
        match components {
            [] => {}
            [name] => {
                self.files
                    .insert((*name).to_string(), file_type.to_string());
            }
            [dir, rest @ ..] => {
                self.dirs
                    .entry((*dir).to_string())
                    .or_default()
                    .insert(rest, file_type);
            }
        }
    }

    pub(crate) fn to_json(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.dirs.len() + self.files.len());
        for (name, node) in &self.dirs {
            out.push(json!({
                "name": name,
                "type": "directory",
                "children": node.to_json(),
            }));
        }
        for (name, file_type) in &self.files {
            // Guards against an impossible filesystem shape that stale
            // (not-yet-tombstoned) occurrence data can produce — e.g. a
            // leftover row for file "foo" alongside a newer one for
            // "foo/sub.rs", where "foo" would need to be both a file and a
            // directory at the same tree level. Directories win
            // deterministically regardless of insertion order (checked here
            // rather than in `insert`, since a directory node for this name
            // may not exist yet at insert time but appear later): the
            // conflicting file is dropped rather than rendering two sibling
            // nodes with the same name, which no real filesystem could
            // produce and which would be a nonsensical tree to hand to a
            // caller.
            if self.dirs.contains_key(name) {
                continue;
            }
            out.push(json!({
                "name": name,
                "type": "file",
                "file_type": file_type,
            }));
        }
        out
    }
}

pub(crate) fn handle_repo_map(
    pool: &DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    discovery_counters: &HashMap<String, attic_discovery::WalkCounters>,
    discovery_diagnostics: &HashMap<String, Vec<attic_discovery::Diagnostic>>,
) -> Result<CallToolResult, ServerError> {
    let repo_id = args
        .get("repository_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;
    require_active_member(active_ids, repo_id)?;
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type {
        validate_filter("file_type", ft, 32)?;
    }

    let all_stats = pool.with_reader(get_repository_stats)?;
    let stats = all_stats.into_iter().find(|s| s.id == repo_id);

    let parsed_repo_id = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;
    let files = pool.with_reader(|c| current_files_for_repo_map(c, &parsed_repo_id, file_type))?;

    let mut root = RepoMapDirNode::default();
    for (path, ft) in &files {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        root.insert(&components, ft);
    }

    // PR-3: last observed discovery-walk counters for this repository, if
    // any bootstrap/reindex has run this process — answers "why did the
    // filesystem count and indexed count differ" without server logs.
    let discovery = discovery_counters.get(repo_id);
    let diagnostics: Vec<Value> = discovery_diagnostics
        .get(repo_id)
        .into_iter()
        .flatten()
        .map(|d| {
            json!({
                "kind": diagnostic_kind_str(&d.kind),
                "path": d.path.display().to_string(),
                "message": d.message,
            })
        })
        .collect();

    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({
            "repository_id": repo_id,
            "stats": stats,
            "tree": root.to_json(),
            "discovery": discovery,
            "diagnostics": diagnostics,
        }))?,
    )]))
}
