use crate::*;

#[path = "file_search.rs"]
pub(crate) mod file_search;

// ─── region arguments: checked parsing + validation ────────────────────────────

/// Parsed, validated region request for the `file` tool.  Byte windows take
/// precedence over line windows when both are supplied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FileRegion {
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    pub start_byte: Option<u64>,
    pub end_byte: Option<u64>,
}

/// Parse an optional unsigned integer argument with CHECKED conversion.
///
/// Missing key / explicit null → `None`.  Anything that is not a non-negative
/// integer (negative numbers, floats, strings, values above `u64::MAX`) is a
/// client-visible error — never an `as`-cast truncation.
pub(crate) fn parse_u64_arg(
    args: &HashMap<String, Value>,
    key: &str,
) -> Result<Option<u64>, ServerError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n)),
            None => Err(ServerError::InvalidArg(format!(
                "{key} must be a non-negative integer"
            ))),
        },
    }
}

pub(crate) fn parse_region(args: &HashMap<String, Value>) -> Result<FileRegion, ServerError> {
    let region = FileRegion {
        start_line: parse_u64_arg(args, "start_line")?,
        end_line: parse_u64_arg(args, "end_line")?,
        start_byte: parse_u64_arg(args, "start_byte")?,
        end_byte: parse_u64_arg(args, "end_byte")?,
    };

    for (name, v) in [
        ("start_line", region.start_line),
        ("end_line", region.end_line),
        ("start_byte", region.start_byte),
        ("end_byte", region.end_byte),
    ] {
        if let Some(v) = v
            && v > MAX_REGION_VALUE
        {
            return Err(ServerError::InvalidArg(format!(
                "{name} exceeds the maximum allowed value ({MAX_REGION_VALUE})"
            )));
        }
    }

    if let (Some(s), Some(e)) = (region.start_byte, region.end_byte) {
        if e < s {
            return Err(ServerError::InvalidArg(
                "end_byte must be greater than or equal to start_byte".into(),
            ));
        }
        if e - s > MAX_BYTE_SPAN {
            return Err(ServerError::InvalidArg(format!(
                "byte region too large (max {MAX_BYTE_SPAN} bytes per request)"
            )));
        }
    }
    if let (Some(s), Some(e)) = (region.start_line, region.end_line) {
        if e < s {
            return Err(ServerError::InvalidArg(
                "end_line must be greater than or equal to start_line".into(),
            ));
        }
        // Inclusive line window covers e - s + 1 lines.
        if e - s + 1 > MAX_LINE_SPAN {
            return Err(ServerError::InvalidArg(format!(
                "line region too large (max {MAX_LINE_SPAN} lines per request)"
            )));
        }
    }
    Ok(region)
}

// ─── UTF-8-safe slicing primitives ─────────────────────────────────────────────

/// Largest index `i <= pos` that is a char boundary of `s`.
///
/// Deterministic byte-region semantics: a user-supplied byte offset that does
/// NOT fall on a UTF-8 character boundary is floored DOWN to the nearest
/// character boundary (the partially-addressed character is included for a
/// start offset and excluded by an end offset).  Offsets past the end of the
/// string clamp to the end.  This function never panics.
pub(crate) fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Slice `s[start..end]` with UTF-8-flooring on both offsets.  Returns an
/// empty slice whenever the floored end does not exceed the floored start.
pub(crate) fn slice_utf8_safe(s: &str, start: usize, end: usize) -> &str {
    let e = floor_char_boundary(s, end);
    let b = floor_char_boundary(s, start).min(e);
    &s[b..e]
}

// ─── region application on in-memory text ──────────────────────────────────────

pub(crate) fn apply_region_bounds(
    text: &str,
    region: FileRegion,
) -> Result<Cow<'_, str>, ServerError> {
    if region.start_byte.is_some() || region.end_byte.is_some() {
        let len_usize = text.len();
        let s = usize::try_from(region.start_byte.unwrap_or(0))
            .unwrap_or(len_usize)
            .min(len_usize);
        let e = region
            .end_byte
            .map(|v| usize::try_from(v).unwrap_or(len_usize))
            .unwrap_or(len_usize)
            .min(len_usize);
        return Ok(Cow::Owned(slice_utf8_safe(text, s, e).to_owned()));
    }
    if region.start_line.is_some() || region.end_line.is_some() {
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len() as u64;
        let sl = region.start_line.unwrap_or(1).saturating_sub(1).min(total) as usize;
        let el = region.end_line.map(|e| e.min(total)).unwrap_or(total) as usize;
        if sl >= el {
            return Ok(Cow::Owned(String::new()));
        }
        return Ok(Cow::Owned(lines[sl..el].join("\n")));
    }
    Ok(Cow::Borrowed(text))
}

