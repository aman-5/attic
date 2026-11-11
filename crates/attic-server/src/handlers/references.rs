use crate::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::fmt;

const DEFAULT_REFERENCE_FILES: usize = 32;
const MAX_REFERENCE_FILES: usize = 64;
const MAX_REFERENCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REFERENCE_NODES: usize = 100_000;
const REFERENCE_BUDGET_MS: u64 = 1_500;

struct JsonHit {
    pointer: String,
    occurrence: usize,
    kind: &'static str,
    match_type: &'static str,
    snippet: String,
    reference_offset: u64,
}

struct JsonScan<'a> {
    target: &'a str,
    offset: u64,
    limit: usize,
    seen: u64,
    nodes: usize,
    occurrences: HashMap<String, usize>,
    hits: Vec<JsonHit>,
    truncated: bool,
    stopped: Option<&'static str>,
    started: std::time::Instant,
}

impl JsonScan<'_> {
    fn enter<E: serde::de::Error>(&mut self, pointer: &str) -> Result<usize, E> {
        self.nodes += 1;
        if self.nodes > MAX_REFERENCE_NODES {
            self.stopped = Some("json_node_budget");
            return Err(E::custom("JSON reference node budget exceeded"));
        }
        if self.started.elapsed() >= Duration::from_millis(REFERENCE_BUDGET_MS) {
            self.stopped = Some("time_budget");
            return Err(E::custom("JSON reference time budget exceeded"));
        }
        if pointer.len() > 4096 {
            self.stopped = Some("json_pointer_too_long");
            return Err(E::custom("JSON pointer exceeds the navigation limit"));
        }
        let count = self.occurrences.entry(pointer.to_owned()).or_default();
        let occurrence = *count;
        *count += 1;
        Ok(occurrence)
    }

    fn record(
        &mut self,
        pointer: &str,
        occurrence: usize,
        kind: &'static str,
        match_type: &'static str,
        text: &str,
    ) {
        self.seen += 1;
        if self.seen <= self.offset {
            return;
        }
        if self.hits.len() >= self.limit {
            self.truncated = true;
            return;
        }
        let start = text.find(self.target).unwrap_or(0).saturating_sub(40);
        let start = crate::handlers::file::floor_char_boundary(text, start);
        let end = crate::handlers::file::floor_char_boundary(text, (start + 240).min(text.len()));
        self.hits.push(JsonHit {
            pointer: pointer.to_owned(),
            occurrence,
            kind,
            match_type,
            snippet: text[start..end].to_owned(),
            reference_offset: self.seen - 1,
        });
    }
}

struct JsonSeed<'a, 'q> {
    state: &'a mut JsonScan<'q>,
    pointer: String,
    occurrence: Option<usize>,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(mut self, deserializer: D) -> Result<(), D::Error> {
        if self.occurrence.is_none() {
            self.occurrence = Some(self.state.enter::<D::Error>(&self.pointer)?);
        }
        deserializer.deserialize_any(JsonVisitor(self))
    }
}

struct JsonVisitor<'a, 'q>(JsonSeed<'a, 'q>);

