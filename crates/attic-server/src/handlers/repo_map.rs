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

fn bounded_integer(
    args: &HashMap<String, Value>,
    name: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ServerError> {
    match args.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| ServerError::InvalidArg(format!("{name} must be in {min}..={max}"))),
    }
}

fn truncate_tree(nodes: &mut [Value], depth: usize) -> bool {
    let mut truncated = false;
    for node in nodes {
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            if depth == 1 && !children.is_empty() {
                children.clear();
                node["children_omitted"] = json!(true);
                truncated = true;
            } else if depth > 1 {
                truncated |= truncate_tree(children, depth - 1);
            }
        }
    }
    truncated
}

fn map_page(
    files: &[(String, String)],
    offset: usize,
    count: usize,
    depth: usize,
    metadata: &Value,
) -> Value {
    let mut root = RepoMapDirNode::default();
    for (path, ft) in &files[offset..offset + count] {
        let components: Vec<_> = path.split('/').filter(|s| !s.is_empty()).collect();
        root.insert(&components, ft);
    }
    let mut tree = root.to_json();
    let depth_limited = truncate_tree(&mut tree, depth);
    let next = offset + count;
    let mut response = metadata.clone();
    response["tree"] = json!(tree);
    response["coverage"] = json!({
        "total_matching_files": files.len(),
        "offset": offset,
        "returned_files": count,
        "next_offset": if next < files.len() && count > 0 { Some(next) } else { None },
        "complete": next == files.len() && !depth_limited,
        "depth_limited": depth_limited,
        "narrowing_required": count == 0 && next < files.len(),
    });
    response
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
    let file_type = args
        .get("file_type")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| ServerError::InvalidArg("file_type must be a string".into()))
        })
        .transpose()?;
    if let Some(ft) = file_type {
        validate_filter("file_type", ft, 32)?;
    }
    let raw_prefix = args
        .get("path_prefix")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| ServerError::InvalidArg("path_prefix must be a string".into()))
        })
        .transpose()?
        .unwrap_or("");
    let prefix = raw_prefix.trim_end_matches('/');
    if raw_prefix.starts_with('/') {
        return Err(ServerError::InvalidArg(
            "path_prefix must be repository-relative".into(),
        ));
    }
    if !prefix.is_empty()
        && (prefix.contains(['\\', ':'])
            || prefix.chars().any(char::is_control)
            || prefix
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == ".."))
    {
        return Err(ServerError::InvalidArg(
            "path_prefix must be a relative /-separated directory path".into(),
        ));
    }
    let offset = bounded_integer(args, "offset", 0, 0, usize::MAX)?;
    let limit = bounded_integer(args, "limit", 200, 1, 1000)?;
    let depth = bounded_integer(args, "max_depth", 4, 1, 32)?;
    let max_bytes = bounded_integer(args, "max_bytes", 32_768, 4096, 131_072)?;

    let all_stats = pool.with_reader(get_repository_stats)?;
    let stats = all_stats.into_iter().find(|s| s.id == repo_id);

    let parsed_repo_id = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;
    let mut files =
        pool.with_reader(|c| current_files_for_repo_map(c, &parsed_repo_id, file_type))?;
    files.retain(|(path, _)| {
        prefix.is_empty()
            || path
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'))
    });
    files.sort();
    if offset > files.len() {
        return Err(ServerError::InvalidArg(
            "offset exceeds matching file count; restart pagination after index changes".into(),
        ));
    }

    // PR-3: last observed discovery-walk counters for this repository, if
    // any bootstrap/reindex has run this process — answers "why did the
    // filesystem count and indexed count differ" without server logs.
    let discovery = discovery_counters.get(repo_id);
    let mut diagnostics: Vec<Value> = discovery_diagnostics
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

    let diagnostic_count = diagnostics.len();
    diagnostics.truncate(20);
    let mut metadata = json!({
        "repository_id": repo_id,
        "stats": stats,
        "discovery": discovery,
        "diagnostics": diagnostics,
        "response_budget": {
            "max_bytes": max_bytes,
            "diagnostics_omitted": diagnostic_count.saturating_sub(diagnostics.len()),
            "guidance": "Continue with next_offset and the same filters; pagination is not a snapshot across reindexing. Use path_prefix, max_depth, or a larger max_bytes when narrowing_required is true."
        }
    });
    loop {
        let empty = map_page(&files, offset, 0, depth, &metadata);
        if serde_json::to_vec(&empty)?.len() <= max_bytes {
            break;
        }
        let diagnostics = metadata["diagnostics"].as_array_mut().ok_or_else(|| {
            ServerError::InvalidArg("repository map diagnostics must be an array".into())
        })?;
        if diagnostics.pop().is_none() {
            return Err(ServerError::InvalidArg(
                "repository metadata exceeds max_bytes; increase the output budget".into(),
            ));
        }
        let omitted = diagnostic_count - diagnostics.len();
        metadata["response_budget"]["diagnostics_omitted"] = json!(omitted);
    }
    let mut low = 0;
    let mut high = limit.min(files.len() - offset);
    while low < high {
        let count = low + (high - low).div_ceil(2);
        let response = map_page(&files, offset, count, depth, &metadata);
        if serde_json::to_vec(&response)?.len() <= max_bytes {
            low = count;
        } else {
            high = count - 1;
        }
    }
    let response = map_page(&files, offset, low, depth, &metadata);
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string(&response)?,
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_map_bounds_actual_utf8_payload_and_paginates_filtered_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        for prefix in ["src", "src-other"] {
            std::fs::create_dir_all(workspace.join(prefix)).unwrap();
        }
        for i in 0..220 {
            std::fs::write(
                workspace.join("src").join(format!("file-{i:03}-é.txt")),
                "map fixture\n",
            )
            .unwrap();
        }
        std::fs::write(workspace.join("src-other").join("excluded.txt"), "fixture").unwrap();
        let server = AtticServer::new(&dir.path().join("catalog.db")).unwrap();
        let repo = server.bootstrap_workspace(&workspace).unwrap();
        let active = HashSet::from([repo.clone()]);
        let mut args = HashMap::from([
            ("repository_id".into(), json!(repo)),
            ("path_prefix".into(), json!("src")),
            ("max_bytes".into(), json!(4096)),
        ]);
        let mut offset = 0;
        loop {
            args.insert("offset".into(), json!(offset));
            let result = handle_repo_map(
                &server.pool,
                &args,
                &active,
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();
            let text = match &result.content[0] {
                ContentBlock::Text(text) => &text.text,
                _ => panic!("expected JSON text"),
            };
            assert!(text.len() <= 4096, "{} bytes", text.len());
            let page: Value = serde_json::from_str(text).unwrap();
            assert_eq!(page["coverage"]["total_matching_files"], 220);
            assert_eq!(page["coverage"]["offset"], offset);
            assert!(page["coverage"]["returned_files"].as_u64().unwrap() > 0);
            if let Some(next) = page["coverage"]["next_offset"].as_u64() {
                assert!(next > offset);
                offset = next;
            } else {
                assert_eq!(page["coverage"]["complete"], true);
                assert_eq!(
                    offset + page["coverage"]["returned_files"].as_u64().unwrap(),
                    220
                );
                break;
            }
        }
        for invalid in [json!(-1), json!("4096"), json!(131073)] {
            args.insert("max_bytes".into(), invalid);
            assert!(
                handle_repo_map(
                    &server.pool,
                    &args,
                    &active,
                    &HashMap::new(),
                    &HashMap::new(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn map_pages_account_for_depth_and_advance_without_losing_files() {
        let files = vec![
            ("src/a.rs".into(), "rust".into()),
            ("src/nested/b.rs".into(), "rust".into()),
            ("tests/c.rs".into(), "rust".into()),
        ];
        let metadata = json!({});
        let first = map_page(&files, 0, 2, 1, &metadata);
        assert_eq!(first["coverage"]["next_offset"], 2);
        assert_eq!(first["coverage"]["depth_limited"], true);
        assert_eq!(first["tree"][0]["children_omitted"], true);
        let last = map_page(&files, 2, 1, 4, &metadata);
        assert_eq!(last["coverage"]["next_offset"], Value::Null);
        assert_eq!(last["coverage"]["complete"], true);
        assert_eq!(last["tree"][0]["children"][0]["name"], "c.rs");
        let stuck = map_page(&files, 0, 0, 4, &metadata);
        assert_eq!(stuck["coverage"]["narrowing_required"], true);
        assert_eq!(stuck["coverage"]["next_offset"], Value::Null);
    }
}
