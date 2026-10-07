use crate::*;

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

    let file_path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("path required".into()))?;

    let region = parse_region(args)?;

    let parsed_repo_id = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;

    let repo_root_str = pool
        .with_reader(|c| get_repository_path(c, &parsed_repo_id))?
        .ok_or_else(|| ServerError::InvalidArg(format!("repository_id {repo_id} not found")))?;
    require_active_member(active_ids, repo_id)?;
    let repo_root_raw = PathBuf::from(&repo_root_str);
    // On Windows, std::fs::canonicalize adds a \\?\ extended-length prefix.
    // canonicalize_within_root canonicalizes the joined path, so the result
    // also has \\?\; but repo_root_raw (from the DB) does not.  Normalize
    // repo_root the same way so that strip_prefix succeeds.
    let repo_root = repo_root_raw.canonicalize().unwrap_or(repo_root_raw);

    let abs_path = canonicalize_within_root(&repo_root.join(file_path), &repo_root)
        .map_err(|e| ServerError::InvalidArg(format!("path rejected: {e}")))?;

    let repo_relative = abs_path
        .strip_prefix(&repo_root)
        .map_err(|_| ServerError::InvalidArg("path outside repo root".into()))?
        .to_string_lossy()
        .replace('\\', "/");

    // Block security-forbidden paths at the server layer regardless of what
    // preprocess_file_content decides, to ensure consistent policy.
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

    // preprocess handles Excluded/Redacted/secrets internally via the secrets scan layer
    let pre = preprocess_file_content(&abs_path, &repo_relative).map_err(ServerError::Discovery)?;

    if pre.decision == SecretScanDecision::Excluded {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "# {repo_relative}\n\n[Excluded by security policy]"
        ))]));
    }
    if pre.decision == SecretScanDecision::PartialScan {
        warn!("file {repo_relative}: partial scan");
    }

    let body: String = if let Some(text) = pre.content {
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

    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
        "{header}{freshness_note}\n{body}"
    ))]))
}