impl<'de> Visitor<'de> for JsonVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let state = self.0.state;
        while let Some(key) = map.next_key::<String>()? {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            let pointer = format!("{}/{}", self.0.pointer, escaped);
            let occurrence = state.enter::<A::Error>(&pointer)?;
            if key.contains(state.target) {
                let kind = if key == state.target {
                    "definition"
                } else {
                    "uncertain"
                };
                state.record(&pointer, occurrence, kind, "json_key", &key);
            }
            map.next_value_seed(JsonSeed {
                state,
                pointer,
                occurrence: Some(occurrence),
            })?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        let state = self.0.state;
        let mut index = 0usize;
        loop {
            let pointer = format!("{}/{}", self.0.pointer, index);
            if sequence
                .next_element_seed(JsonSeed {
                    state,
                    pointer,
                    occurrence: None,
                })?
                .is_none()
            {
                break;
            }
            index += 1;
        }
        Ok(())
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<(), E> {
        if value.contains(self.0.state.target) {
            let kind = if value == self.0.state.target {
                "reference"
            } else {
                "uncertain"
            };
            self.0.state.record(
                &self.0.pointer,
                self.0
                    .occurrence
                    .expect("JSON value occurrence was assigned"),
                kind,
                "json_string",
                value,
            );
        }
        Ok(())
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<(), E> {
        self.visit_str(&value)
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}

struct JsonReferences {
    hits: Vec<Value>,
    next_offset: Option<u64>,
    incomplete: Vec<String>,
}

struct ReferenceRequest<'a> {
    scope: Option<attic_storage::PathScope>,
    after_path: Option<&'a str>,
    offset: u64,
    start_line: Option<u64>,
    target: &'a str,
}

struct ReferenceBudget {
    files_remaining: usize,
    bytes_remaining: u64,
    started: std::time::Instant,
}

fn scan_json(
    text: &str,
    target: &str,
    offset: u64,
    limit: usize,
    started: std::time::Instant,
    position_basis: &str,
) -> Result<JsonReferences, String> {
    let mut state = JsonScan {
        target,
        offset,
        limit,
        seen: 0,
        nodes: 0,
        occurrences: HashMap::new(),
        hits: Vec::new(),
        truncated: false,
        stopped: None,
        started,
    };
    let occurrence = state
        .enter::<serde_json::Error>("")
        .map_err(|error| error.to_string())?;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parse = JsonSeed {
        state: &mut state,
        pointer: String::new(),
        occurrence: Some(occurrence),
    }
    .deserialize(&mut deserializer)
    .and_then(|_| deserializer.end());
    if let Err(error) = parse
        && state.stopped.is_none()
    {
        return Err(error.to_string());
    }
    let next_offset = state.truncated.then_some(offset + state.hits.len() as u64);
    let mut incomplete = Vec::new();
    if let Some(reason) = state.stopped {
        incomplete.push(reason.to_owned());
    }
    if state.truncated {
        incomplete.push("result_limit".to_owned());
    }
    let wanted = state
        .hits
        .iter()
        .map(|hit| hit.pointer.clone())
        .collect::<HashSet<_>>();
    let locations = if wanted.is_empty() || state.stopped.is_some() {
        HashMap::new()
    } else {
        match attic_analyzers::json::json_spans::locate_all(
            text,
            &wanted,
            started,
            REFERENCE_BUDGET_MS,
            &attic_core::CancellationToken::default(),
        ) {
            Ok(locations) => locations,
            Err(error) => {
                incomplete.push(format!("source_locations_unavailable: {error}"));
                HashMap::new()
            }
        }
    };
    let mut positions = Vec::new();
    for hit in &state.hits {
        if let Some(location) = locations
            .get(&hit.pointer)
            .and_then(|locations| locations.get(hit.occurrence))
        {
            positions.push(location.start);
            positions.push(location.end.saturating_sub(1));
        } else if !incomplete
            .iter()
            .any(|reason| reason.starts_with("source_locations_unavailable"))
        {
            incomplete.push("source_location_missing".into());
        }
    }
    positions.sort_unstable();
    positions.dedup();
    let mut lines = HashMap::new();
    let mut line = 1u64;
    let mut cursor = 0usize;
    for position in positions {
        for byte in &text.as_bytes()[cursor..position] {
            if *byte == b'\n' {
                line += 1;
            }
        }
        lines.insert(position, line);
        cursor = position;
    }
    let hits = state
        .hits
        .into_iter()
        .map(|hit| {
            let occurrences = locations.get(&hit.pointer);
            let location = occurrences.and_then(|items| items.get(hit.occurrence));
            json!({
                "json_pointer": hit.pointer,
                "pointer_ambiguous": occurrences.is_some_and(|items| items.len() > 1),
                "kind": hit.kind,
                "match_type": hit.match_type,
                "resolution": "syntactic",
                "reference_offset": hit.reference_offset,
                "snippet": hit.snippet,
                "start_line": location.and_then(|value| lines.get(&value.start)).copied(),
                "end_line": location.and_then(|value| lines.get(&value.end.saturating_sub(1))).copied(),
                "source_start_byte": location.map(|value| value.start),
                "source_end_byte": location.map(|value| value.end),
                "position_basis": position_basis,
                "location_kind": "enclosing_json_value",
            })
        })
        .collect();
    Ok(JsonReferences {
        hits,
        next_offset,
        incomplete,
    })
}

fn add_issue(issues: &mut Vec<Value>, reason: &str, path: Option<&str>, detail: Option<String>) {
    issues.push(json!({"reason": reason, "path": path, "detail": detail}));
}

fn scan_repository(
    pool: &DbPool,
    repo_id: &str,
    request: &ReferenceRequest<'_>,
    limit: usize,
    budget: &mut ReferenceBudget,
) -> Result<Value, ServerError> {
    let scope = request.scope.as_ref();
    let after_path = request.after_path;
    let offset = request.offset;
    let start_line = request.start_line;
    let target = request.target;
    let files_remaining = &mut budget.files_remaining;
    let bytes_remaining = &mut budget.bytes_remaining;
    let started = budget.started;
    let parsed = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|error| ServerError::InvalidArg(format!("invalid repository_id: {error}")))?;
    let root = pool
        .with_reader(|conn| get_repository_path(conn, &parsed))?
        .ok_or_else(|| ServerError::InvalidArg(format!("repository_id {repo_id} not found")))?;
    let root = PathBuf::from(root);
    let root = root.canonicalize().unwrap_or(root);
    let mut files = pool.with_reader(|conn| {
        attic_storage::reference_files(conn, &parsed, scope, after_path, *files_remaining + 1)
    })?;
    let mut issues = Vec::new();
    let exact = matches!(scope, Some(attic_storage::PathScope::Exact(_)));
    if !exact {
        add_issue(&mut issues, "indexed_files_only", None, None);
    }
    if files.is_empty()
        && after_path.is_none()
        && let Some(attic_storage::PathScope::Exact(path)) = scope
    {
        add_issue(&mut issues, "path_not_indexed", Some(path), None);
        match std::fs::metadata(root.join(path)) {
            Ok(_) => files.push(attic_storage::ReferenceFile {
                path: path.clone(),
                content_hash: String::new(),
                freshness_state: "UNKNOWN".into(),
                existence_state: "present".into(),
            }),
            Err(error) => add_issue(
                &mut issues,
                "path_unreadable",
                Some(path),
                Some(error.to_string()),
            ),
        }
    }
    let has_more = files.len() > *files_remaining;
    files.truncate(*files_remaining);
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    let mut considered = 0usize;
    let mut remaining_unprocessed = has_more;
    let mut last_path = after_path.map(str::to_owned);
    let mut within_file = None;
    let mut verified_exact = false;
    for file in files {
        if started.elapsed() >= Duration::from_millis(REFERENCE_BUDGET_MS) {
            add_issue(&mut issues, "time_budget", Some(&file.path), None);
            remaining_unprocessed = true;
            break;
        }
        if matches.len() >= limit {
            add_issue(&mut issues, "result_limit", Some(&file.path), None);
            remaining_unprocessed = true;
            break;
        }
        if *files_remaining == 0 {
            add_issue(&mut issues, "file_budget", Some(&file.path), None);
            remaining_unprocessed = true;
            break;
        }
        *files_remaining -= 1;
        considered += 1;
        if file.existence_state != "present" {
            add_issue(
                &mut issues,
                "file_not_indexed_as_present",
                Some(&file.path),
                Some(file.existence_state),
            );
            last_path = Some(file.path);
            continue;
        }
        let size = match std::fs::metadata(root.join(&file.path)) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                add_issue(
                    &mut issues,
                    "file_unreadable",
                    Some(&file.path),
                    Some(error.to_string()),
                );
                last_path = Some(file.path);
                continue;
            }
        };
        if size > MAX_REFERENCE_BYTES {
            add_issue(&mut issues, "file_size_limit", Some(&file.path), None);
            last_path = Some(file.path);
            continue;
        }
        if size > *bytes_remaining {
            add_issue(&mut issues, "byte_budget", Some(&file.path), None);
            remaining_unprocessed = true;
            break;
        }
        *bytes_remaining -= size;
        let (path, mut pre) =
            match crate::handlers::file::preprocess_permitted_file(&root, &file.path) {
                Ok(preprocessed) => preprocessed,
                Err(error) => {
                    add_issue(
                        &mut issues,
                        "file_unreadable",
                        Some(&file.path),
                        Some(error.to_string()),
                    );
                    last_path = Some(file.path);
                    continue;
                }
            };
        if pre.decision == SecretScanDecision::Excluded {
            add_issue(&mut issues, "security_excluded", Some(&path), None);
            last_path = Some(path);
            continue;
        }
        if pre.decision != SecretScanDecision::Safe {
            add_issue(
                &mut issues,
                "security_redaction_or_partial_scan",
                Some(&path),
                None,
            );
        }
        if file.freshness_state != "CURRENT" {
            add_issue(
                &mut issues,
                "index_not_current",
                Some(&path),
                Some(file.freshness_state.clone()),
            );
        }
        let remaining = limit - matches.len();
        let mut file_hits = Vec::new();
        let verified;
        if path.to_ascii_lowercase().ends_with(".json") {
            let text = match crate::handlers::file::json_source(&mut pre) {
                Ok(text) => text,
                Err(error) => {
                    add_issue(
                        &mut issues,
                        "json_read_failed",
                        Some(&path),
                        Some(error.to_string()),
                    );
                    last_path = Some(path);
                    continue;
                }
            };
            verified = pre.decision == SecretScanDecision::Safe
                && file.freshness_state == "CURRENT"
                && blake3::hash(text.as_bytes()).to_hex().as_str() == file.content_hash;
            let basis = if pre.decision == SecretScanDecision::Safe {
                "source"
            } else {
                "processed"
            };
            match scan_json(&text, target, offset, remaining, started, basis) {
                Ok(found) => {
                    if start_line.is_some() {
                        return Err(ServerError::InvalidArg(
                            "reference_start_line is only valid for text or malformed JSON".into(),
                        ));
                    }
                    for reason in found.incomplete {
                        add_issue(&mut issues, &reason, Some(&path), None);
                    }
                    if let Some(next_offset) = found.next_offset {
                        within_file = Some(json!({"path": path, "reference_offset": next_offset}));
                    }
                    file_hits = found.hits;
                }
                Err(error) => {
                    add_issue(&mut issues, "malformed_json", Some(&path), Some(error));
                    if offset > 0 {
                        return Err(ServerError::InvalidArg(
                            "reference_offset requires valid JSON".into(),
                        ));
                    }
                    pre.content = Some(text);
                    let result = crate::handlers::file::file_search::search(
                        &mut pre,
                        target,
                        false,
                        crate::handlers::file::FileRegion {
                            start_line,
                            ..Default::default()
                        },
                        remaining,
                    )?;
                    if result["coverage_complete"] != true {
                        add_issue(
                            &mut issues,
                            "file_scan_incomplete",
                            Some(&path),
                            Some(result["incomplete_reasons"].to_string()),
                        );
                    }
                    if let Some(next_line) = result["next_line"].as_u64() {
                        within_file =
                            Some(json!({"path": path, "reference_start_line": next_line}));
                    }
                    for entry in result["matches"].as_array().into_iter().flatten() {
                        let mut entry = entry.clone();
                        entry["kind"] = json!("uncertain");
                        entry["match_type"] = json!("text");
                        entry["resolution"] = json!("syntactic");
                        file_hits.push(entry);
                    }
                }
            }
        } else {
            if offset > 0 {
                return Err(ServerError::InvalidArg(
                    "reference_offset requires valid JSON".into(),
                ));
            }
            let region = crate::handlers::file::FileRegion {
                start_line,
                ..Default::default()
            };
            let result = crate::handlers::file::file_search::search(
                &mut pre, target, false, region, remaining,
            )?;
            verified = file.freshness_state == "CURRENT"
                && result["verified_content_hash"].as_str() == Some(file.content_hash.as_str());
            if result["coverage_complete"] != true {
                add_issue(
                    &mut issues,
                    "file_scan_incomplete",
                    Some(&path),
                    Some(result["incomplete_reasons"].to_string()),
                );
            }
            if let Some(next_line) = result["next_line"].as_u64() {
                within_file = Some(json!({"path": path, "reference_start_line": next_line}));
            }
            let symbol_scope = attic_storage::PathScope::Exact(path.clone());
            let symbols = pool.with_reader(|conn| {
                attic_storage::lookup_symbol_exact_scoped(
                    conn,
                    Some(repo_id),
                    target,
                    64,
                    Some(&symbol_scope),
                )
            })?;
            for entry in result["matches"].as_array().into_iter().flatten() {
                let mut entry = entry.clone();
                let line = entry["line"].as_u64();
                let symbol = symbols.iter().find(|symbol| {
                    let first_line = symbol
                        .span_str
                        .split(':')
                        .next()
                        .and_then(|part| part.parse::<u64>().ok());
                    first_line.and_then(|line| line.checked_add(1)) == line
                });
                entry["kind"] = json!(match symbol {
                    Some(symbol) if symbol.is_definition => "definition",
                    Some(_) => "reference",
                    None => "uncertain",
                });
                entry["match_type"] = json!("text");
                entry["resolution"] = json!("syntactic");
                file_hits.push(entry);
            }
        }
        if !verified {
            add_issue(&mut issues, "content_hash_unverified", Some(&path), None);
        }
        if exact && verified {
            verified_exact = true;
        }
        for mut hit in file_hits {
            hit["repository_id"] = json!(repo_id);
            hit["path"] = json!(path);
            hit["index_freshness"] = json!(file.freshness_state);
            hit["content_hash_verified"] = json!(verified);
            matches.push(hit);
        }
        scanned += 1;
        last_path = Some(path);
        if within_file.is_some() {
            break;
        }
    }
    if has_more {
        add_issue(&mut issues, "file_budget", None, None);
    }
    let complete =
        exact && issues.is_empty() && after_path.is_none() && offset == 0 && start_line.is_none();
    let absence_proven = complete && verified_exact && matches.is_empty();
    Ok(json!({
        "matches": matches,
        "coverage": {
            "complete": complete,
            "absence_proven": absence_proven,
            "scope": if exact { "exact_indexed_file" } else { "indexed_file_page" },
            "files_scanned": scanned,
            "files_considered": considered,
            "incomplete_reasons": issues,
        },
        "continuation": {
            "after_path": if remaining_unprocessed { last_path } else { None },
            "within_file": within_file,
        },
    }))
}

