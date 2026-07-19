//! The `/review` built-in: bundle code into a review prompt.
//!
//! Unlike the other built-ins, `/review` is not UI-local - it BUILDS a
//! prompt (files or a git diff, with truncation) and the frontend sends it
//! through the normal prompt path. Gathering is synchronous filesystem/git
//! work, cheap enough for the TUI's key handler; the actual model call
//! then runs through each frontend's usual async machinery.
//!
//! Invocations:
//! - `/review`                    - the whole project (cwd, gitignore-aware)
//! - `/review <path> [<path>...]` - specific files and/or directories
//! - `/review --diff`             - the current git diff (HEAD vs working tree)
//!
//! Content limits lean on `truncate.rs`: a single explicitly named file
//! gets the full tool budget; files swept up by a directory walk share a
//! smaller per-file budget plus a global cap, so `/review` on a large repo
//! produces a bounded prompt instead of a context explosion.

use std::path::{Path, PathBuf};

use crate::truncate::{TruncationOptions, format_size, truncate_head};

/// Per-file budget when a DIRECTORY walk collects many files.
const WALK_FILE_OPTIONS: TruncationOptions = TruncationOptions {
    max_lines: Some(400),
    max_bytes: Some(16 * 1024),
};
/// Budget for one explicitly named file (and for the diff): the standard
/// tool-output budget from truncate.rs.
const EXPLICIT_OPTIONS: TruncationOptions = TruncationOptions {
    max_lines: None,
    max_bytes: None,
};
/// Walk caps: at most this many files, and the whole bundle stops growing
/// past this many bytes (whichever hits first).
const MAX_WALK_FILES: usize = 40;
const MAX_BUNDLE_BYTES: usize = 150 * 1024;

/// What the model is asked to do with the bundle.
const INSTRUCTIONS: &str = "Review the following code for correctness bugs, security issues, and \
     worthwhile simplifications. Be specific: cite file paths and line numbers, explain why each \
     finding is a problem, and rank findings by severity. Do not modify any files - report only.";

/// Build the `/review` prompt. `args` are the already-split command
/// arguments; relative paths resolve against `cwd`. `Err` is a
/// user-facing message (unknown path, no diff, ...) - nothing is sent.
pub fn build_review_prompt(cwd: &Path, args: &[String]) -> Result<String, String> {
    let wants_diff = args.iter().any(|a| a == "--diff");
    if wants_diff && args.len() > 1 {
        return Err("`/review --diff` cannot be combined with paths".to_string());
    }
    if wants_diff {
        return build_diff_prompt(cwd);
    }

    let mut bundle = String::new();
    if args.is_empty() {
        // Whole-project review: sweep the cwd.
        collect_directory(cwd, cwd, &mut bundle);
    } else {
        for arg in args {
            let path = resolve(cwd, arg);
            if path.is_dir() {
                collect_directory(&path, cwd, &mut bundle);
            } else if path.is_file() {
                // An explicitly named file earns the full budget - the user
                // asked for THIS file, so show as much of it as a read would.
                push_file(&path, cwd, EXPLICIT_OPTIONS, &mut bundle);
            } else {
                return Err(format!("path not found: {arg}"));
            }
        }
    }
    if bundle.is_empty() {
        return Err("nothing reviewable found (no readable text files)".to_string());
    }
    Ok(format!("{INSTRUCTIONS}\n\n{bundle}"))
}

/// `git diff HEAD` (staged + unstaged vs the last commit); falls back to
/// plain `git diff` for repos without a commit yet.
fn build_diff_prompt(cwd: &Path) -> Result<String, String> {
    let diff = run_git_diff(cwd, &["diff", "HEAD"])
        .or_else(|_| run_git_diff(cwd, &["diff"]))
        .map_err(|e| format!("cannot run git diff: {e}"))?;
    if diff.trim().is_empty() {
        return Err("no changes to review (git diff is empty)".to_string());
    }

    let result = truncate_head(&diff, EXPLICIT_OPTIONS);
    let mut section = String::from("=== git diff (HEAD vs working tree) ===\n");
    section.push_str(&result.content);
    if result.truncated {
        section.push_str(&format!(
            "\n[diff truncated: showing first {} of {} lines ({} of {})]",
            result.output_lines,
            result.total_lines,
            format_size(result.output_bytes),
            format_size(result.total_bytes),
        ));
    }
    Ok(format!(
        "{INSTRUCTIONS}\n\nThe changes below are a git diff; review the CHANGED code in \
         context.\n\n{section}"
    ))
}

