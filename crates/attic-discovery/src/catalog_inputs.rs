//! Paths whose content feeds the cross-repository dependency catalog.
//!
//! Shared by indexing (which stores their indexed bytes) and the cross-repo
//! catalog (which parses them), so both always agree on the set.

/// Repo-relative paths Attic treats as dependency-declaration files for
/// freshness/invalidation purposes.
pub fn is_manifest_path(rel_path: &str) -> bool {
    matches!(
        rel_path,
        "go.mod"
            | "package.json"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "pyproject.toml"
            | "requirements.txt"
            | "requirements-dev.txt"
            | "requirements_dev.txt"
            | ".gitmodules"
    ) || rel_path.ends_with("/package.json")
        || rel_path.ends_with("/pom.xml")
        || rel_path.ends_with("/go.mod")
        || rel_path.ends_with("/pyproject.toml")
        || rel_path.ends_with("/MANIFEST.MF")
        || rel_path == "MANIFEST.MF"
        || rel_path.ends_with("/.content.xml")
}

/// Every path whose content the cross-repo catalog reads: dependency
/// manifests plus `.proto` files (generated-API imports).
pub fn is_catalog_input_path(rel_path: &str) -> bool {
    is_manifest_path(rel_path) || rel_path.ends_with(".proto")
}
