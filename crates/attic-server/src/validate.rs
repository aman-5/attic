use crate::ServerError;
use attic_storage::PathScope;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub(crate) fn validate_filter(name: &str, value: &str, max_len: usize) -> Result<(), ServerError> {
    if value.len() > max_len {
        return Err(ServerError::InvalidArg(format!(
            "{name} too long (max {max_len})"
        )));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(ServerError::InvalidArg(format!(
            "{name} contains control characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate_repository_id(id: &str) -> Result<(), ServerError> {
    validate_filter("repository_id", id, 64)?;
    if !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(ServerError::InvalidArg(
            "repository_id must be a UUID (hex digits and hyphens only)".into(),
        ));
    }
    Ok(())
}

pub(crate) fn parse_path_scope(
    args: &HashMap<String, Value>,
) -> Result<Option<PathScope>, ServerError> {
    let mut selected = None;
    for key in ["path", "path_prefix", "path_glob"] {
        let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        if selected.is_some() {
            return Err(ServerError::InvalidArg(
                "specify only one of path, path_prefix, or path_glob".into(),
            ));
        }
        let raw = value
            .as_str()
            .ok_or_else(|| ServerError::InvalidArg(format!("{key} must be a string")))?;
        validate_filter(key, raw, 1024)?;
        let normalized = raw.replace('\\', "/");
        let normalized = normalized.trim_start_matches("./").trim_end_matches('/');
        if normalized.is_empty()
            || normalized.starts_with('/')
            || normalized
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
            || normalized
                .as_bytes()
                .get(1)
                .is_some_and(|byte| *byte == b':')
        {
            return Err(ServerError::InvalidArg(format!(
                "{key} must be a normalized repository-relative path"
            )));
        }
        if key == "path_glob" && normalized.contains(['[', ']']) {
            return Err(ServerError::InvalidArg(
                "path_glob supports only * and ? wildcards".into(),
            ));
        }
        selected = Some(match key {
            "path" => PathScope::Exact(normalized.to_owned()),
            "path_prefix" => PathScope::Prefix(normalized.to_owned()),
            _ => PathScope::Glob(normalized.to_owned()),
        });
    }
    Ok(selected)
}

/// Reject an explicit `repository_id` that does not belong to the currently
/// configured logical workspace (membership-authoritative retrieval, §14/§16).
pub(crate) fn require_active_member(
    active_ids: &HashSet<String>,
    repo_id: &str,
) -> Result<(), ServerError> {
    if active_ids.contains(repo_id) {
        return Ok(());
    }
    Err(ServerError::InvalidArg(format!(
        "repository_id {repo_id} is not part of the configured workspace — it may have been \
         removed from membership or never configured. Inspect membership with the `workspace` tool."
    )))
}

pub(crate) fn resolve_repository_selector(
    pool: &attic_storage::DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Result<HashMap<String, Value>, ServerError> {
    let selector = match args.get("repository") {
        None | Some(Value::Null) => return Ok(args.clone()),
        Some(Value::String(value)) if !value.trim().is_empty() => value.as_str(),
        Some(_) => {
            return Err(ServerError::InvalidArg(
                "repository must be a non-empty name, UUID, or absolute root path".into(),
            ));
        }
    };
    validate_filter("repository", selector, 4096)?;
    if args
        .get("repository_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(ServerError::InvalidArg(
            "repository and repository_id are mutually exclusive".into(),
        ));
    }
    let requested_path = Path::new(selector);
    let path_key = if requested_path.is_absolute() {
        let resolved = requested_path
            .canonicalize()
            .unwrap_or_else(|_| requested_path.to_path_buf());
        Some(crate::root_identity_key(&resolved))
    } else {
        None
    };
    let mut ids: Vec<_> = active_ids.iter().collect();
    ids.sort();
    let repositories = pool.with_reader(|conn| {
        let mut repositories = Vec::new();
        for id in ids {
            let parsed = id.parse::<attic_core::RepositoryId>().map_err(|error| {
                attic_storage::StorageError::CorruptDatabase {
                    reason: format!("invalid active repository ID: {error}"),
                }
            })?;
            let name = attic_storage::get_repository_display_name(conn, &parsed)?;
            let root = attic_storage::get_repository_path(conn, &parsed)?;
            if let (Some(name), Some(root)) = (name, root) {
                repositories.push((id.clone(), name, root));
            }
        }
        Ok(repositories)
    })?;
    let matches: Vec<_> = repositories
        .iter()
        .filter(|(id, name, root)| {
            id == selector
                || name.eq_ignore_ascii_case(selector)
                || path_key.as_ref().is_some_and(|key| {
                    let root_path = Path::new(root);
                    let resolved = root_path
                        .canonicalize()
                        .unwrap_or_else(|_| root_path.to_path_buf());
                    crate::root_identity_key(&resolved) == *key
                })
        })
        .collect();
    let id = match matches.as_slice() {
        [(id, _, _)] => id,
        [] => {
            return Err(ServerError::InvalidArg(format!(
                "repository {selector:?} is not an active workspace repository; inspect `workspace` for valid names, paths, and IDs"
            )));
        }
        _ => {
            let choices = matches
                .iter()
                .take(10)
                .map(|(id, name, root)| format!("{name} ({id}, {root})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ServerError::InvalidArg(format!(
                "repository {selector:?} is ambiguous; use repository_id or an absolute path: {choices}"
            )));
        }
    };
    let mut resolved = args.clone();
    resolved.remove("repository");
    resolved.insert("repository_id".into(), Value::String(id.clone()));
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_scope_normalizes_and_rejects_unsafe_or_ambiguous_values() {
        let args = HashMap::from([("path_prefix".into(), json!(r".\src\forms\"))]);
        assert_eq!(
            parse_path_scope(&args).unwrap(),
            Some(PathScope::Prefix("src/forms".into()))
        );
        for value in ["../secret", "/absolute", r"C:\absolute", "a//b"] {
            let args = HashMap::from([("path".into(), json!(value))]);
            assert!(parse_path_scope(&args).is_err(), "{value}");
        }
        let args = HashMap::from([
            ("path".into(), json!("one.json")),
            ("path_glob".into(), json!("*.json")),
        ]);
        assert!(parse_path_scope(&args).is_err());
    }
}
