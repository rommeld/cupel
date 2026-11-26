#![cfg(test)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

#[test]
fn advisory_warning_uses_pr_head_line_despite_divergence_and_dirty_checkout() {
    let repo = Repo::new();
    let path = "src/example.rs";
    let original = "fn first() {} // retained on PR\n\nfn middle() {}\n\nfn last() {}\n";
    repo.write(path, original);
    repo.commit("common ancestor");

    repo.git(&["switch", "-c", "pr"]);
    repo.write(
        path,
        &format!(
            "{original}\n/// PR documentation\nfn pr() -> &'static str {{\n    r\"// literal\" // PR comment\n}}\n"
        ),
    );
    let head = repo.commit("pull request changes");

    repo.git(&["switch", "main"]);
    let target = original.replace("// retained on PR", "// rewritten on target");
    repo.write(
        path,
        &format!("// target-only comment\nfn target() {{}}\n\n{target}"),
    );
    let base = repo.commit("target changes");

    repo.git(&["switch", "--detach", &head]);
    repo.git(&["merge", "--no-edit", &base]);
    let merged = repo.git(&["show", "HEAD:src/example.rs"]);
    assert!(
        merged
            .lines()
            .nth(11)
            .is_some_and(|line| line.ends_with("// PR comment"))
    );

    repo.write(path, "// staged only\n");
    repo.git(&["add", "--", path]);
    repo.write(path, "// unstaged only\n");
    let index = repo.git(&["write-tree"]);

    let output = repo.run(&["--base", &base, "--head", &head, "--github-actions"]);
    assert_eq!(
        stdout(output),
        "::warning file=src/example.rs,line=9,title=no-new-comments::new or rewritten Rust comment\n"
    );
    assert_eq!(repo.git(&["write-tree"]), index, "the index was changed");
    assert_eq!(
        fs::read_to_string(repo.path().join(path)).unwrap(),
        "// unstaged only\n"
    );
}

#[test]
fn detected_rename_keeps_its_baseline_and_warns_at_the_destination() {
    let repo = Repo::new();
    let original = "// retained comment\nfn stable_code_keeps_rename_similarity_high() {}\n";
    repo.write("src/original.rs", original);
    repo.write("src/deleted.rs", "// deleted\n");
    let base = repo.commit("baseline");

    let path = "src/renamed %, file.rs";
    repo.git(&["mv", "src/original.rs", path]);
    repo.git(&["rm", "src/deleted.rs"]);
    repo.write(path, &format!("{original}// added\n"));
    repo.write("src/added.rs", "// in a new file\n");
    repo.write("not-rust.txt", "// ignored\n");
    repo.commit("rename and edits");

    assert_eq!(
        stdout(repo.run(&["--head", "HEAD", "--base", &base])),
        concat!(
            "src/added.rs:1: warning: no-new-comments: new or rewritten Rust comment\n",
            "src/renamed %, file.rs:3: warning: no-new-comments: new or rewritten Rust comment\n",
        ),
    );
    assert_eq!(
        stdout(repo.run(&["--head", "HEAD", "--base", &base, "--github-actions"])),
        concat!(
            "::warning file=src/added.rs,line=1,title=no-new-comments::new or rewritten Rust comment\n",
            "::warning file=src/renamed %25%2C file.rs,line=3,title=no-new-comments::new or rewritten Rust comment\n",
        ),
    );
}

#[test]
fn duplicated_inline_comment_is_reported_at_the_inserted_line() {
    let repo = Repo::new();
    let original = "fn example() -> Vec<i32> {\n    let mut items = Vec::new();\n    items.push(1); // keep\n    items\n}\n";
    repo.write("example.rs", original);
    let base = repo.commit("baseline");
    let new = original.replace(
        "    items.push(1)",
        "    items.push(2); // keep\n    items.push(1)",
    );
    repo.write("example.rs", &new);
    repo.commit("insert a line with the same comment");

    assert_eq!(
        stdout(repo.run(&["--base", &base, "--head", "HEAD", "--github-actions"])),
        "::warning file=example.rs,line=3,title=no-new-comments::new or rewritten Rust comment\n"
    );
}

