//! Bounded parsers for build/package metadata (Phase 6 §4).
//!
//! SECURITY CONTRACT:
//! - Manifest content is UNTRUSTED DATA.  Nothing here executes, resolves
//!   over the network, or follows any instruction found in a manifest.
//! - Parsers never copy raw file content into outputs; diagnostics carry
//!   counts only, so secret bytes cannot leak into catalog/edge provenance.
//! - Every parser enforces [`crate::limits::MAX_MANIFEST_BYTES`] and
//!   bounded iteration; malformed input degrades to "no evidence" plus a
//!   diagnostic, never an error that would block indexing.

use std::collections::BTreeSet;

use crate::{DeclarationKind, DependencyDeclaration, Ecosystem, ProvidedIdentity, limits};

/// Result of parsing one manifest file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ManifestParse {
    /// Identities this file shows the repository PROVIDES.
    pub provides: Vec<ProvidedIdentity>,
    /// Dependency targets DECLARED by this file.
    pub declarations: Vec<DependencyDeclaration>,
    /// Count-only diagnostics (no content).
    pub diagnostics: Vec<String>,
}

impl ManifestParse {
    fn merge(&mut self, other: ManifestParse) {
        self.provides.extend(other.provides);
        self.declarations.extend(other.declarations);
        self.diagnostics.extend(other.diagnostics);
    }
}

/// Dispatch on repo-relative file name.  Unknown files parse to empty.
pub fn parse_manifest(rel_path: &str, bytes: &[u8]) -> ManifestParse {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    if bytes.len() as u64 > limits::MAX_MANIFEST_BYTES {
        let mut out = ManifestParse::default();
        out.diagnostics
            .push(format!("manifest_too_large:{rel_path}"));
        return out;
    }
    let text = String::from_utf8_lossy(bytes);
    match name {
        "go.mod" => parse_go_mod(&text),
        "package.json" => parse_package_json(&text),
        "pom.xml" => parse_pom_xml(&text),
        "build.gradle" | "build.gradle.kts" => parse_build_gradle(&text),
        "settings.gradle" | "settings.gradle.kts" => parse_settings_gradle(&text),
        "pyproject.toml" => parse_pyproject_toml(&text),
        "requirements.txt" | "requirements-dev.txt" | "requirements_dev.txt" => {
            parse_requirements_txt(&text)
        }
        ".gitmodules" => parse_gitmodules(&text),
        _ => ManifestParse::default(),
    }
}

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
}

// ---------------------------------------------------------------------------
// Go — go.mod
// ---------------------------------------------------------------------------

