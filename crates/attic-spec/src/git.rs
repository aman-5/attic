use crate::{Result, SpecError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub orig_path: Option<String>,
    pub added: u64,
    pub deleted: u64,
    pub binary: bool,
}

fn fail() -> SpecError {
    SpecError::new(
        "ERR_GIT_OUTPUT",
        "malformed, truncated or non-UTF8 Git NUL stream",
    )
}
fn text(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(String::from)
        .map_err(|_| fail())
}
fn tokens(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err(fail());
    }
    let mut parts: Vec<_> = bytes.split(|b| *b == 0).collect();
    parts.pop();
    Ok(parts)
}
pub fn parse_status(bytes: &[u8]) -> Result<Vec<Change>> {
    let parts = tokens(bytes)?;
    let mut i = 0;
    let mut out = Vec::new();
    while i < parts.len() {
        let token = parts[i];
        i += 1;
        if token.len() < 4 || token[2] != b' ' {
            return Err(fail());
        }
        let rename = token[..2].iter().any(|b| matches!(*b, b'R' | b'C'));
        let orig_path = if rename {
            let old = parts.get(i).ok_or_else(fail)?;
            i += 1;
            if old.is_empty() {
                return Err(fail());
            }
            Some(text(old)?)
        } else {
            None
        };
        out.push(Change {
            path: text(&token[3..])?,
            orig_path,
            added: 0,
            deleted: 0,
            binary: false,
        });
    }
    Ok(out)
}
pub fn parse_numstat(bytes: &[u8]) -> Result<Vec<Change>> {
    let parts = tokens(bytes)?;
    let mut i = 0;
    let mut out = Vec::new();
    while i < parts.len() {
        let mut fields = parts[i].splitn(3, |b| *b == b'\t');
        i += 1;
        let added = fields.next().ok_or_else(fail)?;
        let deleted = fields.next().ok_or_else(fail)?;
        let raw_path = fields.next().ok_or_else(fail)?;
        let binary = added == b"-" && deleted == b"-";
        let parse = |bytes: &[u8]| -> Result<u64> {
            if binary {
                Ok(0)
            } else {
                text(bytes)?.parse().map_err(|_| fail())
            }
        };
        let (path, orig_path) = if raw_path.is_empty() {
            let old = parts.get(i).ok_or_else(fail)?;
            let new = parts.get(i + 1).ok_or_else(fail)?;
            i += 2;
            if old.is_empty() || new.is_empty() {
                return Err(fail());
            }
            (text(new)?, Some(text(old)?))
        } else {
            (text(raw_path)?, None)
        };
        out.push(Change {
            path,
            orig_path,
            added: parse(added)?,
            deleted: parse(deleted)?,
            binary,
        });
    }
    Ok(out)
}
pub fn output(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(SpecError::new(
            "ERR_GIT",
            format!("git {} failed", args.first().unwrap_or(&"command")),
        ));
    }
    Ok(out.stdout)
}
pub fn toplevel(root: &Path) -> Result<PathBuf> {
    Ok(
        PathBuf::from(text(&output(root, &["rev-parse", "--show-toplevel"])?)?.trim())
            .canonicalize()?,
    )
}
pub fn common_dir(root: &Path) -> Result<PathBuf> {
    let dir = text(&output(root, &["rev-parse", "--git-common-dir"])?)?;
    Ok(root.join(dir.trim()).canonicalize()?)
}
pub fn status(root: &Path) -> Result<Vec<Change>> {
    parse_status(&output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?)
}
pub fn diff(root: &Path, branch: Option<&str>) -> Result<Vec<Change>> {
    let base = if let Some(reference) = branch {
        if reference.starts_with('-') {
            return Err(SpecError::new("ERR_CONFIG", "invalid branch reference"));
        }
        let oid = text(&output(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
        )?)?;
        text(&output(root, &["merge-base", "HEAD", oid.trim()])?)?
            .trim()
            .to_string()
    } else if output(root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
        "HEAD".into()
    } else {
        let mut child = Command::new("git")
            .args(["hash-object", "-t", "tree", "--stdin"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        drop(child.stdin.take());
        let result = child.wait_with_output()?;
        if !result.status.success() {
            return Err(SpecError::new(
                "ERR_GIT",
                "could not compute object-format-aware empty tree",
            ));
        }
        text(&result.stdout)?.trim().into()
    };
    parse_numstat(&output(
        root,
        &[
            "diff",
            "--cached",
            "--numstat",
            "-z",
            "-M",
            "-C",
            &base,
            "--",
        ],
    )?)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub mode: String,
    pub blob: String,
}
pub fn index(root: &Path) -> Result<BTreeMap<String, IndexEntry>> {
    let mut result = BTreeMap::new();
    for token in tokens(&output(root, &["ls-files", "--stage", "-z"])?)? {
        let tab = token.iter().position(|b| *b == b'\t').ok_or_else(fail)?;
        let header = text(&token[..tab])?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || fields[2] != "0" {
            return Err(SpecError::new(
                "ERR_GIT",
                "unmerged index cannot be approved",
            ));
        }
        result.insert(
            text(&token[tab + 1..])?,
            IndexEntry {
                mode: fields[0].into(),
                blob: fields[1].into(),
            },
        );
    }
    Ok(result)
}
pub fn worktree_matches_index(root: &Path, path: &str, entry: Option<&IndexEntry>) -> Result<bool> {
    if let Some(entry) = entry {
        let abs = root.join(path);
        if !abs.is_file() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = std::fs::metadata(&abs)?.permissions().mode() & 0o111 != 0;
            if (entry.mode == "100755") != executable {
                return Ok(false);
            }
        }
        let hash = text(&output(
            root,
            &["hash-object", &format!("--path={path}"), "--", path],
        )?)?;
        Ok(hash.trim() == entry.blob)
    } else {
        Ok(!root.join(path).try_exists()?)
    }
}
pub fn write_stdin(root: &Path, args: &[&str], bytes: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| SpecError::new("ERR_GIT", "stdin unavailable"))?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(SpecError::new("ERR_GIT", "git stdin operation failed"));
    }
    Ok(output.stdout)
}