pub(crate) fn handle_references(
    pool: &DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Result<CallToolResult, ServerError> {
    handle_references_with(args, active_ids, |id, request, limit, budget| {
        scan_repository(pool, id, request, limit, budget)
    })
}

pub(crate) fn handle_managed_references(
    server: &AtticServer,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Result<CallToolResult, ServerError> {
    handle_references_with(args, active_ids, |id, request, limit, budget| {
        let local = server.repository_server(id)?;
        scan_repository(&local.pool, id, request, limit, budget)
    })
}

fn handle_references_with(
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    mut scan: impl FnMut(
        &str,
        &ReferenceRequest<'_>,
        usize,
        &mut ReferenceBudget,
    ) -> Result<Value, ServerError>,
) -> Result<CallToolResult, ServerError> {
    let target = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
    validate_filter("query", target, 512)?;
    if target.is_empty() {
        return Err(ServerError::InvalidArg("query must not be empty".into()));
    }
    let scope = parse_path_scope(args)?;
    let repo_id = args.get("repository_id").and_then(Value::as_str);
    if let Some(id) = repo_id {
        validate_repository_id(id)?;
        require_active_member(active_ids, id)?;
    }
    let max_results = crate::handlers::file::parse_u64_arg(args, "max_results")?.unwrap_or(100);
    let max_files = crate::handlers::file::parse_u64_arg(args, "max_files")?
        .unwrap_or(DEFAULT_REFERENCE_FILES as u64);
    let offset = crate::handlers::file::parse_u64_arg(args, "reference_offset")?.unwrap_or(0);
    let start_line = crate::handlers::file::parse_u64_arg(args, "reference_start_line")?;
    let after_path = args
        .get("after_path")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ServerError::InvalidArg("after_path must be a string".into()))
        })
        .transpose()?;
    if !(1..=200).contains(&max_results) || !(1..=MAX_REFERENCE_FILES as u64).contains(&max_files) {
        return Err(ServerError::InvalidArg(
            "max_results must be 1..=200 and max_files 1..=64".into(),
        ));
    }
    if offset > 100_000 {
        return Err(ServerError::InvalidArg(
            "reference_offset must be at most 100000".into(),
        ));
    }
    if (offset > 0 || start_line.is_some())
        && (!matches!(scope, Some(attic_storage::PathScope::Exact(_))) || repo_id.is_none())
    {
        return Err(ServerError::InvalidArg(
            "reference continuation requires one repository and an exact path".into(),
        ));
    }
    if offset > 0 && start_line.is_some() {
        return Err(ServerError::InvalidArg(
            "reference_offset and reference_start_line are mutually exclusive".into(),
        ));
    }
    if start_line == Some(0) {
        return Err(ServerError::InvalidArg(
            "reference_start_line must be at least 1".into(),
        ));
    }
    if after_path.is_some() && repo_id.is_none() {
        return Err(ServerError::InvalidArg(
            "after_path requires repository_id or repository".into(),
        ));
    }
    if let Some(path) = after_path {
        validate_filter("after_path", path, 1024)?;
    }
    if after_path.is_some() && (offset > 0 || start_line.is_some()) {
        return Err(ServerError::InvalidArg(
            "after_path and within-file continuation are mutually exclusive".into(),
        ));
    }
    if matches!(args.get("scope").and_then(Value::as_str), Some("knowledge")) {
        return Err(ServerError::InvalidArg(
            "REFERENCES scans active repositories; knowledge scope is not supported".into(),
        ));
    }
    let mut ids = match repo_id {
        Some(id) => vec![id.to_owned()],
        None => active_ids.iter().cloned().collect::<Vec<_>>(),
    };
    ids.sort();
    let started = std::time::Instant::now();
    let mut budget = ReferenceBudget {
        files_remaining: max_files as usize,
        bytes_remaining: MAX_REFERENCE_BYTES,
        started,
    };
    let request = ReferenceRequest {
        scope,
        after_path,
        offset,
        start_line,
        target,
    };
    let mut matches = Vec::new();
    let mut repositories = Vec::new();
    let mut unavailable = Vec::new();
    for id in ids {
        if matches.len() >= max_results as usize {
            repositories.push(json!({"repository_id": id, "coverage": {
                "complete": false, "absence_proven": false,
                "incomplete_reasons": [{"reason": "result_limit"}]
            }}));
            continue;
        }
        if budget.files_remaining == 0
            || started.elapsed() >= Duration::from_millis(REFERENCE_BUDGET_MS)
        {
            repositories.push(json!({"repository_id": id, "coverage": {
                "complete": false, "absence_proven": false,
                "incomplete_reasons": [{"reason": "workspace_budget"}]
            }}));
            continue;
        }
        let result = match scan(
            &id,
            &request,
            (max_results as usize).saturating_sub(matches.len()),
            &mut budget,
        ) {
            Ok(result) => result,
            Err(error @ ServerError::InvalidArg(_)) => return Err(error),
            Err(error) => {
                unavailable.push(json!({"repository_id": id, "error": error.to_string()}));
                continue;
            }
        };
        matches.extend(result["matches"].as_array().into_iter().flatten().cloned());
        repositories.push(json!({
            "repository_id": id,
            "coverage": result["coverage"],
            "continuation": result["continuation"],
        }));
    }
    let complete = repo_id.is_some()
        && unavailable.is_empty()
        && repositories.len() == 1
        && repositories[0]["coverage"]["complete"] == true;
    let absence_proven =
        complete && matches.is_empty() && repositories[0]["coverage"]["absence_proven"] == true;
    crate::handlers::context::context_response_bounded(
        json!({
            "matches": matches,
            "coverage": {
                "complete": complete,
                "absence_proven": absence_proven,
                "repositories": repositories,
                "unavailable": unavailable,
                "note": "Kinds are syntactic; JSON key definitions and string references do not imply semantic resolution.",
            },
            "telemetry": {"server_processing_ms": started.elapsed().as_secs_f64() * 1000.0},
        }),
        crate::handlers::context::response_limit(args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_json_matches_namespaced_keys_and_string_values() {
        let text = r#"{
  "events": {
    "click": ["awaitFn(decrypt(x), 'custom:decryptSuccess_1', 'custom:decryptError_1')"],
    "custom:decryptSuccess_1": ["handle()"],
    "decryptSuccess_1": "decryptSuccess_1"
  }
}"#;
        let found = scan_json(
            text,
            "decryptSuccess_1",
            0,
            100,
            std::time::Instant::now(),
            "source",
        )
        .expect("valid JSON scans");
        let kinds = found
            .hits
            .iter()
            .map(|hit| {
                (
                    hit["json_pointer"].as_str().unwrap().to_owned(),
                    hit["match_type"].as_str().unwrap().to_owned(),
                    hit["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert!(kinds.contains(&(
            "/events/click/0".into(),
            "json_string".into(),
            "uncertain".into()
        )));
        assert!(kinds.contains(&(
            "/events/custom:decryptSuccess_1".into(),
            "json_key".into(),
            "uncertain".into()
        )));
        assert!(kinds.contains(&(
            "/events/decryptSuccess_1".into(),
            "json_key".into(),
            "definition".into()
        )));
        assert!(kinds.contains(&(
            "/events/decryptSuccess_1".into(),
            "json_string".into(),
            "reference".into()
        )));
        assert_eq!(kinds.len(), 4);
        assert!(found.hits.iter().all(|hit| hit["start_line"].is_u64()));
    }
}
