//! Source-aware JSON value locations for already validated, sanitized text.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::cancellation::CancellationToken;

/// Half-open byte range of one JSON value in the analyzed text.
#[derive(Debug, Clone, Copy)]
pub struct JsonLocation {
    /// Inclusive starting byte.
    pub start: usize,
    /// Exclusive ending byte.
    pub end: usize,
}

pub(super) fn locate(
    text: &str,
    wanted: &HashSet<String>,
    started: Instant,
    max_time_ms: u64,
    cancel: &CancellationToken,
) -> Result<HashMap<String, JsonLocation>, String> {
    Ok(
        locate_impl(text, wanted, started, max_time_ms, cancel, false)?
            .into_iter()
            .filter_map(|(pointer, locations)| {
                locations
                    .last()
                    .copied()
                    .map(|location| (pointer, location))
            })
            .collect(),
    )
}

/// Locate every occurrence of requested pointers, including duplicate object keys.
///
/// The caller must pass the same sanitized JSON text used for any displayed
/// content. Results describe that text, not an unverified live source file.
pub fn locate_all(
    text: &str,
    wanted: &HashSet<String>,
    started: Instant,
    max_time_ms: u64,
    cancel: &CancellationToken,
) -> Result<HashMap<String, Vec<JsonLocation>>, String> {
    locate_impl(text, wanted, started, max_time_ms, cancel, true)
}

fn locate_impl(
    text: &str,
    wanted: &HashSet<String>,
    started: Instant,
    max_time_ms: u64,
    cancel: &CancellationToken,
    all_occurrences: bool,
) -> Result<HashMap<String, Vec<JsonLocation>>, String> {
    let mut scanner = Scanner {
        bytes: text.as_bytes(),
        position: 0,
        wanted,
        locations: HashMap::new(),
        started,
        max_time_ms,
        cancel,
        all_occurrences,
        location_count: 0,
    };
    scanner.value("", 0)?;
    scanner.whitespace();
    if scanner.position != scanner.bytes.len() {
        return Err("unexpected trailing JSON content while locating units".into());
    }
    Ok(scanner.locations)
}

struct Scanner<'a> {
    bytes: &'a [u8],
    position: usize,
    wanted: &'a HashSet<String>,
    locations: HashMap<String, Vec<JsonLocation>>,
    started: Instant,
    max_time_ms: u64,
    cancel: &'a CancellationToken,
    all_occurrences: bool,
    location_count: usize,
}

impl Scanner<'_> {
    fn check_budget(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("JSON location scan cancelled".into());
        }
        if self.max_time_ms > 0 && self.started.elapsed().as_millis() > self.max_time_ms as u128 {
            return Err("JSON location scan exceeded its time budget".into());
        }
        Ok(())
    }

    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> Result<(), String> {
        self.whitespace();
        if self.bytes.get(self.position) != Some(&expected) {
            return Err(format!("invalid JSON token at byte {}", self.position));
        }
        self.position += 1;
        Ok(())
    }

    fn string(&mut self) -> Result<(usize, usize), String> {
        let start = self.position;
        self.consume(b'"')?;
        while let Some(&byte) = self.bytes.get(self.position) {
            if self.position.is_multiple_of(4096) {
                self.check_budget()?;
            }
            self.position += 1;
            match byte {
                b'"' => return Ok((start, self.position)),
                b'\\' => {
                    if self.position >= self.bytes.len() {
                        break;
                    }
                    self.position += 1;
                }
                _ => {}
            }
        }
        Err("unterminated JSON string while locating units".into())
    }

    fn value(&mut self, pointer: &str, depth: usize) -> Result<(), String> {
        self.check_budget()?;
        if depth > 128 {
            return Err("JSON location scan exceeded its recursion limit".into());
        }
        self.whitespace();
        let start = self.position;
        match self.bytes.get(self.position).copied() {
            Some(b'{') => {
                self.position += 1;
                self.whitespace();
                if self.bytes.get(self.position) == Some(&b'}') {
                    self.position += 1;
                } else {
                    loop {
                        self.whitespace();
                        let (key_start, key_end) = self.string()?;
                        let key: String = serde_json::from_slice(&self.bytes[key_start..key_end])
                            .map_err(|error| error.to_string())?;
                        self.consume(b':')?;
                        let escaped = key.replace('~', "~0").replace('/', "~1");
                        self.value(&format!("{pointer}/{escaped}"), depth + 1)?;
                        self.whitespace();
                        match self.bytes.get(self.position) {
                            Some(b',') => self.position += 1,
                            Some(b'}') => {
                                self.position += 1;
                                break;
                            }
                            _ => return Err("invalid JSON object while locating units".into()),
                        }
                    }
                }
            }
            Some(b'[') => {
                self.position += 1;
                self.whitespace();
                if self.bytes.get(self.position) == Some(&b']') {
                    self.position += 1;
                } else {
                    let mut index = 0usize;
                    loop {
                        self.value(&format!("{pointer}/{index}"), depth + 1)?;
                        index += 1;
                        self.whitespace();
                        match self.bytes.get(self.position) {
                            Some(b',') => self.position += 1,
                            Some(b']') => {
                                self.position += 1;
                                break;
                            }
                            _ => return Err("invalid JSON array while locating units".into()),
                        }
                    }
                }
            }
            Some(b'"') => {
                self.string()?;
            }
            Some(_) => {
                while self.bytes.get(self.position).is_some_and(|byte| {
                    !matches!(byte, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t')
                }) {
                    self.position += 1;
                }
                if self.position == start {
                    return Err("invalid JSON value while locating units".into());
                }
            }
            None => return Err("unexpected end of JSON while locating units".into()),
        }
        if self.wanted.contains(pointer) {
            let locations = self.locations.entry(pointer.to_owned()).or_default();
            if !self.all_occurrences {
                locations.clear();
            } else if self.location_count >= 10_000 {
                return Err("JSON location scan exceeded its occurrence budget".into());
            }
            locations.push(JsonLocation {
                start,
                end: self.position,
            });
            self.location_count += 1;
        }
        Ok(())
    }
}