fn run_git_diff(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Absolute args stay as-is; relative ones anchor at the cwd.
fn resolve(cwd: &Path, arg: &str) -> PathBuf {
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Sweep a directory into the bundle: gitignore-aware (the same walker
/// knobs as the grep tool and `@path` autocomplete - hidden files in,
/// `.git` out), capped by file count and total bundle size, deterministic
/// order. Skipped files are summarized so the model knows what it is NOT
/// seeing - a silently partial review reads as a complete one.
fn collect_directory(dir: &Path, cwd: &Path, bundle: &mut String) {
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(ignore::DirEntry::into_path)
        .collect();
    files.sort();

    let mut included = 0_usize;
    let mut omitted = 0_usize;
    for path in &files {
        if included >= MAX_WALK_FILES || bundle.len() >= MAX_BUNDLE_BYTES {
            omitted += 1;
            continue;
        }
        if push_file(path, cwd, WALK_FILE_OPTIONS, bundle) {
            included += 1;
        } else {
            omitted += 1; // unreadable / not UTF-8 text (binaries)
        }
    }
    if omitted > 0 {
        bundle.push_str(&format!(
            "[{omitted} of {} files omitted (bundle limits or binary content) - ask for \
             specific paths to review them]\n\n",
            files.len()
        ));
    }
}

/// Append one file as a delimited section; false = skipped (unreadable or
/// not UTF-8, i.e. binaries).
fn push_file(path: &Path, cwd: &Path, options: TruncationOptions, bundle: &mut String) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    // Headers show cwd-relative paths so findings cite paths the user can
    // click/open directly.
    let display = path.strip_prefix(cwd).unwrap_or(path).display();
    let result = truncate_head(&content, options);
    bundle.push_str(&format!("=== file: {display} ===\n"));
    bundle.push_str(&result.content);
    if result.truncated {
        bundle.push_str(&format!(
            "\n[file truncated: showing first {} of {} lines ({} of {})]",
            result.output_lines,
            result.total_lines,
            format_size(result.output_bytes),
            format_size(result.total_bytes),
        ));
    }
    bundle.push_str("\n\n");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-review-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn explicit_file_gets_full_content_and_relative_header() {
        let cwd = temp_root("file");
        std::fs::write(cwd.join("lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let prompt = build_review_prompt(&cwd, &args(&["lib.rs"])).unwrap();
        assert!(prompt.contains("=== file: lib.rs ==="));
        assert!(prompt.contains("fn a() {}\nfn b() {}"));
        assert!(!prompt.contains("[file truncated"), "small file untouched");
        assert!(prompt.starts_with(INSTRUCTIONS));
    }

    #[test]
    fn large_files_are_truncated_with_a_note() {
        let cwd = temp_root("large");
        // 3000 lines exceeds the 2000-line default budget.
        let big: String = (0..3000).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line {i}");
            s
        });
        std::fs::write(cwd.join("big.txt"), &big).unwrap();
        let prompt = build_review_prompt(&cwd, &args(&["big.txt"])).unwrap();
        assert!(
            prompt.contains("[file truncated: showing first 2000 of 3000 lines"),
            "truncation note missing"
        );
        assert!(!prompt.contains("line 2999"), "tail must be cut");
    }

    #[test]
    fn project_review_walks_respects_gitignore_and_notes_omissions() {
        let cwd = temp_root("walk");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        // .gitignore rules only apply INSIDE a git repo (the ignore crate's
        // require_git default, same as the grep tool) - a bare .git dir
        // marks this fixture as one, like the autocomplete tests do.
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(cwd.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(cwd.join("secret.txt"), "hidden").unwrap();
        // A binary file must be skipped, not poison the bundle.
        std::fs::write(cwd.join("blob.bin"), [0_u8, 159, 146, 150]).unwrap();

        let prompt = build_review_prompt(&cwd, &[]).unwrap();
        assert!(prompt.contains("=== file: src/main.rs ==="));
        assert!(!prompt.contains("hidden"), "gitignored file excluded");
        assert!(prompt.contains("omitted"), "binary skip is announced");
    }

    #[test]
    fn multiple_paths_bundle_in_order_and_missing_paths_error() {
        let cwd = temp_root("multi");
        std::fs::write(cwd.join("a.rs"), "// a").unwrap();
        std::fs::write(cwd.join("b.rs"), "// b").unwrap();
        let prompt = build_review_prompt(&cwd, &args(&["a.rs", "b.rs"])).unwrap();
        let a = prompt.find("=== file: a.rs ===").unwrap();
        let b = prompt.find("=== file: b.rs ===").unwrap();
        assert!(a < b, "sections follow argument order");

        let err = build_review_prompt(&cwd, &args(&["nope.rs"])).unwrap_err();
        assert!(err.contains("path not found: nope.rs"));
    }

    #[test]
    fn diff_mode_reviews_the_git_diff() {
        let cwd = temp_root("diff");
        let git = |cmd_args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(cmd_args)
                .current_dir(&cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {cmd_args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(cwd.join("x.rs"), "old\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(cwd.join("x.rs"), "new\n").unwrap();

        let prompt = build_review_prompt(&cwd, &args(&["--diff"])).unwrap();
        assert!(prompt.contains("=== git diff"));
        assert!(prompt.contains("-old"));
        assert!(prompt.contains("+new"));

        // Clean tree: a clear message instead of an empty prompt.
        git(&["add", "."]);
        git(&["commit", "-qm", "second"]);
        let err = build_review_prompt(&cwd, &args(&["--diff"])).unwrap_err();
        assert!(err.contains("no changes"));

        // --diff mixed with paths is ambiguous - refuse.
        let err = build_review_prompt(&cwd, &args(&["--diff", "x.rs"])).unwrap_err();
        assert!(err.contains("cannot be combined"));
    }
}
