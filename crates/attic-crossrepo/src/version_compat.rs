//! Version compatibility of resolved cross-repository dependencies
//! (Phase 8).
//!
//! A link says *which* repository a dependency resolves to by name; this
//! module checks whether the provider's declared version satisfies the
//! consumer's requirement. Small, dependency-free evaluators:
//! npm semver ranges, Maven/OSGi version ranges, Python PEP 440
//! specifiers. Anything that cannot be decided (property placeholders,
//! missing versions, Go module requirements, unsupported syntax) is
//! `UNCHECKED` — never guessed.

use std::cmp::Ordering;

use crate::Ecosystem;

/// Outcome stored in `core_relationships.version_compat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    /// The provider's version satisfies the requirement.
    Compatible,
    /// The provider's version is outside the requirement.
    Incompatible,
    /// Could not be decided.
    Unchecked,
}

impl Compat {
    /// DB token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "COMPATIBLE",
            Self::Incompatible => "INCOMPATIBLE",
            Self::Unchecked => "UNCHECKED",
        }
    }
}

/// Version a manifest declares for what it provides. `None` (→ UNCHECKED)
/// when the manifest does not declare its own version (a Maven child POM
/// inheriting `<version>` from its parent) or uses a placeholder.
pub fn provided_version(path: &str, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let file = path.rsplit('/').next().unwrap_or(path);
    let v = match file {
        "package.json" => serde_json::from_str::<serde_json::Value>(text)
            .ok()?
            .get("version")?
            .as_str()?
            .to_owned(),
        "pom.xml" => pom_project_version(text)?,
        "pyproject.toml" => text.lines().find_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("version")?.trim_start().strip_prefix('=')?;
            Some(rest.trim().trim_matches(['"', '\'']).to_owned())
        })?,
        "MANIFEST.MF" => text.lines().find_map(|l| {
            l.strip_prefix("Bundle-Version:")
                .map(|v| v.trim().to_owned())
        })?,
        _ => return None,
    };
    (!v.is_empty() && !v.contains("${")).then_some(v)
}

/// The `<version>` that is a DIRECT child of `<project>` (depth 2), never a
/// parent's, dependency's or plugin's.
fn pom_project_version(text: &str) -> Option<String> {
    let mut depth = 0usize;
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        if let Some(body) = after.strip_prefix("!--") {
            rest = &body[body.find("-->")? + 3..];
            continue;
        }
        let close = after.find('>')?;
        let tag = &after[..close];
        rest = &after[close + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            if name.trim() == "project" && depth <= 1 {
                return None;
            }
            depth = depth.saturating_sub(1);
            continue;
        }
        if tag.ends_with('/') {
            continue;
        }
        depth += 1;
        let name = tag.split_whitespace().next().unwrap_or(tag);
        if depth == 2 && name == "version" {
            let end = rest.find("</version>")?;
            return Some(rest[..end].trim().to_owned());
        }
    }
    None
}

/// Maven treats these qualifiers as the release itself.
fn strip_release_qualifier(v: &str) -> &str {
    for q in [
        ".RELEASE", ".Final", ".FINAL", ".GA", "-RELEASE", "-Final", "-GA",
    ] {
        if let Some(s) = v.strip_suffix(q) {
            return s;
        }
    }
    v
}

/// Does `version` satisfy `requirement` in `ecosystem`?
pub fn evaluate(ecosystem: Ecosystem, requirement: Option<&str>, version: Option<&str>) -> Compat {
    let (Some(req), Some(ver)) = (requirement, version) else {
        return Compat::Unchecked;
    };
    let req = req.trim();
    if req.is_empty() || req.contains("${") || ver.contains("${") {
        return Compat::Unchecked;
    }
    let ok = match ecosystem {
        Ecosystem::Npm => npm_satisfies(req, ver),
        Ecosystem::Maven | Ecosystem::Gradle | Ecosystem::Osgi => maven_satisfies(req, ver),
        Ecosystem::Python => pep440_satisfies(req, ver),
        _ => None,
    };
    match ok {
        Some(true) => Compat::Compatible,
        Some(false) => Compat::Incompatible,
        None => Compat::Unchecked,
    }
}

// ── version comparison ─────────────────────────────────────────────────────

