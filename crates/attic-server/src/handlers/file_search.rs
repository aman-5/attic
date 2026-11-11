use super::*;

const MAX_GREP_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_GREP_SNIPPET_BYTES: usize = 400;
const MAX_GREP_COLUMNS_PER_LINE: usize = 128;

enum Pattern {
    Literal(String),
    Regex(regex::Regex),
}

impl Pattern {
    fn columns(&self, line: &str) -> (Vec<usize>, bool) {
        let mut columns = Vec::new();
        let mut omitted = false;
        let mut add = |column| {
            if columns.len() < MAX_GREP_COLUMNS_PER_LINE {
                columns.push(column);
            } else {
                omitted = true;
            }
        };
        match self {
            Self::Literal(pattern) => {
                for (column, _) in line
                    .match_indices(pattern.as_str())
                    .take(MAX_GREP_COLUMNS_PER_LINE + 1)
                {
                    add(column);
                }
            }
            Self::Regex(pattern) => {
                for found in pattern.find_iter(line).take(MAX_GREP_COLUMNS_PER_LINE + 1) {
                    add(found.start());
                }
            }
        }
        (columns, omitted)
    }
}

struct GrepCollector {
    pattern: Pattern,
    start_line: u64,
    end_line: Option<u64>,
    max_matches: usize,
    line_no: u64,
    scanned: u64,
    pending: String,
    discard_line: bool,
    stopped: bool,
    matches: Vec<Value>,
    reasons: Vec<&'static str>,
    next_line: Option<u64>,
    hasher: blake3::Hasher,
}

impl GrepCollector {
    fn reason(&mut self, reason: &'static str) {
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    fn line(&mut self, line: &str) {
        self.line_no += 1;
        if self.line_no >= self.start_line && self.end_line.is_none_or(|end| self.line_no <= end) {
            let (columns, omitted) = self.pattern.columns(line);
            if let Some(&column) = columns.first() {
                if omitted {
                    self.reason("line_match_limit");
                }
                let start = floor_char_boundary(line, column.saturating_sub(100));
                let end = floor_char_boundary(
                    line,
                    column
                        .saturating_add(MAX_GREP_SNIPPET_BYTES - 100)
                        .min(line.len()),
                );
                self.matches.push(json!({
                    "line": self.line_no,
                    "byte_column": column,
                    "byte_columns": columns,
                    "additional_columns_omitted": omitted,
                    "snippet_start_byte_column": start,
                    "snippet": &line[start..end],
                }));
                if self.matches.len() >= self.max_matches {
                    self.reason("result_limit");
                    self.next_line = Some(self.line_no + 1);
                    self.stopped = true;
                }
            }
        }
        if self.end_line.is_some_and(|end| self.line_no >= end) {
            self.stopped = true;
        }
    }

    fn feed(&mut self, chunk: &str) {
        if self.stopped {
            return;
        }
        let remaining = MAX_STREAM_SCAN_BYTES.saturating_sub(self.scanned) as usize;
        let take = floor_char_boundary(chunk, remaining.min(chunk.len()));
        self.scanned += take as u64;
        self.hasher.update(&chunk.as_bytes()[..take]);
        let mut rest = &chunk[..take];
        while !rest.is_empty() && !self.stopped {
            let newline = rest.find('\n');
            let length = newline.map_or(rest.len(), |position| position + 1);
            let part = &rest[..length];
            rest = &rest[length..];
            if !self.discard_line {
                if self.pending.len().saturating_add(part.len()) > MAX_GREP_LINE_BYTES {
                    self.pending.clear();
                    self.discard_line = true;
                    self.reason("line_too_large");
                } else {
                    self.pending.push_str(part);
                }
            }
            if newline.is_some() {
                if self.discard_line {
                    self.line_no += 1;
                    if self.end_line.is_some_and(|end| self.line_no >= end) {
                        self.stopped = true;
                    }
                } else {
                    let line = std::mem::take(&mut self.pending);
                    self.line(line.trim_end_matches(['\r', '\n']));
                }
                self.discard_line = false;
            }
        }
        if take < chunk.len() && !self.stopped {
            self.reason("scan_limit");
            self.stopped = true;
        }
    }

    fn finish(mut self, decision: SecretScanDecision) -> Value {
        if !self.stopped && !self.discard_line && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line);
        }
        if decision == SecretScanDecision::PartialScan {
            self.reason("partial_security_scan");
        }
        let content_hash =
            if decision == SecretScanDecision::Safe && self.reasons.is_empty() && !self.stopped {
                Some(self.hasher.finalize().to_hex().to_string())
            } else {
                None
            };
        json!({
            "matches": self.matches,
            "coverage_complete": self.reasons.is_empty(),
            "incomplete_reasons": self.reasons,
            "next_line": self.next_line,
            "scope": {"start_line": self.start_line, "end_line": self.end_line},
            "scan_limit_bytes": MAX_STREAM_SCAN_BYTES,
            "verified_content_hash": content_hash,
        })
    }
}

pub(crate) fn search(
    pre: &mut attic_discovery::PreprocessResult,
    pattern: &str,
    use_regex: bool,
    region: FileRegion,
    max_matches: usize,
) -> Result<Value, ServerError> {
    validate_filter("grep", pattern, 512)?;
    if pattern.is_empty() {
        return Err(ServerError::InvalidArg("grep must not be empty".into()));
    }
    if region.start_byte.is_some() || region.end_byte.is_some() {
        return Err(ServerError::InvalidArg(
            "grep supports line ranges, not byte ranges".into(),
        ));
    }
    let pattern = if use_regex {
        Pattern::Regex(
            regex::RegexBuilder::new(pattern)
                .size_limit(1_000_000)
                .dfa_size_limit(2_000_000)
                .build()
                .map_err(|error| ServerError::InvalidArg(format!("invalid grep regex: {error}")))?,
        )
    } else {
        Pattern::Literal(pattern.to_owned())
    };
    let mut collector = GrepCollector {
        pattern,
        start_line: region.start_line.unwrap_or(1).max(1),
        end_line: region.end_line,
        max_matches,
        line_no: 0,
        scanned: 0,
        pending: String::new(),
        discard_line: false,
        stopped: false,
        matches: Vec::new(),
        reasons: Vec::new(),
        next_line: None,
        hasher: blake3::Hasher::new(),
    };
    if let Some(text) = pre.content.take() {
        collector.feed(&text);
    } else if let Some(mut stream) = pre.stream.take() {
        while let Some(chunk) = stream.next_chunk() {
            let chunk = chunk.map_err(ServerError::Discovery)?;
            collector.feed(&chunk.redacted);
            if collector.stopped {
                break;
            }
        }
    } else {
        return Err(ServerError::Retrieval(
            "preprocessed file content is unavailable".into(),
        ));
    }
    Ok(collector.finish(pre.decision))
}
