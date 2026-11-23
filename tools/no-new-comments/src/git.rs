use std::process::Command;
use std::str;

use anyhow::{Context, Result};

pub(crate) struct Change {
    pub path: String,
    pub old_blob: Option<String>,
    pub new_blob: String,
}

pub(crate) fn changes(base: &str, head: &str) -> Result<Vec<Change>> {
    let base = resolve_commit(base)?;
    let head = resolve_commit(head)?;
    let merge_base = git_text(&["merge-base", &base, &head])
        .with_context(|| format!("resolve merge base for {base} and {head}"))?;
    let raw = git(&[
        "diff-tree",
        "-r",
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-relative",
        "--find-renames",
        "--diff-filter=AMRT",
        merge_base.trim_end(),
        &head,
        "--",
    ])
    .context("compare merge-base and head trees")?;

    parse_changes(&raw)
}

fn parse_changes(raw: &[u8]) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let mut fields = raw.split_inclusive(|&byte| byte == 0);
    while let Some(header) = fields.next() {
        let header = str::from_utf8(nul_field(header)?)?;
        let parts: Vec<_> = header.split_ascii_whitespace().collect();
        let [old_mode, new_mode, old_blob, new_blob, status] = parts.as_slice() else {
            anyhow::bail!("invalid Git diff header {header:?}");
        };
        let first_path = nul_field(fields.next().context("missing Git diff path")?)?;
        let path = if status.starts_with('R') {
            nul_field(fields.next().context("missing Git rename destination")?)?
        } else {
            first_path
        };
        let old_is_file = old_mode.starts_with(":100");
        if !path.ends_with(b".rs")
            || !new_mode.starts_with("100")
            || (old_is_file && old_blob == new_blob)
        {
            continue;
        }

        changes.push(Change {
            path: str::from_utf8(path)
                .context("changed Rust path is not UTF-8")?
                .to_owned(),
            old_blob: old_is_file.then(|| (*old_blob).to_owned()),
            new_blob: (*new_blob).to_owned(),
        });
    }
    Ok(changes)
}

fn nul_field(field: &[u8]) -> Result<&[u8]> {
    field
        .strip_suffix(b"\0")
        .context("unterminated Git diff field")
}

pub(crate) fn read_blob(blob: &str) -> Result<String> {
    git_text(&["cat-file", "blob", blob])
}

fn resolve_commit(reference: &str) -> Result<String> {
    let peeled = format!("{reference}^{{commit}}");
    git_text(&["rev-parse", "--verify", "--end-of-options", &peeled])
        .with_context(|| format!("resolve commit {reference:?}"))
        .map(|oid| oid.trim_end().to_owned())
}

fn git_text(args: &[&str]) -> Result<String> {
    String::from_utf8(git(args)?)
        .with_context(|| format!("git {} returned non-UTF-8 data", args[0]))
}

fn git(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git").args(args).output().context("run Git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} exited with {}: {}",
        args[0],
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end(),
    );
    Ok(output.stdout)
}
