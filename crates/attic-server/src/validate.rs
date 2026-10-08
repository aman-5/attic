use crate::ServerError;
use std::collections::HashSet;

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