// ─── bounded streaming for LARGE files ────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum WindowSpec {
    /// Whole (redacted) stream, subject only to the response cap.
    All,
    /// Byte window `[start, end)` over the concatenated redacted stream.
    Bytes { start: u64, end: u64 },
    /// Inclusive 1-based line window `[start, end]` over the stream.
    Lines { start: u64, end: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopState {
    Running,
    WindowSatisfied,
    CapReached,
    ScanBoundReached,
}

/// Incrementally assembles the requested window from a LARGE file's sanitized
/// chunk stream.  At most `MAX_RESPONSE_BYTES` (+ one trailing marker) of
/// content is ever retained; the full file is NEVER accumulated.
pub(crate) struct StreamWindowCollector {
    spec: WindowSpec,
    pub(crate) out: String,
    produced: u64,
    line_no: u64,
    pending_line: String,
    state: StopState,
}

impl StreamWindowCollector {
    pub(crate) fn new(spec: WindowSpec) -> Self {
        Self {
            spec,
            out: String::new(),
            produced: 0,
            line_no: 0,
            pending_line: String::new(),
            state: StopState::Running,
        }
    }

    /// Feed one sanitized chunk.  Returns `false` when the caller may stop
    /// pulling further chunks (window complete or a limit reached).
    pub(crate) fn feed(&mut self, chunk: &str) -> bool {
        if self.state != StopState::Running {
            return false;
        }

        let chunk_len = chunk.len() as u64;
        let chunk_start = self.produced;
        let chunk_end = chunk_start + chunk_len;
        self.produced = chunk_end;

        match self.spec {
            WindowSpec::Bytes { start, end } => {
                if chunk_end > start && chunk_start < end {
                    let local_s = start.saturating_sub(chunk_start).min(chunk_len) as usize;
                    let local_e = end.saturating_sub(chunk_start).min(chunk_len) as usize;
                    let piece = slice_utf8_safe(chunk, local_s, local_e.max(local_s));
                    self.push_bounded(piece);
                }
                if self.produced >= end && self.state == StopState::Running {
                    self.state = StopState::WindowSatisfied;
                }
            }
            WindowSpec::Lines { start, end } => {
                let mut rest = chunk;
                let mut carry = std::mem::take(&mut self.pending_line);
                while let Some(nl) = rest.find('\n') {
                    carry.push_str(&rest[..=nl]);
                    self.line_no += 1;
                    if self.line_no >= start && self.line_no <= end {
                        self.push_bounded(&carry);
                    }
                    carry.clear();
                    rest = &rest[nl + 1..];
                    if self.line_no >= end {
                        break;
                    }
                }
                // Whatever remains has no newline yet — buffer for the next chunk.
                carry.push_str(rest);
                if carry.len() > MAX_RESPONSE_BYTES * 2 {
                    // Pathological single-line input: keep memory bounded.
                    carry.truncate(MAX_RESPONSE_BYTES * 2);
                }
                self.pending_line = carry;
                if self.line_no >= end && self.state == StopState::Running {
                    self.state = StopState::WindowSatisfied;
                }
            }
            WindowSpec::All => {
                self.push_bounded(chunk);
            }
        }

        if self.out.len() >= MAX_RESPONSE_BYTES {
            self.state = StopState::CapReached;
        } else if self.produced >= MAX_STREAM_SCAN_BYTES {
            self.state = StopState::ScanBoundReached;
        }

        self.state == StopState::Running
    }

    /// Append at most enough of `piece` to stay under the response cap,
    /// refusing to split a UTF-8 character at the cut point.
    fn push_bounded(&mut self, piece: &str) {
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(self.out.len());
        if remaining == 0 {
            return;
        }
        if piece.len() <= remaining {
            self.out.push_str(piece);
            return;
        }
        let mut take = remaining;
        while take > 0 && !piece.is_char_boundary(take) {
            take -= 1;
        }
        self.out.push_str(&piece[..take]);
    }

    /// Finish the stream and produce the final response body.
    pub(crate) fn finish(mut self) -> String {
        if self.spec_matches_lines() && !self.pending_line.is_empty() {
            // Final unterminated line.
            self.line_no += 1;
            let last = std::mem::take(&mut self.pending_line);
            if let WindowSpec::Lines { start, end } = self.spec
                && self.line_no >= start
                && self.line_no <= end
            {
                self.push_bounded(&last);
            }
        }
        if matches!(self.spec, WindowSpec::Lines { .. })
            && let Some(stripped) = self.out.strip_suffix('\n')
        {
            self.out = stripped.to_owned();
        }
        match self.state {
            StopState::CapReached => self
                .out
                .push_str("\n\n[truncated: response exceeded the server output limit]"),
            StopState::ScanBoundReached => self.out.push_str(
                "\n\n[truncated: file exceeds the maximum scannable size for one response]",
            ),
            _ => {}
        }
        self.out
    }

    fn spec_matches_lines(&self) -> bool {
        matches!(self.spec, WindowSpec::Lines { .. })
    }
}

/// Consume a LARGE file's sanitized chunk stream and assemble ONLY the
/// requested window, enforcing every output bound.  The complete file is
/// never accumulated in memory.
pub(crate) fn stream_window_from_large_file(
    stream: &mut attic_discovery::LargeFileStream,
    region: FileRegion,
) -> Result<String, ServerError> {
    let spec = if region.start_byte.is_some() || region.end_byte.is_some() {
        WindowSpec::Bytes {
            start: region.start_byte.unwrap_or(0),
            end: region.end_byte.unwrap_or(u64::MAX),
        }
    } else if region.start_line.is_some() || region.end_line.is_some() {
        WindowSpec::Lines {
            start: region.start_line.unwrap_or(1),
            end: region.end_line.unwrap_or(u64::MAX),
        }
    } else {
        WindowSpec::All
    };

    let mut collector = StreamWindowCollector::new(spec);
    'pull: while let Some(chunk_result) = stream.next_chunk() {
        let chunk = chunk_result.map_err(ServerError::Discovery)?;
        if !collector.feed(&chunk.redacted) {
            break 'pull;
        }
    }
    Ok(collector.finish())
}

