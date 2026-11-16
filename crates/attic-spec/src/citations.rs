use crate::{Result, SpecError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Kind {
    ExistingRef,
    TargetModify,
    TargetCreateFile,
    TargetCreateSymbol,
    CrossRepoDependency,
    AllowGlob,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingRef => "EXISTING_REF",
            Self::TargetModify => "TARGET_MODIFY",
            Self::TargetCreateFile => "TARGET_CREATE_FILE",
            Self::TargetCreateSymbol => "TARGET_CREATE_SYMBOL",
            Self::CrossRepoDependency => "CROSS_REPO_DEPENDENCY",
            Self::AllowGlob => "ALLOW_GLOB",
        }
    }
    pub fn writes(self) -> bool {
        matches!(
            self,
            Self::TargetModify
                | Self::TargetCreateFile
                | Self::TargetCreateSymbol
                | Self::AllowGlob
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Citation {
    pub kind: Kind,
    pub repository: String,
    pub path: Option<String>,
    pub symbol: Option<String>,
    pub dependency: Option<String>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

// Parse errors are read by agents; spell out the grammar so a single failure is enough to correct the line.
const CITATION_FORMAT_HINT: &str = "expected three or four tokens: <KIND> <repository> <path> [symbol], or CROSS_REPO_DEPENDENCY <consumer-repo> -> <provider-repo>";
const UNKNOWN_KIND_HINT: &str = "unknown citation kind; expected one of EXISTING_REF, TARGET_MODIFY, TARGET_CREATE_FILE, TARGET_CREATE_SYMBOL, CROSS_REPO_DEPENDENCY, ALLOW_GLOB";

pub fn parse(bytes: &[u8]) -> Result<Vec<Citation>> {
    let fail = |line: usize, msg: &str| {
        SpecError::new("ERR_CITATION_PARSE", format!("line {line}: {msg}"))
    };
    if bytes.len() > 1_048_576 {
        return Err(fail(0, "doc too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| fail(0, "document must be UTF-8"))?;
    let mut block = false;
    let mut blocks = 0;
    let mut other_fence: Option<(u8, usize)> = None;
    let mut results = Vec::new();
    let mut seen = BTreeSet::new();
    for (number, line) in text.lines().enumerate() {
        let number = number + 1;
        let trimmed = line.trim();
        if !block {
            let marker = trimmed.as_bytes().first().copied();
            if let Some((ch, len)) = other_fence {
                if marker == Some(ch)
                    && trimmed.bytes().take_while(|c| *c == ch).count() >= len
                    && trimmed.bytes().all(|c| c == ch)
                {
                    other_fence = None;
                }
                continue;
            }
            if matches!(marker, Some(b'`' | b'~')) {
                let ch = marker.unwrap();
                let len = trimmed.bytes().take_while(|c| *c == ch).count();
                if len >= 3 {
                    if &trimmed[len..] == "attic-citations" {
                        blocks += 1;
                        if blocks > 1 {
                            return Err(fail(number, "exactly one attic-citations block required"));
                        }
                        block = true;
                        other_fence = Some((ch, len));
                    } else {
                        other_fence = Some((ch, len));
                    }
                }
            }
            continue;
        }
        if let Some((ch, len)) = other_fence
            && trimmed.bytes().all(|c| c == ch)
            && trimmed.len() >= len
        {
            block = false;
            other_fence = None;
            continue;
        }
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let tokens: Vec<_> = line.split_whitespace().collect();
        if tokens.len() < 3 || tokens.len() > 4 {
            return Err(fail(number, CITATION_FORMAT_HINT));
        }
        let kind = match tokens[0] {
            "EXISTING_REF" => Kind::ExistingRef,
            "TARGET_MODIFY" => Kind::TargetModify,
            "TARGET_CREATE_FILE" => Kind::TargetCreateFile,
            "TARGET_CREATE_SYMBOL" => Kind::TargetCreateSymbol,
            "CROSS_REPO_DEPENDENCY" => Kind::CrossRepoDependency,
            "ALLOW_GLOB" => Kind::AllowGlob,
            _ => return Err(fail(number, UNKNOWN_KIND_HINT)),
        };
        let dependency = kind == Kind::CrossRepoDependency;
        if dependency && (tokens.len() != 4 || tokens[2] != "->") {
            return Err(fail(number, "dependency requires consumer -> provider"));
        }
        if kind == Kind::TargetCreateSymbol && tokens.len() != 4 {
            return Err(fail(number, "TARGET_CREATE_SYMBOL requires a symbol"));
        }
        if kind == Kind::AllowGlob && tokens.len() != 3 {
            return Err(fail(number, "ALLOW_GLOB takes no symbol"));
        }
        if !dependency && !valid_path(tokens[2]) {
            return Err(fail(
                number,
                "path must be repository-relative without . or ..",
            ));
        }
        if kind == Kind::AllowGlob {
            globset::Glob::new(tokens[2]).map_err(|_| fail(number, "invalid glob"))?;
        }
        let citation = Citation {
            kind,
            repository: tokens[1].into(),
            path: (!dependency).then(|| tokens[2].into()),
            symbol: (!dependency)
                .then(|| tokens.get(3).map(|s| s.to_string()))
                .flatten(),
            dependency: dependency.then(|| tokens[3].into()),
        };
        if !seen.insert(citation.clone()) {
            return Err(fail(number, "duplicate citation"));
        }
        results.push(citation);
        if results.len() > 500 {
            return Err(fail(number, "more than 500 citations"));
        }
    }
    if blocks != 1 || block {
        return Err(fail(0, "exactly one closed attic-citations block required"));
    }
    if !results.iter().any(|c| {
        matches!(
            c.kind,
            Kind::TargetModify | Kind::TargetCreateFile | Kind::TargetCreateSymbol
        )
    }) {
        return Err(fail(0, "at least one TARGET_* citation required"));
    }
    Ok(results)
}