/// Parsed version: numeric release parts + optional pre-release tag
/// (a pre-release sorts before its release).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    parts: Vec<u64>,
    pre: Option<String>,
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches(['v', 'V', '=']).trim();
    let s = s.split('+').next()?;
    let (core, pre) = match s.find(['-']) {
        Some(i) => (&s[..i], Some(s[i + 1..].to_owned())),
        None => (s, None),
    };
    // Maven/OSGi qualifiers: 1.2.3.SNAPSHOT / 1.2.3.qualifier.
    let mut parts = Vec::new();
    let mut pre = pre;
    for (i, p) in core.split('.').enumerate() {
        match p.parse::<u64>() {
            Ok(n) => parts.push(n),
            Err(_) if i > 0 && pre.is_none() => {
                pre = Some(p.to_owned());
                break;
            }
            Err(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Version { parts, pre })
}

fn cmp_versions(a: &Version, b: &Version) -> Ordering {
    let n = a.parts.len().max(b.parts.len());
    for i in 0..n {
        let x = a.parts.get(i).copied().unwrap_or(0);
        let y = b.parts.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    match (&a.pre, &b.pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    }
}

// ── npm semver ─────────────────────────────────────────────────────────────

fn npm_satisfies(range: &str, version: &str) -> Option<bool> {
    let v = parse_version(version)?;
    if matches!(range, "*" | "latest" | "x") {
        return Some(true);
    }
    if range.contains(':') || range.contains('/') {
        return None; // file:, git:, workspace:, tags
    }
    let mut any = false;
    for alt in range.split("||") {
        if npm_set(alt.trim(), &v)? {
            any = true;
        }
    }
    Some(any)
}

fn npm_set(set: &str, v: &Version) -> Option<bool> {
    if let Some((lo, hi)) = set.split_once(" - ") {
        let lo = parse_version(lo)?;
        let hi_v = parse_version(hi)?;
        return Some(
            cmp_versions(v, &lo) != Ordering::Less && cmp_versions(v, &hi_v) != Ordering::Greater,
        );
    }
    for comp in set.split_whitespace() {
        if !npm_comparator(comp, v)? {
            return Some(false);
        }
    }
    Some(true)
}

fn npm_comparator(comp: &str, v: &Version) -> Option<bool> {
    let (op, rest) = ["<=", ">=", "<", ">", "=", "^", "~"]
        .iter()
        .find_map(|op| comp.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("", comp));
    let rest = rest.trim_start_matches('v');
    let pieces: Vec<&str> = rest.split('.').collect();
    let wild = |p: &&str| matches!(*p, "x" | "X" | "*");
    let fixed: Vec<u64> = pieces
        .iter()
        .take_while(|p| !wild(p))
        .map(|p| p.split('-').next().unwrap_or(p).parse::<u64>())
        .collect::<Result<_, _>>()
        .ok()?;
    let partial = fixed.len() < 3 || pieces.iter().any(wild);
    let at = |parts: Vec<u64>| Version { parts, pre: None };
    let base = || -> Option<Version> {
        if partial {
            Some(at(fixed.clone()))
        } else {
            parse_version(rest)
        }
    };
    let ge = |lo: &Version| cmp_versions(v, lo) != Ordering::Less;
    let lt = |hi: &Version| cmp_versions(v, hi) == Ordering::Less;
    Some(match op {
        "^" => {
            let lo = base()?;
            let mut hi = fixed.clone();
            let idx = hi
                .iter()
                .position(|n| *n != 0)
                .unwrap_or(hi.len().saturating_sub(1));
            hi.truncate(idx + 1);
            if let Some(last) = hi.last_mut() {
                *last += 1;
            }
            ge(&lo) && lt(&at(hi))
        }
        "~" => {
            let lo = base()?;
            let mut hi = fixed.clone();
            hi.truncate(if fixed.len() >= 2 { 2 } else { 1 });
            if let Some(last) = hi.last_mut() {
                *last += 1;
            }
            ge(&lo) && lt(&at(hi))
        }
        "" | "=" if partial => {
            if fixed.is_empty() {
                true
            } else {
                let lo = at(fixed.clone());
                let mut hi = fixed.clone();
                *hi.last_mut()? += 1;
                ge(&lo) && lt(&at(hi))
            }
        }
        _ => {
            let b = base()?;
            let o = cmp_versions(v, &b);
            match op {
                "" | "=" => o == Ordering::Equal,
                ">" => o == Ordering::Greater,
                ">=" => o != Ordering::Less,
                "<" => o == Ordering::Less,
                "<=" => o != Ordering::Greater,
                _ => return None,
            }
        }
    })
}

// ── Maven / OSGi ranges ────────────────────────────────────────────────────

fn maven_satisfies(req: &str, version: &str) -> Option<bool> {
    // Qualifiers order differently in Maven and OSGi (and per tool); only
    // release-equivalent ones are decided, anything else is UNCHECKED.
    let v = parse_version(strip_release_qualifier(version.trim()))?;
    if v.pre.is_some() {
        return None;
    }
    let req = req.trim().trim_matches('"');
    if !req.starts_with(['[', '(']) {
        // Soft requirement: the declared version must match.
        let want = parse_version(strip_release_qualifier(req))?;
        if want.pre.is_some() {
            return None;
        }
        return Some(cmp_versions(&v, &want) == Ordering::Equal);
    }
    // Union of ranges: "[1.0,2.0),[3.0,)".
    let mut any = false;
    let mut rest = req;
    while !rest.is_empty() {
        let end = rest.find([']', ')'])?;
        let range = &rest[..=end];
        if maven_range(range, &v)? {
            any = true;
        }
        rest = rest[end + 1..].trim_start_matches(',').trim();
    }
    Some(any)
}

fn maven_range(range: &str, v: &Version) -> Option<bool> {
    let lo_inc = range.starts_with('[');
    let hi_inc = range.ends_with(']');
    let inner = &range[1..range.len() - 1];
    let (lo, hi) = match inner.split_once(',') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (inner.trim(), inner.trim()),
    };
    if !lo.is_empty() {
        let lo = parse_version(lo)?;
        let o = cmp_versions(v, &lo);
        if o == Ordering::Less || (o == Ordering::Equal && !lo_inc) {
            return Some(false);
        }
    }
    if !hi.is_empty() {
        let hi = parse_version(hi)?;
        let o = cmp_versions(v, &hi);
        if o == Ordering::Greater || (o == Ordering::Equal && !hi_inc) {
            return Some(false);
        }
    }
    Some(true)
}

// ── Python PEP 440 ─────────────────────────────────────────────────────────

fn pep440_satisfies(spec: &str, version: &str) -> Option<bool> {
    let v = parse_version(version)?;
    let spec = spec.split(';').next()?.trim(); // drop environment markers
    for clause in spec.split(',') {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        let (op, rest) = ["~=", "==", "!=", ">=", "<=", ">", "<", "==="]
            .iter()
            .find_map(|op| clause.strip_prefix(op).map(|r| (*op, r.trim())))?;
        let ok = match op {
            "~=" => {
                let lo = parse_version(rest)?;
                if lo.parts.len() < 2 {
                    return None;
                }
                let mut hi = lo.parts[..lo.parts.len() - 1].to_vec();
                *hi.last_mut()? += 1;
                cmp_versions(&v, &lo) != Ordering::Less
                    && cmp_versions(
                        &v,
                        &Version {
                            parts: hi,
                            pre: None,
                        },
                    ) == Ordering::Less
            }
            "==" | "!=" if rest.ends_with(".*") => {
                let prefix = parse_version(rest.trim_end_matches(".*"))?;
                let matches = v.parts.starts_with(&prefix.parts);
                if op == "==" { matches } else { !matches }
            }
            "===" => version.trim() == rest,
            _ => {
                let b = parse_version(rest)?;
                let o = cmp_versions(&v, &b);
                match op {
                    "==" => o == Ordering::Equal,
                    "!=" => o != Ordering::Equal,
                    ">=" => o != Ordering::Less,
                    "<=" => o != Ordering::Greater,
                    ">" => o == Ordering::Greater,
                    "<" => o == Ordering::Less,
                    _ => return None,
                }
            }
        };
        if !ok {
            return Some(false);
        }
    }
    Some(true)
}

/// Annotate package-coordinate edges with `version_compat` and
/// `provider_version` in their provenance. `versions` maps a provider
/// repository id to its `(ecosystem, name, version)` triples.
pub fn annotate_edges(
    edges: &mut [crate::resolver::EdgeDraft],
    versions: &std::collections::HashMap<String, Vec<(String, String, String)>>,
) {
    for edge in edges {
        let Ok(mut prov) = serde_json::from_str::<serde_json::Value>(&edge.provenance_json) else {
            continue;
        };
        if prov["kind"] != "package_coordinate" {
            continue;
        }
        let (Some(eco), Some(name)) = (prov["ecosystem"].as_str(), prov["coordinate"].as_str())
        else {
            continue;
        };
        let Ok(ecosystem) = Ecosystem::from_db_str(eco) else {
            continue;
        };
        let provider_version = versions
            .get(&edge.target_repository_id)
            .and_then(|vs| vs.iter().find(|(e, n, _)| e == eco && n == name))
            .map(|(_, _, v)| v.clone());
        let compat = evaluate(
            ecosystem,
            prov["version_req"].as_str(),
            provider_version.as_deref(),
        );
        prov["version_compat"] = serde_json::json!(compat.as_str());
        prov["provider_version"] = serde_json::json!(provider_version);
        edge.provenance_json = prov.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Compat::*;

    fn e(eco: Ecosystem, req: &str, ver: &str) -> Compat {
        evaluate(eco, Some(req), Some(ver))
    }

    #[test]
    fn npm_ranges() {
        for (req, ver, want) in [
            ("^1.2.3", "1.9.0", Compatible),
            ("^1.2.3", "2.0.0", Incompatible),
            ("^0.2.3", "0.2.9", Compatible),
            ("^0.2.3", "0.3.0", Incompatible),
            ("~1.2.3", "1.2.9", Compatible),
            ("~1.2.3", "1.3.0", Incompatible),
            (">=1.0.0 <2.0.0", "1.5.0", Compatible),
            (">=1.0.0 <2.0.0", "2.0.0", Incompatible),
            ("1.x", "1.4.2", Compatible),
            ("1.x", "2.0.0", Incompatible),
            ("1.2.3", "1.2.3", Compatible),
            ("1.0.0 - 2.0.0", "2.0.0", Compatible),
            ("^1.0.0 || ^3.0.0", "3.1.0", Compatible),
            ("*", "9.9.9", Compatible),
            ("workspace:*", "1.0.0", Unchecked),
            ("file:../lib", "1.0.0", Unchecked),
        ] {
            assert_eq!(e(Ecosystem::Npm, req, ver), want, "{req} vs {ver}");
        }
    }

    #[test]
    fn maven_and_osgi_ranges() {
        for (eco, req, ver, want) in [
            (Ecosystem::Maven, "1.0.0", "1.0.0", Compatible),
            (Ecosystem::Maven, "1.0.0", "2.0.0", Incompatible),
            (Ecosystem::Maven, "[1.0,2.0)", "1.5", Compatible),
            (Ecosystem::Maven, "[1.0,2.0)", "2.0", Incompatible),
            (Ecosystem::Maven, "[1.0,)", "7.1", Compatible),
            (Ecosystem::Maven, "(,1.0],[1.2,)", "1.1", Incompatible),
            (Ecosystem::Maven, "${lib.version}", "1.0", Unchecked),
            (Ecosystem::Maven, "[5.3.0,6)", "5.3.0.RELEASE", Compatible),
            (Ecosystem::Maven, "1.0.0", "1.0.0-SNAPSHOT", Unchecked),
            (Ecosystem::Osgi, "[1.2,2)", "1.4.0", Compatible),
            (Ecosystem::Osgi, "[1.2,2)", "1.4.0.qualifier", Unchecked),
            (Ecosystem::Osgi, "[1.2,2)", "2.0.0", Incompatible),
        ] {
            assert_eq!(e(eco, req, ver), want, "{req} vs {ver}");
        }
    }

    #[test]
    fn pep440_specifiers() {
        for (req, ver, want) in [
            (">=1.4,<2", "1.9", Compatible),
            (">=1.4,<2", "2.0", Incompatible),
            ("~=1.4.2", "1.4.9", Compatible),
            ("~=1.4.2", "1.5.0", Incompatible),
            ("==1.4.*", "1.4.7", Compatible),
            ("!=1.4.*", "1.4.7", Incompatible),
            ("==2.0; python_version>'3'", "2.0", Compatible),
        ] {
            assert_eq!(e(Ecosystem::Python, req, ver), want, "{req} vs {ver}");
        }
    }

    #[test]
    fn undecidable_is_unchecked() {
        assert_eq!(
            evaluate(Ecosystem::Go, Some("v1.2.3"), Some("1.2.3")),
            Unchecked
        );
        assert_eq!(evaluate(Ecosystem::Npm, Some("^1.0.0"), None), Unchecked);
        assert_eq!(evaluate(Ecosystem::Npm, None, Some("1.0.0")), Unchecked);
    }

    #[test]
    fn provider_versions_are_read_from_manifests() {
        assert_eq!(
            provided_version("package.json", br#"{"name":"lib","version":"2.1.0"}"#),
            Some("2.1.0".into())
        );
        assert_eq!(
            provided_version(
                "svc/pom.xml",
                b"<project><parent><version>9</version></parent><artifactId>a</artifactId><version>1.4.0</version></project>"
            ),
            Some("1.4.0".into())
        );
        // Child POM inheriting its version: never take a dependency's.
        assert_eq!(
            provided_version(
                "svc/pom.xml",
                b"<?xml version=\"1.0\"?><project><parent><version>9</version></parent><artifactId>a</artifactId><dependencies><dependency><version>3.12.0</version></dependency></dependencies></project>"
            ),
            None
        );
        assert_eq!(
            provided_version(
                "pyproject.toml",
                b"[project]\nname = \"x\"\nversion = \"0.3.1\"\n"
            ),
            Some("0.3.1".into())
        );
        assert_eq!(
            provided_version("META-INF/MANIFEST.MF", b"Bundle-Version: 1.2.0.qualifier\n"),
            Some("1.2.0.qualifier".into())
        );
        assert_eq!(
            provided_version(
                "pom.xml",
                b"<project><version>${revision}</version></project>"
            ),
            None
        );
    }
}