// ─── response-size enforcement for non-streamed content ───────────────────────

pub(crate) fn enforce_response_limit(mut body: String) -> String {
    if body.len() <= MAX_RESPONSE_BYTES {
        return body;
    }
    let mut cut = MAX_RESPONSE_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str("\n\n[truncated: response exceeded the server output limit]");
    body
}

const MAX_JSON_NAVIGATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_OUTLINE_ENTRIES: usize = 200;

fn optional_string<'a>(
    args: &'a HashMap<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, ServerError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ServerError::InvalidArg(format!("{name} must be a string"))),
    }
}

fn optional_bool(args: &HashMap<String, Value>, name: &str) -> Result<bool, ServerError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ServerError::InvalidArg(format!("{name} must be a boolean"))),
    }
}

fn validate_json_pointer(pointer: &str) -> Result<(), ServerError> {
    if pointer.len() > 4096 || (!pointer.is_empty() && !pointer.starts_with('/')) {
        return Err(ServerError::InvalidArg(
            "json_pointer must be an RFC 6901 pointer of at most 4096 bytes".into(),
        ));
    }
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return Err(ServerError::InvalidArg(
                "json_pointer contains an invalid ~ escape".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn json_source(
    pre: &mut attic_discovery::PreprocessResult,
) -> Result<String, ServerError> {
    if let Some(text) = pre.content.take() {
        if text.len() > MAX_JSON_NAVIGATION_BYTES {
            return Err(ServerError::InvalidArg(format!(
                "JSON navigation requires a complete file of at most {MAX_JSON_NAVIGATION_BYTES} bytes"
            )));
        }
        return Ok(text);
    }
    let mut stream = pre
        .stream
        .take()
        .ok_or_else(|| ServerError::Retrieval("preprocessed file content is unavailable".into()))?;
    let mut text = String::new();
    while let Some(chunk) = stream.next_chunk() {
        let chunk = chunk.map_err(ServerError::Discovery)?;
        if text.len().saturating_add(chunk.redacted.len()) > MAX_JSON_NAVIGATION_BYTES {
            return Err(ServerError::InvalidArg(format!(
                "JSON navigation requires a complete file of at most {MAX_JSON_NAVIGATION_BYTES} bytes"
            )));
        }
        text.push_str(&chunk.redacted);
    }
    Ok(text)
}

pub(crate) fn preprocess_permitted_file(
    repo_root: &Path,
    file_path: &str,
) -> Result<(String, attic_discovery::PreprocessResult), ServerError> {
    let abs_path = canonicalize_within_root(&repo_root.join(file_path), repo_root)
        .map_err(|error| ServerError::InvalidArg(format!("path rejected: {error}")))?;
    let repo_relative = abs_path
        .strip_prefix(repo_root)
        .map_err(|_| ServerError::InvalidArg("path outside repo root".into()))?
        .to_string_lossy()
        .replace('\\', "/");
    if is_security_forbidden(&repo_relative) {
        let detail = if repo_relative.eq_ignore_ascii_case(".git")
            || repo_relative
                .split('/')
                .any(|component| component.eq_ignore_ascii_case(".git"))
        {
            ".git internals are forbidden"
        } else {
            "security-forbidden content is not readable"
        };
        return Err(ServerError::InvalidArg(format!("path rejected: {detail}")));
    }
    let pre = preprocess_file_content(&abs_path, &repo_relative).map_err(ServerError::Discovery)?;
    Ok((repo_relative, pre))
}

fn add_outline_entries(
    value: &Value,
    pointer: &str,
    depth: u64,
    max_depth: u64,
    entries: &mut Vec<Value>,
    truncated: &mut bool,
) {
    if entries.len() >= MAX_OUTLINE_ENTRIES || pointer.len() > 4096 {
        *truncated = true;
        return;
    }
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    entries.push(json!({"json_pointer": pointer, "type": kind}));
    if depth >= max_depth {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                add_outline_entries(
                    child,
                    &format!("{pointer}/{escaped}"),
                    depth + 1,
                    max_depth,
                    entries,
                    truncated,
                );
                if *truncated {
                    return;
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                add_outline_entries(
                    child,
                    &format!("{pointer}/{index}"),
                    depth + 1,
                    max_depth,
                    entries,
                    truncated,
                );
                if *truncated {
                    return;
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_file(
    pool: &DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Result<CallToolResult, ServerError> {
    let repo_id = args
        .get("repository_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;

    let requested_path = optional_string(args, "path")?;
    let short_unit_id = optional_string(args, "unit_id")?;
    let retrieval_unit_id = optional_string(args, "retrieval_unit_id")?;
    if short_unit_id.is_some() && retrieval_unit_id.is_some() {
        return Err(ServerError::InvalidArg(
            "unit_id and retrieval_unit_id are mutually exclusive".into(),
        ));
    }
    let unit_id = retrieval_unit_id.or(short_unit_id);
    if requested_path.is_none() && unit_id.is_none() {
        return Err(ServerError::InvalidArg(
            "path required unless unit_id is supplied".into(),
        ));
    }
    let requested_pointer = optional_string(args, "json_pointer")?;
    let grep = optional_string(args, "grep")?;
    let grep_regex = optional_bool(args, "regex")?;
    let parent = optional_bool(args, "parent")?;
    let outline = optional_bool(args, "outline")?;
    if grep.is_some() && (unit_id.is_some() || requested_pointer.is_some() || parent || outline) {
        return Err(ServerError::InvalidArg(
            "grep cannot be combined with unit or JSON navigation".into(),
        ));
    }
    if grep.is_none() && (args.contains_key("regex") || args.contains_key("max_matches")) {
        return Err(ServerError::InvalidArg(
            "regex and max_matches require grep".into(),
        ));
    }
    let max_matches = parse_u64_arg(args, "max_matches")?.unwrap_or(50);
    if !(1..=200).contains(&max_matches) {
        return Err(ServerError::InvalidArg(
            "max_matches must be 1..=200".into(),
        ));
    }
    if unit_id.is_some() && requested_pointer.is_some() {
        return Err(ServerError::InvalidArg(
            "unit_id and json_pointer are mutually exclusive".into(),
        ));
    }
    let outline_depth = parse_u64_arg(args, "outline_depth")?.unwrap_or(2);
    if !(1..=5).contains(&outline_depth) || (!outline && args.contains_key("outline_depth")) {
        return Err(ServerError::InvalidArg(
            "outline_depth requires outline=true and must be 1..=5".into(),
        ));
    }
    let mut region = parse_region(args)?;
    if (unit_id.is_some() || requested_pointer.is_some() || outline || parent)
        && region != FileRegion::default()
    {
        return Err(ServerError::InvalidArg(
            "unit and JSON navigation cannot be combined with line or byte regions".into(),
        ));
    }

    let parsed_repo_id = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;

    let repo_root_str = pool
        .with_reader(|c| get_repository_path(c, &parsed_repo_id))?
        .ok_or_else(|| ServerError::InvalidArg(format!("repository_id {repo_id} not found")))?;
    require_active_member(active_ids, repo_id)?;
    let unit = if let Some(id) = unit_id {
        id.parse::<attic_core::RetrievalUnitId>()
            .map_err(|error| ServerError::InvalidArg(format!("invalid unit_id: {error}")))?;
        Some(
            pool.with_reader(|conn| attic_storage::retrieval_unit_location(conn, repo_id, id))?
                .ok_or_else(|| {
                    ServerError::InvalidArg(
                        "unit_id not found in the active repository or no longer readable".into(),
                    )
                })?,
        )
    } else {
        None
    };
    let file_path = match (requested_path, unit.as_ref()) {
        (Some(path), Some(unit)) if path.replace('\\', "/") != unit.path => {
            return Err(ServerError::InvalidArg(
                "path does not match the requested unit".into(),
            ));
        }
        (Some(path), _) => path,
        (None, Some(unit)) => unit.path.as_str(),
        (None, None) => unreachable!("path or unit_id validated above"),
    };
    let mut pointer = requested_pointer.map(str::to_owned);
    let mut group_range = None;
    if let Some(unit) = &unit {
        if unit.analyzer_id == "json" {
            let metadata: Value = unit
                .occurrence_metadata
                .as_deref()
                .ok_or_else(|| ServerError::Retrieval("JSON unit lacks pointer metadata".into()))
                .and_then(|value| {
                    serde_json::from_str(value).map_err(|error| {
                        ServerError::Retrieval(format!("invalid JSON unit metadata: {error}"))
                    })
                })?;
            pointer = Some(
                metadata
                    .get("json_pointer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ServerError::Retrieval("JSON unit lacks pointer metadata".into())
                    })?
                    .to_owned(),
            );
            if metadata.get("array_start").is_some() || metadata.get("array_end").is_some() {
                let start = metadata["array_start"]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok());
                let end = metadata["array_end"]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok());
                match (start, end) {
                    (Some(start), Some(end)) if start < end => group_range = Some((start, end)),
                    _ => {
                        return Err(ServerError::Retrieval(
                            "invalid indexed JSON array group range".into(),
                        ));
                    }
                }
            }
        } else if !outline && !parent {
            region.start_line = unit
                .start_line
                .and_then(|line| line.checked_add(1))
                .map(u64::from);
            region.end_line = unit.end_line.map(u64::from);
            if region.start_line.is_none() || region.end_line.is_none() {
                return Err(ServerError::Retrieval(
                    "unit has no usable indexed line range".into(),
                ));
            }
        }
    }
    if let Some(pointer) = &pointer {
        validate_json_pointer(pointer)?;
    }
    if parent {
        let current = pointer.as_deref().ok_or_else(|| {
            ServerError::InvalidArg("parent requires a JSON pointer or unit".into())
        })?;
        let index = current
            .rfind('/')
            .ok_or_else(|| ServerError::InvalidArg("the JSON root has no parent".into()))?;
        pointer = Some(current[..index].to_owned());
        group_range = None;
    }
    if outline && pointer.is_none() {
        pointer = Some(String::new());
    }
    let repo_root_raw = PathBuf::from(&repo_root_str);
    // On Windows, std::fs::canonicalize adds a \\?\ extended-length prefix.
    // canonicalize_within_root canonicalizes the joined path, so the result
    // also has \\?\; but repo_root_raw (from the DB) does not.  Normalize
    // repo_root the same way so that strip_prefix succeeds.
    let repo_root = repo_root_raw.canonicalize().unwrap_or(repo_root_raw);

    let (repo_relative, mut pre) = preprocess_permitted_file(&repo_root, file_path)?;

    if pre.decision == SecretScanDecision::Excluded {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "# {repo_relative}\n\n[Excluded by security policy]"
        ))]));
    }
    if pre.decision == SecretScanDecision::PartialScan {
        warn!("file {repo_relative}: partial scan");
    }

    let body: String = if let Some(pattern) = grep {
        serde_json::to_string_pretty(&file_search::search(
            &mut pre,
            pattern,
            grep_regex,
            region,
            max_matches as usize,
        )?)?
    } else if let Some(pointer) = &pointer {
        let text = json_source(&mut pre)?;
        let document: Value = serde_json::from_str(&text)
            .map_err(|error| ServerError::InvalidArg(format!("file is not valid JSON: {error}")))?;
        let selected = document.pointer(pointer).ok_or_else(|| {
            ServerError::InvalidArg(format!(
                "JSON pointer {} does not exist in the live file",
                serde_json::to_string(pointer).unwrap_or_default()
            ))
        })?;
        let group;
        let selected = if let Some((start, end)) = group_range {
            let array = selected.as_array().ok_or_else(|| {
                ServerError::Retrieval(
                    "indexed JSON array group no longer points to an array".into(),
                )
            })?;
            let items = array.get(start..end).ok_or_else(|| {
                ServerError::Retrieval(
                    "indexed JSON array group no longer exists in the live file".into(),
                )
            })?;
            group = Value::Array(items.to_vec());
            &group
        } else {
            selected
        };
        if outline {
            let mut entries = Vec::new();
            let mut truncated = false;
            if let Some((start, _)) = group_range {
                entries.push(json!({"json_pointer": pointer, "type": "array"}));
                if outline_depth > 0 {
                    for (index, item) in selected.as_array().into_iter().flatten().enumerate() {
                        add_outline_entries(
                            item,
                            &format!("{pointer}/{}", start + index),
                            1,
                            outline_depth,
                            &mut entries,
                            &mut truncated,
                        );
                        if truncated {
                            break;
                        }
                    }
                }
            } else {
                add_outline_entries(
                    selected,
                    pointer,
                    0,
                    outline_depth,
                    &mut entries,
                    &mut truncated,
                );
            }
            let mut bounded = Vec::new();
            let mut encoded_bytes = 0usize;
            for entry in entries {
                let size = serde_json::to_vec(&entry)?.len() + 1;
                if encoded_bytes.saturating_add(size) > MAX_RESPONSE_BYTES - 4096 {
                    truncated = true;
                    break;
                }
                encoded_bytes += size;
                bounded.push(entry);
            }
            serde_json::to_string_pretty(&json!({
                "entries": bounded,
                "truncated": truncated,
                "max_depth": outline_depth,
            }))?
        } else {
            let encoded = serde_json::to_string_pretty(selected)?;
            if encoded.len() > MAX_RESPONSE_BYTES {
                return Err(ServerError::InvalidArg(format!(
                    "selected JSON value exceeds {MAX_RESPONSE_BYTES} bytes; use outline or a more specific json_pointer"
                )));
            }
            encoded
        }
    } else if let Some(text) = pre.content {
        // SMALL / Redacted / PartialScan content held fully in memory already.
        let bounded = apply_region_bounds(&text, region)?;
        enforce_response_limit(bounded.into_owned())
    } else if let Some(mut stream) = pre.stream {
        // LARGE file: genuinely bounded incremental retrieval.  Sanitized
        // chunks are streamed through the window collector; the whole file is
        // never accumulated.
        stream_window_from_large_file(&mut stream, region)?
    } else {
        String::new()
    };

    let header = match pre.decision {
        SecretScanDecision::Redacted => format!("# {repo_relative}\n# [Secrets redacted]\n\n"),
        SecretScanDecision::PartialScan => format!("# {repo_relative}\n# [Partial scan]\n\n"),
        _ => format!("# {repo_relative}\n\n"),
    };

    // Phase 2: never present stale indexed state as CURRENT.  The body is
    // read live from disk, but if the latest occurrence for this path is not
    // CURRENT the response says so explicitly.
    let freshness_note = pool
        .with_reader(|c| {
            attic_storage::lookup_occurrence_snapshot(c, &parsed_repo_id, &repo_relative)
        })?
        .map(|s| s.freshness_state)
        .filter(|f| f != "CURRENT")
        .map(|f| format!("# [index freshness: {f}]\n"))
        .unwrap_or_default();

    let navigation_note = if let Some(pointer) = &pointer {
        format!(
            "# [live JSON pointer: {}{}{}]\n",
            serde_json::to_string(pointer)?,
            group_range
                .map(|(start, end)| format!("; array elements {start}..{end}"))
                .unwrap_or_default(),
            if unit_id.is_some() {
                "; indexed selection may have changed"
            } else {
                ""
            }
        )
    } else if unit_id.is_some() {
        "# [unit line range is indexed; content is live and may have changed]\n".to_string()
    } else {
        String::new()
    };
    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
        "{header}{freshness_note}{navigation_note}\n{body}"
    ))]))
}