fn parse_go_mod(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let mut in_require_block = false;
    for raw in text.lines().take(10_000) {
        let line = strip_go_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("module ") {
            let name = line["module ".len()..].trim().trim_matches('"').to_string();
            if !name.is_empty() {
                out.provides.push(ProvidedIdentity {
                    ecosystem: Ecosystem::Go,
                    name,
                });
            }
            continue;
        }
        if line == "require (" {
            in_require_block = true;
            continue;
        }
        if in_require_block && line == ")" {
            in_require_block = false;
            continue;
        }
        if in_require_block || line.starts_with("require ") {
            let body = line.strip_prefix("require ").unwrap_or(&line).trim();
            if let Some(d) = go_require_decl(body) {
                out.declarations.push(d);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("replace ") {
            out.merge(go_replace_decl(rest));
        }
    }
    out
}

fn strip_go_comment(line: &str) -> &str {
    // `//` comments only; no string literals contain `//` in go.mod syntax
    // we consume (module paths/versions), so a naive split is safe here.
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn go_require_decl(body: &str) -> Option<DependencyDeclaration> {
    let mut parts = body.split_whitespace();
    let module = parts.next()?.trim_matches('"');
    if module.is_empty() {
        return None;
    }
    let version = parts.next().map(|v| v.trim_matches('"').to_string());
    Some(DependencyDeclaration {
        path: "go.mod".to_string(),
        ecosystem: Ecosystem::Go,
        name: module.to_string(),
        version_req: version,
        kind: DeclarationKind::External,
        local_hint: None,
    })
}

fn go_replace_decl(rest: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let Some((old, new)) = rest.split_once("=>") else {
        out.diagnostics.push("replace_missing_arrow".to_string());
        return out;
    };
    let old = old.trim().trim_matches('"').to_string();
    let new = new.trim().trim_matches('"').to_string();
    if new.starts_with("./") || new.starts_with("../") || new == "." {
        // Strip leading "./" but preserve ".." segments — the resolver
        // canonicalizes hints relative to the declaring repo root.
        let hint = if let Some(rest) = new.strip_prefix("./") {
            rest.to_string()
        } else {
            new
        };
        if hint.is_empty() {
            out.diagnostics.push("replace_empty_hint".to_string());
            return out;
        }
        out.declarations.push(DependencyDeclaration {
            path: "go.mod".to_string(),
            ecosystem: Ecosystem::Go,
            name: old,
            version_req: None,
            kind: DeclarationKind::LocalPath,
            local_hint: Some(hint),
        });
    }
    // Remote replacements stay external-by-name with NO local hint; they can
    // still resolve against another workspace repository's module identity.
    else if !new.is_empty() {
        out.declarations.push(DependencyDeclaration {
            path: "go.mod".to_string(),
            ecosystem: Ecosystem::Go,
            name: old,
            version_req: None,
            kind: DeclarationKind::External,
            local_hint: None,
        });
    }
    out
}

fn normalize_relative(p: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    let normalized = p.replace('\\', "/");
    for seg in normalized.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    stack.join("/")
}

// ---------------------------------------------------------------------------
// npm — package.json (+ workspaces)
// ---------------------------------------------------------------------------

fn parse_package_json(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        out.diagnostics.push("package_json_unparseable".to_string());
        return out;
    };
    if let Some(name) = v.get("name").and_then(|n| n.as_str())
        && !name.trim().is_empty() {
            out.provides.push(ProvidedIdentity {
                ecosystem: Ecosystem::Npm,
                name: name.trim().to_string(),
            });
        }
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = v.get(section).and_then(|m| m.as_object()) {
            for (dep_name, spec) in map.iter().take(limits::MAX_DECLARATIONS_PER_REPO) {
                let version = spec.as_str().map(str::to_string);
                let (kind, hint): (DeclarationKind, Option<String>) = match version.as_deref() {
                    Some(s) if is_npm_local_spec(s) => {
                        // Strip "file:" or "link:" prefix before normalizing
                        let path = s
                            .strip_prefix("file:")
                            .or_else(|| s.strip_prefix("link:"))
                            .unwrap_or(s);
                        (DeclarationKind::LocalPath, Some(normalize_relative(path)))
                    }
                    _ => (DeclarationKind::External, None),
                };
                if kind == DeclarationKind::LocalPath && hint.as_deref() == Some("") {
                    continue;
                }
                out.declarations.push(DependencyDeclaration {
                    path: "package.json".to_string(),
                    ecosystem: Ecosystem::Npm,
                    name: dep_name.clone(),
                    version_req: version,
                    kind,
                    local_hint: hint,
                });
            }
        }
    }
    // workspaces: ["packages/*", ...] or {"packages": [...]}
    let ws: Option<Vec<String>> = match v.get("workspaces") {
        Some(serde_json::Value::Array(a)) => Some(
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
        ),
        Some(serde_json::Value::Object(o)) => o.get("packages").and_then(|p| {
            p.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
        }),
        _ => None,
    };
    if let Some(pats) = ws {
        for p in pats.into_iter().take(limits::MAX_PROVIDES_PER_REPO) {
            if p.trim().is_empty() {
                continue;
            }
            out.declarations.push(DependencyDeclaration {
                path: "package.json".to_string(),
                ecosystem: Ecosystem::Npm,
                name: format!("workspace:{p}"),
                version_req: None,
                kind: DeclarationKind::WorkspaceMember,
                local_hint: Some(normalize_relative(&p)),
            });
        }
    }
    out
}

/// `file:` / `link:` specs are local; registry ranges and `workspace:` are
/// handled elsewhere (`workspace:` stays external — the member itself is the
/// provider candidate).
fn is_npm_local_spec(spec: &str) -> bool {
    spec.starts_with("file:") || spec.starts_with("link:") || spec.starts_with(".")
}

// ---------------------------------------------------------------------------
// Maven — pom.xml (bounded tag scanner)
// ---------------------------------------------------------------------------

fn parse_pom_xml(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let mut coords: [Option<String>; 2] = [None, None]; // groupId, artifactId at project level

    let mut stack: Vec<String> = Vec::with_capacity(16);
    let mut dep_group: Option<String> = None;
    let mut dep_artifact: Option<String> = None;
    let mut dep_version: Option<String> = None;
    let mut text_buf = String::new();

    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                if starts_with(bytes, i, b"<!--") {
                    // skip comment
                    match find_sub(bytes, i + 4, b"-->") {
                        Some(end) => {
                            i = end + 3;
                            continue;
                        }
                        None => {
                            out.diagnostics.push("pom_unterminated_comment".to_string());
                            break;
                        }
                    }
                }
                flush_text(
                    &stack,
                    &mut text_buf,
                    &mut coords,
                    &mut dep_group,
                    &mut dep_artifact,
                    &mut dep_version,
                    &mut out,
                );
                if starts_with(bytes, i, b"</") {
                    let Some(gt) = find_byte(bytes, i, b'>') else {
                        out.diagnostics
                            .push("pom_unterminated_close_tag".to_string());
                        break;
                    };
                    let name = text_slice_trimmed(bytes, i + 2, gt);
                    // close matching element
                    while let Some(top) = stack.pop() {
                        if top == name {
                            break;
                        }
                    }
                    if name == "dependency"
                        && stack.last().map(String::as_str) == Some("dependencies")
                    {
                        emit_dependency(
                            &mut out,
                            dep_group.take(),
                            dep_artifact.take(),
                            dep_version.take(),
                        );
                    }
                    i = gt + 1;
                } else {
                    // open tag (possibly self-closing) — skip attributes
                    let Some(gt) = find_byte(bytes, i, b'>') else {
                        out.diagnostics.push("pom_unterminated_tag".to_string());
                        break;
                    };
                    let self_closing = gt > i && bytes[gt - 1] == b'/';
                    let raw_name = text_slice_trimmed(bytes, i + 1, gt);
                    let name = raw_name
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    text_buf.clear();
                    if !self_closing {
                        if stack.len() >= 32 {
                            out.diagnostics.push("pom_depth_exceeded".to_string());
                            break;
                        }
                        stack.push(name.clone());
                    } else if name == "dependency"
                        && stack.last().map(String::as_str) == Some("dependencies")
                    {
                        emit_dependency(
                            &mut out,
                            dep_group.take(),
                            dep_artifact.take(),
                            dep_version.take(),
                        );
                    }
                    i = gt + 1;
                }
            }
            _ => {
                // accumulate element text until next '<'
                let start = i;
                while i < bytes.len() && bytes[i] != b'<' {
                    i += 1;
                }
                // decode the handful of entities pom coordinates use; unknown
                // entities pass through unchanged (harmless for identity keys)
                let chunk = &text[start..i.min(text.len())];
                push_decoded(chunk, &mut text_buf);
            }
        }
        if out.declarations.len() >= limits::MAX_DECLARATIONS_PER_REPO {
            out.diagnostics
                .push("pom_declaration_cap_reached".to_string());
            break;
        }
    }

    if let (Some(g), Some(a)) = (&coords[0], &coords[1]) {
        out.provides.push(ProvidedIdentity {
            ecosystem: Ecosystem::Maven,
            name: format!("{g}:{a}"),
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn flush_text(
    stack: &[String],
    buf: &mut String,
    coords: &mut [Option<String>; 2],
    dep_group: &mut Option<String>,
    dep_artifact: &mut Option<String>,
    dep_version: &mut Option<String>,
    out: &mut ManifestParse,
) {
    let t = buf.trim().to_string();
    buf.clear();
    if t.is_empty() || stack.is_empty() {
        return;
    }
    let parent: Option<&String> = stack.iter().rev().nth(1);
    let current = stack.last().map(String::as_str);
    match (parent.map(String::as_str), current) {
        (Some("project"), Some("groupId")) => {
            if coords[0].is_none() {
                coords[0] = Some(t);
            }
        }
        (Some("project"), Some("artifactId")) => {
            if coords[1].is_none() {
                coords[1] = Some(t);
            }
        }
        (Some("modules"), Some("module")) => {
            if out.declarations.len() < limits::MAX_DECLARATIONS_PER_REPO && !t.is_empty() {
                out.declarations.push(DependencyDeclaration {
                    path: "pom.xml".to_string(),
                    ecosystem: Ecosystem::Maven,
                    name: format!("module:{t}"),
                    version_req: None,
                    kind: DeclarationKind::WorkspaceMember,
                    local_hint: Some(t),
                });
            }
        }
        (Some("dependency"), Some("groupId")) => {
            *dep_group = Some(t);
        }
        (Some("dependency"), Some("artifactId")) => {
            *dep_artifact = Some(t);
        }
        (Some("dependency"), Some("version")) => {
            *dep_version = Some(t);
        }
        _ => {}
    }
}

fn emit_dependency(
    out: &mut ManifestParse,
    group: Option<String>,
    artifact: Option<String>,
    version: Option<String>,
) {
    let (Some(g), Some(a)) = (group, artifact) else {
        out.diagnostics
            .push("pom_dependency_missing_coords".to_string());
        return;
    };
    if g.contains('$') || a.contains('$') {
        // property placeholders cannot be resolved without executing Maven
        out.diagnostics
            .push("pom_dependency_property_placeholder".to_string());
        return;
    }
    out.declarations.push(DependencyDeclaration {
        path: "pom.xml".to_string(),
        ecosystem: Ecosystem::Maven,
        name: format!("{g}:{a}"),
        version_req: version,
        kind: DeclarationKind::External,
        local_hint: None,
    });
}

fn push_decoded(chunk: &str, buf: &mut String) {
    let mut rest = chunk;
    while let Some(pos) = rest.find('&') {
        buf.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(semi) = tail.find(';') {
            let ent = &tail[1..semi];
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => None,
            };
            match decoded {
                Some(c) => buf.push(c),
                None => buf.push_str(tail[..=semi].as_ref()),
            }
            rest = &tail[semi + 1..];
        } else {
            buf.push('&');
            rest = &tail[1..];
        }
    }
    buf.push_str(rest);
}

fn starts_with(bytes: &[u8], at: usize, pat: &[u8]) -> bool {
    bytes.len() >= at + pat.len() && &bytes[at..at + pat.len()] == pat
}

fn find_byte(bytes: &[u8], from: usize, b: u8) -> Option<usize> {
    bytes[from..].iter().position(|&x| x == b).map(|p| p + from)
}

fn find_sub<'a>(bytes: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    let hay = &bytes[from..];
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    hay.windows(pat.len())
        .position(|w| w == pat)
        .map(|p| p + from)
}

fn text_slice_trimmed(bytes: &[u8], from: usize, to: usize) -> String {
    String::from_utf8_lossy(&bytes[from..to.min(bytes.len())])
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Gradle — build.gradle / settings.gradle (line scanner)
// ---------------------------------------------------------------------------

fn parse_settings_gradle(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    for raw in text.lines().take(5_000) {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("rootProject.name") {
            if let Some(name) = first_quoted(rest) {
                out.provides.push(ProvidedIdentity {
                    ecosystem: Ecosystem::Gradle,
                    name,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("include") {
            for token in gradle_tokens(rest) {
                let hint = token.trim_start_matches(':').replace(':', "/");
                if hint.is_empty() {
                    continue;
                }
                out.declarations.push(DependencyDeclaration {
                    path: "settings.gradle".to_string(),
                    ecosystem: Ecosystem::Gradle,
                    name: format!("project:{token}"),
                    version_req: None,
                    kind: DeclarationKind::WorkspaceMember,
                    local_hint: Some(hint),
                });
            }
        }
    }
    out
}

fn parse_build_gradle(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let mut in_deps = false;
    for raw in text.lines().take(10_000) {
        let line = raw.trim();
        if line == "dependencies" && line.ends_with('{') || line == "dependencies {" {
            in_deps = true;
            continue;
        }
        if in_deps {
            if line == "}" {
                in_deps = false;
            } else {
                out.declarations
                    .extend(gradle_dependency_line("build.gradle", line));
            }
        }
    }
    out
}

/// One dependencies-block line → zero or more declarations.
fn gradle_dependency_line(path: &str, line: &str) -> Vec<DependencyDeclaration> {
    let mut out = Vec::new();
    // project(...) references
    if let Some(open) = line.find("project(")
        && let Some(token) = first_quoted(&line[open..]) {
            let hint = token.trim_start_matches(':').replace(':', "/");
            if !hint.is_empty() {
                out.push(DependencyDeclaration {
                    path: path.to_string(),
                    ecosystem: Ecosystem::Gradle,
                    name: format!("project:{token}"),
                    version_req: None,
                    kind: if hint.is_empty() {
                        DeclarationKind::External
                    } else {
                        DeclarationKind::LocalPath
                    },
                    local_hint: Some(hint),
                });
                return out;
            }
        }
    // single-quoted G:A:V notation
    for token in gradle_tokens(line) {
        let parts: Vec<&str> = token.split(':').collect();
        if parts.len() >= 2
            && !parts[0].is_empty()
            && !parts[1].is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ":.-_+".contains(c))
        {
            out.push(DependencyDeclaration {
                path: path.to_string(),
                ecosystem: Ecosystem::Gradle,
                name: format!("{}:{}", parts[0], parts[1]),
                version_req: parts.get(2).map(|v| v.to_string()),
                kind: DeclarationKind::External,
                local_hint: None,
            });
        }
    }
    // map-style: group: 'g', name: 'a', version: 'v'
    if out.is_empty() {
        let group = extract_keyed_value(line, "group");
        let name = extract_keyed_value(line, "name");
        if let (Some(g), Some(a)) = (group, name) {
            out.push(DependencyDeclaration {
                path: path.to_string(),
                ecosystem: Ecosystem::Gradle,
                name: format!("{g}:{a}"),
                version_req: extract_keyed_value(line, "version"),
                kind: DeclarationKind::External,
                local_hint: None,
            });
        }
    }
    out
}

fn gradle_tokens(line: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '\'' || c == '"' {
            chars.next();
            let mut cur = String::new();
            for c2 in chars.by_ref() {
                if c2 == '\'' || c2 == '"' {
                    break;
                }
                cur.push(c2);
            }
            if !cur.trim().is_empty() {
                toks.push(cur.trim().to_string());
            }
        } else {
            chars.next();
        }
        if toks.len() >= 32 {
            break;
        }
    }
    toks
}

fn first_quoted(s: &str) -> Option<String> {
    for q in ['\'', '"'] {
        if let Some(start) = s.find(q)
            && let Some(end) = s[start + 1..].find(q) {
                let v = s[start + 1..start + 1 + end].trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
    }
    None
}

fn extract_keyed_value(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let pos = line.find(&needle)?;
    first_quoted(&line[pos + needle.len()..])
}

// ---------------------------------------------------------------------------
// Python — pyproject.toml / requirements.txt (bounded extraction)
// ---------------------------------------------------------------------------

fn parse_pyproject_toml(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let mut in_project = false;
    let mut in_dependencies_array = false;
    for raw in text.lines().take(20_000) {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && !in_dependencies_array {
            in_project = trimmed == "[project]";
            continue;
        }
        if in_dependencies_array {
            // accumulate entries until closing ']'
            let entry_part = trimmed.trim_end_matches(',').trim();
            let done = entry_part.contains(']');
            let entry = entry_part.trim_end_matches(']').trim().trim_matches(',');
            if !entry.is_empty()
                && let Some(d) = python_req_decl("pyproject.toml", entry) {
                    out.declarations.push(d);
                }
            if done {
                in_dependencies_array = false;
            }
            continue;
        }
        if in_project {
            if let Some(rest) = stripped_key(trimmed, "name") {
                if let Some(v) = first_quoted(rest) {
                    out.provides.push(ProvidedIdentity {
                        ecosystem: Ecosystem::Python,
                        name: pep503_normalize(&v),
                    });
                }
                continue;
            }
            if let Some(rest) = stripped_key(trimmed, "dependencies") {
                let after = rest.trim();
                if after.starts_with('[') {
                    let inner = after.trim_start_matches('[').trim();
                    if inner.contains(']') {
                        let entry = inner.split(']').next().unwrap_or("").trim();
                        if !entry.is_empty()
                            && let Some(d) = python_req_decl("pyproject.toml", entry) {
                                out.declarations.push(d);
                            }
                    } else if !inner.trim_end_matches(',').trim().is_empty() {
                        if let Some(d) =
                            python_req_decl("pyproject.toml", inner.trim_end_matches(','))
                        {
                            out.declarations.push(d);
                        }
                        in_dependencies_array = true;
                    } else {
                        in_dependencies_array = true;
                    }
                }
                continue;
            }
        }
    }
    out
}

fn stripped_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
    Some(rest.trim_start())
}

fn python_req_decl(path: &str, req: &str) -> Option<DependencyDeclaration> {
    let req = req.trim().trim_matches('"').trim_matches('\'').trim();
    if req.is_empty() || req.starts_with('-') || req.starts_with('#') {
        return None;
    }
    // path requirement: ./local or ../local or file:...
    if req.starts_with("./") || req.starts_with("../") || req.starts_with("file:") {
        let raw = req.strip_prefix("file:").unwrap_or(req);
        let hint = normalize_relative(raw);
        if hint.is_empty() {
            return None;
        }
        return Some(DependencyDeclaration {
            path: path.to_string(),
            ecosystem: Ecosystem::Python,
            name: format!("path:{hint}"),
            version_req: None,
            kind: DeclarationKind::LocalPath,
            local_hint: Some(hint),
        });
    }
    let name: String = req
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    if name.is_empty() {
        return None;
    }
    let version = req[name.len()..]
        .trim()
        .trim_start_matches(['=', '<', '>', '~', '!', ';']);
    Some(DependencyDeclaration {
        path: path.to_string(),
        ecosystem: Ecosystem::Python,
        name: pep503_normalize(&name),
        version_req: (!version.is_empty()).then(|| version.to_string()),
        kind: DeclarationKind::External,
        local_hint: None,
    })
}

/// PEP 503 name normalization: lowercase, runs of `-_.` → `-`.
fn pep503_normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !prev_sep {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.extend(c.to_lowercase());
            prev_sep = false;
        }
    }
    out
}

fn parse_requirements_txt(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    for raw in text.lines().take(20_000) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("-r") {
            continue;
        }
        // Strip editable-install flag: "-e ./path" → "./path"
        let effective = line.strip_prefix("-e ").unwrap_or(line);
        if let Some(d) = python_req_decl("requirements.txt", effective) {
            out.declarations.push(d);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Git submodules — .gitmodules
// ---------------------------------------------------------------------------

fn parse_gitmodules(text: &str) -> ManifestParse {
    let mut out = ManifestParse::default();
    let mut current_name: Option<String> = None;
    let mut current_path: Option<String> = None;
    let flush = |out: &mut ManifestParse, name: &Option<String>, path: &Option<String>| {
        if let (Some(n), Some(p)) = (name, path) {
            out.declarations.push(DependencyDeclaration {
                path: ".gitmodules".to_string(),
                ecosystem: Ecosystem::Submodule,
                name: n.clone(),
                version_req: None,
                kind: DeclarationKind::LocalPath,
                local_hint: Some(p.clone()),
            });
        }
    };
    for raw in text.lines().take(5_000) {
        let line = raw.trim();
        if line.starts_with("[submodule") {
            flush(&mut out, &current_name, &current_path);
            current_name = first_quoted(line);
            current_path = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("path") {
            current_path = rest
                .trim_start()
                .strip_prefix('=')
                .map(|v| v.trim().to_string());
            continue;
        }
        let _ = line.strip_prefix("url"); // url recorded but not used for resolution
    }
    flush(&mut out, &current_name, &current_path);
    out
}

// ---------------------------------------------------------------------------
// Generated API / schema — .proto import lines
// ---------------------------------------------------------------------------

/// Extract bounded protobuf import targets from `.proto` files.
/// Specifiers are resolved later against OTHER repositories' layouts.
pub fn parse_proto_imports(rel_path: &str, bytes: &[u8]) -> ManifestParse {
    let mut out = ManifestParse::default();
    if bytes.len() as u64 > limits::MAX_MANIFEST_BYTES {
        out.diagnostics.push(format!("proto_too_large:{rel_path}"));
        return out;
    }
    let text = String::from_utf8_lossy(bytes);
    for raw in text.lines().take(5_000) {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("import ") else {
            continue;
        };
        let rest = rest
            .strip_prefix("public ")
            .or_else(|| rest.strip_prefix("weak "))
            .unwrap_or(rest);
        if let Some(spec) = first_quoted(rest) {
            out.declarations.push(DependencyDeclaration {
                path: rel_path.to_string(),
                ecosystem: Ecosystem::GeneratedApi,
                name: spec,
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            });
        }
    }
    out
}

/// Deduplicate declarations preserving deterministic order.
pub fn dedupe_declarations(decls: &mut Vec<DependencyDeclaration>) {
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    decls.retain(|d| {
        seen.insert((
            d.ecosystem.as_str().to_string(),
            d.name.clone(),
            d.local_hint.clone().unwrap_or_default(),
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_mod_module_and_requires() {
        let p = parse_manifest(
            "go.mod",
            b"module example.com/team/api\n\nrequire (\n\texample.com/team/lib v1.2.3\n\tgithub.com/x/y v0.1.0 // indirect\n)\n\nreplace example.com/team/lib => ./../lib\n",
        );
        assert_eq!(p.provides.len(), 1);
        assert_eq!(p.provides[0].name, "example.com/team/api");
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "example.com/team/lib"
                    && d.kind == DeclarationKind::LocalPath
                    && d.local_hint.as_deref() == Some("../lib"))
        );
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "github.com/x/y" && d.kind == DeclarationKind::External)
        );
    }

    #[test]
    fn package_json_name_deps_and_workspaces() {
        let body = br#"{"name":"@team/svc","dependencies":{"lodash":"^4","./inner":"file:./packages/inner"},"devDependencies":{"jest":"*"},"workspaces":["packages/*"]}"#;
        let p = parse_manifest("package.json", body);
        assert_eq!(p.provides[0].name, "@team/svc");
        assert!(p.declarations.iter().any(|d| d.name == "lodash"));
        assert!(
            p.declarations
                .iter()
                .any(|d| d.kind == DeclarationKind::LocalPath
                    && d.local_hint.as_deref() == Some("packages/inner"))
        );
        assert!(
            p.declarations
                .iter()
                .any(|d| d.kind == DeclarationKind::WorkspaceMember)
        );
    }

    #[test]
    fn pom_project_coords_modules_and_dependencies() {
        let body = b"<?xml version=\"1.0\"?><!-- c --><project xmlns=\"m\"><groupId>com.team</groupId><artifactId>api</artifactId><modules><module>core</module></modules><dependencies><dependency><groupId>com.team</groupId><artifactId>lib</artifactId><version>1.0</version></dependency><dependency><groupId>junit</groupId><artifactId>junit</artifactId></dependency></dependencies></project>";
        let p = parse_manifest("pom.xml", body);
        assert_eq!(
            p.provides.first().map(|x| x.name.as_str()),
            Some("com.team:api")
        );
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "com.team:lib" && d.version_req.as_deref() == Some("1.0"))
        );
        assert!(p.declarations.iter().any(|d| d.name == "junit:junit"));
    }

    #[test]
    fn gradle_project_and_artifact_deps() {
        let body = b"dependencies {\n    implementation project(\":core\")\n    implementation 'commons-io:commons-io:2.11'\n}\n";
        let p = parse_manifest("build.gradle", body);
        assert!(p.declarations.iter().any(|d| d.name == "project::core"
            || d.name == "project:::core"
            || d.local_hint.as_deref() == Some("core")));
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "commons-io:commons-io")
        );
    }

    #[test]
    fn settings_gradle_includes_and_root_name() {
        let body = b"rootProject.name = 'mono'\ninclude ':app', ':lib'\n";
        let p = parse_manifest("settings.gradle", body);
        assert_eq!(p.provides.first().map(|x| x.name.as_str()), Some("mono"));
        assert_eq!(p.declarations.len(), 2);
        assert!(
            p.declarations
                .iter()
                .all(|d| d.kind == DeclarationKind::WorkspaceMember)
        );
    }

    #[test]
    fn pyproject_and_requirements() {
        let py = parse_manifest(
            "pyproject.toml",
            b"[project]\nname = \"My_Lib\"\ndependencies = [\n  \"requests>=2.0\",\n  \"team-lib\",\n]\n",
        );
        assert_eq!(py.provides[0].name, "my-lib");
        assert!(py.declarations.iter().any(|d| d.name == "requests"));
        assert!(py.declarations.iter().any(|d| d.name == "team-lib"));

        let reqs = parse_manifest(
            "requirements.txt",
            b"-e ./shared\nflask~=3.0\n# comment\n\nnumpy\n",
        );
        assert!(
            reqs.declarations
                .iter()
                .any(|d| d.kind == DeclarationKind::LocalPath)
        );
        assert!(reqs.declarations.iter().any(|d| d.name == "flask"));
        assert!(reqs.declarations.iter().any(|d| d.name == "numpy"));
    }

    #[test]
    fn gitmodules_blocks() {
        let body = b"[submodule \"libs/shared\"]\n\tpath = libs/shared\n\turl = https://example.test/shared.git\n[submodule \"tools/gen\"]\n\tpath = tools/gen\n\turl = https://example.test/gen.git\n";
        let p = parse_manifest(".gitmodules", body);
        assert_eq!(p.declarations.len(), 2);
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "libs/shared" && d.local_hint.as_deref() == Some("libs/shared"))
        );
    }

    #[test]
    fn proto_imports_extracted() {
        let p = parse_proto_imports(
            "api/payload.proto",
            b"syntax = \"proto3\";\nimport public \"common/types.proto\";\nimport \"local.proto\";\n",
        );
        assert_eq!(p.declarations.len(), 2);
        assert!(
            p.declarations
                .iter()
                .any(|d| d.name == "common/types.proto")
        );
    }

    #[test]
    fn oversized_manifest_refused() {
        let big = vec![b'a'; (limits::MAX_MANIFEST_BYTES as usize) + 1];
        let p = parse_manifest("go.mod", &big);
        assert!(p.provides.is_empty());
        assert!(
            p.diagnostics
                .iter()
                .any(|d| d.starts_with("manifest_too_large"))
        );
    }

    #[test]
    fn malformed_inputs_degrade_without_panic() {
        for (path, body) in [
            ("go.mod", &b"modul e broken\nrequire (\n"[..]),
            ("package.json", b"{not json"),
            ("pom.xml", b"<project><dependency><groupId>g"),
            ("pyproject.toml", b"[proj\ndef = ["),
            (".gitmodules", b"[submodule \"x\"\npath"),
        ] {
            let _ = parse_manifest(path, body); // must not panic
        }
    }

    #[test]
    fn manifest_path_detection() {
        assert!(is_manifest_path("go.mod"));
        assert!(is_manifest_path("services/api/package.json"));
        assert!(is_manifest_path("modules/core/pom.xml"));
        assert!(!is_manifest_path("src/main.rs"));
        assert!(!is_manifest_path("docs/package-lock.json"));
    }
}