#[test]
fn shebang_with_or_without_bom_keeps_the_real_comment_on_line_two() {
    let repo = Repo::new();
    repo.write("main.rs", "fn main() {}\n");
    let base = repo.commit("baseline");
    let script =
        "#!/usr/bin/env -S rust-script // interpreter argument\nfn main() {} // real comment\n";
    repo.write("bom.rs", &format!("\u{feff}{script}"));
    repo.write("plain.rs", script);
    repo.commit("add scripts with shebangs");

    assert_eq!(
        stdout(repo.run(&["--base", &base, "--head", "HEAD", "--github-actions"])),
        concat!(
            "::warning file=bom.rs,line=2,title=no-new-comments::new or rewritten Rust comment\n",
            "::warning file=plain.rs,line=2,title=no-new-comments::new or rewritten Rust comment\n",
        )
    );
}

#[test]
fn blanket_lint_warnings_use_committed_head_and_merge_base() {
    let repo = Repo::new();
    let original = "fn main() {}\n";
    repo.write("main.rs", original);
    let retained = "#[allow(warnings)]\nfn kept() {}\n";
    repo.write("lib.rs", retained);
    repo.commit("common ancestor");

    repo.git(&["switch", "-c", "pr"]);
    repo.write(
        "main.rs",
        &format!("\u{feff}#![allow(warnings)]\n{original}"),
    );
    repo.write(
        "lib.rs",
        &format!("{retained}\n#[allow(dead_code,\n    clippy::all, reason = \"pending cleanup\",\n)]\nfn added() {{}}\n"),
    );
    let head = repo.commit("introduce inner and outer blanket suppressions");

    repo.git(&["switch", "main"]);
    repo.write("main.rs", &format!("#![allow(warnings)]\n{original}"));
    let base = repo.commit("independently introduce target suppression");

    repo.write("lib.rs", "#![allow(clippy::all)]\n");
    repo.git(&["add", "lib.rs"]);
    repo.write("main.rs", original);

    assert_eq!(
        stdout(repo.run(&["--base", &base, "--head", &head, "--github-actions"])),
        concat!(
            "::warning file=lib.rs,line=4,title=no-blanket-lint-allows::new blanket Rust lint suppression\n",
            "::warning file=main.rs,line=1,title=no-blanket-lint-allows::new blanket Rust lint suppression\n",
        )
    );
    assert_eq!(
        stdout(repo.run(&["--base", &base, "--head", &head])),
        concat!(
            "lib.rs:4: warning: no-blanket-lint-allows: new blanket Rust lint suppression\n",
            "main.rs:1: warning: no-blanket-lint-allows: new blanket Rust lint suppression\n",
        )
    );
}

#[test]
fn missing_ref_and_unrelated_histories_fail_the_check() {
    let repo = Repo::new();
    repo.write("main.rs", "fn main() {}\n");
    repo.commit("main");
    repo.git(&["switch", "--orphan", "unrelated"]);
    repo.write("other.rs", "// unrelated\n");
    repo.commit("unrelated");

    for (base, error) in [
        ("missing", "resolve commit"),
        ("main", "resolve merge base"),
    ] {
        let output = repo.run(&["--base", base, "--head", "unrelated", "--github-actions"]);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !output.status.success(),
            "unexpected success with base {base}"
        );
        assert!(stderr.contains(error), "expected {error:?}, got {stderr:?}");
        assert!(output.stdout.is_empty());
    }
}

struct Repo(TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "--initial-branch=main", "--template="]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim_end().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(self.path())
            .args(args)
            .output()
            .unwrap();
        stdout(output)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_no-new-comments"))
            .current_dir(self.path())
            .args(args)
            .output()
            .unwrap()
    }
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
