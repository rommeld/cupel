/// The git side of `/spinoff`: parallel sessions, each in its own git worktree. This
/// module knows nothing about sessions or the UI. If finds the checkouts that belong
/// together, creates a spinoff's worktree, and tells whether two checkout's changes
/// would conflict.
///
/// Every git call is a plain `std::process::Command`, as in `reviews.rs`. Only
/// machine-readable output (`porcelain` and plumbing) is parsed, because git
/// translates its message for people.
use std::path::{Path, PathBuf};

pub const BRANCH_PREFIX: &str = "cupel/spinoff/";

#[derive(Debug, thiserror::Error)]
pub enum SpinoffError {
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Blocked(String),
    #[error("`git {command}` failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("cannot update {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The checkouts that work together: the main checkout, where the origin session runs,
/// and every worktree on a `cupel/spinoff/<name>` branch. Other worktrees, such as
/// Claude Code's `.claude/worktrees/*`, are not part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub origin: Origin,
    pub spinoffs: Vec<Spinoff>,
}

/// The main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spinoff {
    pub name: String,
    pub path: PathBuf,
    pub head: String,
}

impl Group {
    /// Read the group from git. `cwd` may be anywhere inside the main checkout or
    /// inside one of its worktrees.
    pub fn discover(cwd: &Path) -> Result<Self, SpinoffError> {
        let listing =
            git(cwd, &["worktree", "list", "--porcelain"]).map_err(|error| match error {
                SpinoffError::Git { stderr, .. } => {
                    SpinoffError::Unsupported(format!("/spinoff needs a git repository: {stderr}"))
                }
                other => other,
            })?;
        Self::parse(&listing)
            .ok_or_else(|| SpinoffError::Unsupported("git listed no worktree".to_string()))
    }

    /// Parse `git worktree list --porcelain`: one record per worktree, separated by
    /// blank lines. The first record is always the main checkout.
    fn parse(listing: &str) -> Option<Self> {
        let mut entries = listing.split("\n\n").filter_map(Entry::parse);
        let main = entries.next()?;
        let origin = Origin {
            path: PathBuf::from(main.path),
            head: main.head.to_string(),
            branch: main.branch.map(str::to_string),
        };
        let spinoffs = entries
            .filter(|entry| !entry.prunable)
            .filter_map(|entry| {
                let name = entry.branch?.strip_prefix(BRANCH_PREFIX)?;
                Some(Spinoff {
                    name: name.to_string(),
                    path: PathBuf::from(entry.path),
                    head: entry.head.to_string(),
                })
            })
            .collect();
        Some(Self { origin, spinoffs })
    }
}

/// One record of `git worktree list --porcelain`. It borrows its text from the listing
/// (the lifetime `'a`) instead of copying it; `Group::parse` copies only what it keeps.
struct Entry<'a> {
    path: &'a str,
    head: &'a str,
    branch: Option<&'a str>,
    prunable: bool,
}

impl<'a> Entry<'a> {
    fn parse(record: &'a str) -> Option<Self> {
        let mut entry = Entry {
            path: "",
            head: "",
            branch: None,
            prunable: false,
        };
        for line in record.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => entry.path = value,
                "HEAD" => entry.head = value,
                "branch" => entry.branch = value.strip_prefix("refs/heads/"),
                "prunable" => entry.prunable = true,
                // "detached", "bare", "locked": nothing `/spinoff` needs
                _ => {}
            }
        }
        (!entry.path.is_empty()).then_some(entry)
    }
}

/// Start `git -C <dir> <args>`, optionally with another index file (`GIT_INDEX_FILE`,
/// see [`snapshot`]). Fails only when git cannot be started at all; a git command that
/// fails still returns its output.
fn run(
    dir: &Path,
    index: Option<&Path>,
    args: &[&str],
) -> Result<std::process::Output, SpinoffError> {
    let mut command = std::process::Command::new("git");
    command.arg("-C").arg(dir).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
        .output()
        .map_err(|error| SpinoffError::Unsupported(format!("cannot run git: {error}")))
}

/// [`run`] for commands that must succeed: their stdout, or [`SpinoffError::Git`] with
/// git's own explanation.
fn git_in(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String, SpinoffError> {
    let output = run(dir, index, args)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(failure(args, &output))
    }
}

/// [`git_in`] with the checkout's own index.
fn git(dir: &Path, args: &[&str]) -> Result<String, SpinoffError> {
    git_in(dir, None, args)
}

/// The error for a git command that exited unsuccessfully.
fn failure(args: &[&str], output: &std::process::Output) -> SpinoffError {
    SpinoffError::Git {
        command: args.join(" "),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// A spinoff name that passed validation: 3 to 24 characters from `a-z`, `0-9` and
/// `-`, and not a `/spinoff` subcommand. The only way to get one is to parse a string,
/// so a function that takes a `&SpinoffName` never has to check it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinoffName(String);

const RESERVED: [&str; 2] = ["merge", "drop"];

impl core::str::FromStr for SpinoffName {
    type Err = SpinoffError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        let valid = (3..=24).contains(&name.len())
            && !name.starts_with('-')
            && name.chars().all(allowed)
            && !RESERVED.contains(&name);
        if valid {
            Ok(Self(name.to_string()))
        } else {
            Err(SpinoffError::Blocked(format!(
                "invalid spinoff name \"{name}\": use 3 to 24 characters from a-z, 0-9 and -, \
                not starting with - (merge and drop are commands)"
            )))
        }
    }
}

impl SpinoffName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const EXCLUDE_LINE: &str = "/.cupel/worktrees/";

/// Create spinoff `name`: the branch `cupel/spinoff/<name>` at the origin's HEAD,
/// checked out in `<origin>/.cupel/worktrees/<name>`.
pub fn create(group: &Group, name: &SpinoffName) -> Result<Spinoff, SpinoffError> {
    let origin = &group.origin;
    if origin.branch.is_none() {
        return Err(SpinoffError::Blocked(
            "the main checkout is on a detached HEAD - check out a branch first, so the \
            spinoff has a branch to merge back into"
                .to_string(),
        ));
    }
    if origin.head.bytes().all(|byte| byte == b'0') {
        return Err(SpinoffError::Blocked(
            "the repository has no commit yet - commit once, then start a spinoff".to_string(),
        ));
    }
    if group
        .spinoffs
        .iter()
        .any(|spinoff| spinoff.name == name.as_str())
    {
        return Err(SpinoffError::Blocked(format!(
            "spinoff {} already exists",
            name.as_str()
        )));
    }
    let branch = format!("{BRANCH_PREFIX}{}", name.as_str());
    let reference = format!("refs/heads/{branch}");
    let verify = ["rev-parse", "--verify", "--quiet", reference.as_str()];
    if run(&origin.path, None, &verify)?.status.success() {
        return Err(SpinoffError::Blocked(format!(
            "branch {branch} already exists - delete it (git branch -D {branch}) or pick \
            another name"
        )));
    }
    let path = origin
        .path
        .join(".cupel")
        .join("worktrees")
        .join(name.as_str());
    if path.exists() {
        return Err(SpinoffError::Blocked(format!(
            "{} already exists - remove it or pick another name",
            path.display()
        )));
    }

    ensure_excluded(&origin.path)?;
    let target = path.to_string_lossy();
    git(
        &origin.path,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            &branch,
            &target,
            &origin.head,
        ],
    )?;
    Ok(Spinoff {
        name: name.as_str().to_string(),
        path,
        head: origin.head.clone(),
    })
}

/// Make the main checkout ignore `.cupel/worktrees/`. Without the line, `git add -A`
/// there would add every spinoff as an ambedded repository. `.git/info/exclude` works
/// like `.gitignore`, but is never committed.
fn ensure_excluded(origin: &Path) -> Result<(), SpinoffError> {
    let common = git(
        origin,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let info = Path::new(common.trim_end()).join("info");
    let exclude = info.join("exclude");
    let mut text = match std::fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(SpinoffError::Io {
                path: exclude,
                source,
            });
        }
    };
    if text.lines().any(|line| line.trim() == EXCLUDE_LINE) {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(EXCLUDE_LINE);
    text.push('\n');
    std::fs::create_dir_all(&info)
        .and_then(|()| std::fs::write(&exclude, text))
        .map_err(|source| SpinoffError::Io {
            path: exclude,
            source,
        })
}

/// A copy of a checkot's index for [`snaphsot`] to fill. Dropping the guard deletes
/// the file, on every way out of `snapshot`, including the early returns of `?`.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Record the checkout at `dir` as it is right now, tracked changes and untracked
/// files alike (`.gitignore` respected), as a commit whose parent is its HEAD. Returns
/// the commit id. The checkout's index, files and branches stay untouched: the file go
/// into a copy of the index, and no branch points at the commit, so git's garbage
/// collection removes it.
pub fn snapshot(dir: &Path) -> Result<String, SpinoffError> {
    let index = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let index = PathBuf::from(index.trim_end());
    // Next to the real index, so every checkout gets its own copy.
    let temp = TempIndex(index.with_extension("cupel-snapshot"));
    // A copy keeps git's file timestamp, so `add --all` only reads the files that
    // changed.
    std::fs::copy(&index, &temp.0).map_err(|source| SpinoffError::Io {
        path: index.clone(),
        source,
    })?;
    git_in(dir, Some(&temp.0), &["add", "--all"])?;
    let tree = git_in(dir, Some(&temp.0), &["write-tree"])?;
    let commit = git(
        dir,
        &[
            "-c",
            "user.name=cupel",
            "-c",
            "user.email=cupel@localhost",
            "commit-tree",
            "--no-gpg-sign",
            "-p",
            "HEAD",
            "-m",
            "cupel snapshot",
            tree.trim_end(),
        ],
    )?;
    Ok(commit.trim_end().to_string())
}

/// The files that conflict when the two snapshots are merged. `repo` is any chechout
/// of the respository, since all of them share one object store. Nothing changes here
/// ether. The merged tree only goes into the object store.
pub fn conflicts(repo: &Path, ours: &str, theirs: &str) -> Result<Vec<String>, SpinoffError> {
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        "-z",
        ours,
        theirs,
    ];
    let output = run(repo, None, &args)?;
    match output.status.code() {
        Some(0) => Ok(Vec::new()),
        Some(1) => Ok(parse_conflicts(&output.stdout)),
        _ => Err(failure(&args, &output)),
    }
}

/// Read `mege-tree -z --name-only` output. The merged tree's id, then the conflicted
/// paths verbatim; without it, git would quote unusual ones (`ü.rs` becomes
/// `"\303\274.rs"`).
fn parse_conflicts(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .split('\0')
        .skip(1)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// One side of a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    Origin,
    Spinoff(String),
}

/// Two checkouts whose current changes conflict, and the files they conflict in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub sides: (Side, Side),
    pub files: Vec<String>,
}

impl Conflict {
    /// The other side when `side` is one of the two, else `None`.
    #[must_use]
    pub fn other(&self, side: &Side) -> Option<&Side> {
        if self.sides.0 == *side {
            Some(&self.sides.1)
        } else if self.sides.1 == *side {
            Some(&self.sides.0)
        } else {
            None
        }
    }
}

/// Every pair of checkouts in `group` whose current changes conflict. Each checkout is
/// snapshotted once; then every pair is merged in the object store, so nothing in any
/// checkout changes.
pub fn check(group: &Group) -> Result<Vec<Conflict>, SpinoffError> {
    let mut members = vec![(Side::Origin, snapshot(&group.origin.path)?)];
    for spinoff in &group.spinoffs {
        let side = Side::Spinoff(spinoff.name.clone());
        members.push((side, snapshot(&spinoff.path)?));
    }
    let mut found = Vec::new();
    for (index, (side, snap)) in members.iter().enumerate() {
        for (other, other_snap) in &members[index + 1..] {
            let files = conflicts(&group.origin.path, snap, other_snap)?;
            if !files.is_empty() {
                found.push(Conflict {
                    sides: (side.clone(), other.clone()),
                    files,
                });
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten numbered lines: edits to line 2 and line 9 are far enough apart
    /// for git to merge them.
    const LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    /// An empty repository on `main`. Every test passes its own name,
    /// because tests run in parallel. The path is canonical because git
    /// prints canonical paths: on macOS the temp dir `/var/...` is really
    /// `/private/var/...`.
    fn init(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cupel-spinoff-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        sh(&root, &["init", "--quiet", "--initial-branch", "main"]);
        root
    }

    /// [`init`] plus one commit of `a.txt`.
    fn repo(name: &str) -> PathBuf {
        let root = init(name);
        std::fs::write(root.join("a.txt"), LINES).unwrap();
        sh(&root, &["add", "--all"]);
        sh(&root, &["commit", "--quiet", "--message", "init"]);
        root
    }

    /// Run git for test setup with a fixed identity (CI runners have no git
    /// config) and no commit signing. Returns stdout.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@localhost"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?}: {stderr}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `git worktree add -b <branch> <origin>/<dir>` by hand; `create`
    /// follows in step 1.3. Returns the worktree's path.
    fn add_worktree(origin: &Path, branch: &str, dir: &str) -> PathBuf {
        let path = origin.join(dir);
        let target = path.to_str().unwrap();
        sh(origin, &["worktree", "add", "-b", branch, target]);
        path
    }

    #[test]
    fn discover_finds_the_origin_and_only_spinoff_worktrees() {
        let origin = repo("discover");
        let head = sh(&origin, &["rev-parse", "HEAD"]).trim_end().to_string();
        let auth = add_worktree(&origin, "cupel/spinoff/auth", ".cupel/worktrees/auth");
        // A worktree that is not a spinoff, like Claude Code's.
        add_worktree(&origin, "other", ".claude/worktrees/other");
        // A spinoff whose directory was deleted by hand: git marks it prunable.
        let gone = add_worktree(&origin, "cupel/spinoff/gone", ".cupel/worktrees/gone");
        std::fs::remove_dir_all(gone).unwrap();

        let group = Group::discover(&origin).unwrap();
        let expected_origin = Origin {
            path: origin.clone(),
            head: head.clone(),
            branch: Some("main".to_string()),
        };
        assert_eq!(group.origin, expected_origin);
        let expected_auth = Spinoff {
            name: "auth".to_string(),
            path: auth.clone(),
            head,
        };
        assert_eq!(group.spinoffs, vec![expected_auth]);
        // Seen from inside the spinoff, the group is the same.
        assert_eq!(Group::discover(&auth).unwrap(), group);
    }

    #[test]
    fn discover_outside_a_repository_is_unsupported() {
        let dir = std::env::temp_dir().join("cupel-spinoff-no-repo");
        std::fs::create_dir_all(&dir).unwrap();
        let result = Group::discover(&dir);
        assert!(matches!(result, Err(SpinoffError::Unsupported(_))));
    }

    /// The message of a `Blocked` error; panics on anything else. The UI
    /// shows it as one notice line, so it must not contain a line break,
    /// which a wrapped string literal without a trailing `\` would add.
    fn blocked<T>(result: Result<T, SpinoffError>) -> String
    where
        T: core::fmt::Debug,
    {
        match result {
            Err(SpinoffError::Blocked(message)) => {
                assert!(!message.contains('\n'), "line break in: {message}");
                message
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn spinoff_names_are_validated() {
        for good in ["auth", "fix-2", "2nd", "abc", "abcdefghijklmnopqrstuvwx"] {
            assert_eq!(good.parse::<SpinoffName>().unwrap().as_str(), good);
        }
        let too_long = "abcdefghijklmnopqrstuvwxy";
        for bad in [
            "", "Auth", "a b", "-x", "a/b", "ü", "merge", "drop", too_long,
        ] {
            let message = blocked(bad.parse::<SpinoffName>());
            assert!(message.contains("a-z, 0-9 and -"), "{bad}");
        }
    }

    #[test]
    fn create_checks_out_a_branch_that_the_origin_does_not_see() {
        let origin = repo("create");
        let group = Group::discover(&origin).unwrap();
        let auth = create(&group, &"auth".parse().unwrap()).unwrap();

        assert_eq!(auth.path, origin.join(".cupel/worktrees/auth"));
        let branch = sh(&auth.path, &["branch", "--show-current"]);
        assert_eq!(branch, "cupel/spinoff/auth\n");
        assert_eq!(auth.head, group.origin.head);
        // The main checkout's git does not see the nested worktree.
        assert_eq!(sh(&origin, &["status", "--porcelain"]), "");
        // A second spinoff does not add the exclude line again.
        let group = Group::discover(&origin).unwrap();
        create(&group, &"docs".parse().unwrap()).unwrap();
        let exclude = std::fs::read_to_string(origin.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(EXCLUDE_LINE).count(), 1);
        assert_eq!(Group::discover(&origin).unwrap().spinoffs.len(), 2);
    }

    #[test]
    fn create_refuses_what_it_cannot_merge_back() {
        let origin = repo("refuse");
        let auth: SpinoffName = "auth".parse().unwrap();
        create(&Group::discover(&origin).unwrap(), &auth).unwrap();

        // The same name twice.
        let twice = create(&Group::discover(&origin).unwrap(), &auth);
        assert!(blocked(twice).contains("already exists"));
        // A branch left behind by a worktree that was deleted by hand.
        sh(&origin, &["branch", "cupel/spinoff/left"]);
        let left = create(&Group::discover(&origin).unwrap(), &"left".parse().unwrap());
        assert!(blocked(left).contains("git branch -D cupel/spinoff/left"));
        // A detached HEAD: there is no branch to merge back into.
        sh(&origin, &["checkout", "--quiet", "--detach"]);
        let detached = create(&Group::discover(&origin).unwrap(), &"docs".parse().unwrap());
        assert!(blocked(detached).contains("detached"));
        // No commit at all.
        let empty = init("refuse-empty");
        let none = create(&Group::discover(&empty).unwrap(), &auth);
        assert!(blocked(none).contains("no commit"));
    }

    /// Replace line `number` (counting from 1) of `a.txt` in `dir`.
    fn edit(dir: &Path, number: usize, text: &str) {
        let path = dir.join("a.txt");
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[number - 1] = text.to_string();
        std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }

    #[test]
    fn snapshots_include_untracked_files_and_leave_the_checkout_alone() {
        let origin = repo("snapshot");
        std::fs::write(origin.join(".gitignore"), "target/\n").unwrap();
        sh(&origin, &["add", ".gitignore"]);
        sh(
            &origin,
            &["commit", "--quiet", "--message", "ignore target"],
        );
        edit(&origin, 2, "2 changed");
        std::fs::write(origin.join("new.rs"), "fn new() {}\n").unwrap();
        std::fs::create_dir_all(origin.join("target")).unwrap();
        std::fs::write(origin.join("target/junk"), "build output\n").unwrap();
        let before = sh(&origin, &["status", "--porcelain"]);

        let commit = snapshot(&origin).unwrap();

        // Untracked new.rs is in, ignored target/ is out.
        let files = sh(&origin, &["ls-tree", "-r", "--name-only", &commit]);
        assert_eq!(files, ".gitignore\na.txt\nnew.rs\n");
        let a_txt = sh(&origin, &["show", &format!("{commit}:a.txt")]);
        assert_eq!(
            a_txt,
            std::fs::read_to_string(origin.join("a.txt")).unwrap()
        );
        // The parent is HEAD, and the checkout is exactly as it was.
        let parent = sh(&origin, &["rev-parse", &format!("{commit}^")]);
        assert_eq!(parent, sh(&origin, &["rev-parse", "HEAD"]));
        assert_eq!(sh(&origin, &["status", "--porcelain"]), before);
        assert!(!origin.join(".git/index.cupel-snapshot").exists());
    }

    #[test]
    fn conflicts_report_the_same_line_but_not_separate_hunks() {
        let origin = repo("conflicts");
        let group = Group::discover(&origin).unwrap();
        let auth = create(&group, &"auth".parse().unwrap()).unwrap();

        // Different lines of the same file merge cleanly.
        edit(&origin, 2, "2 origin");
        edit(&auth.path, 9, "9 auth");
        let ours = snapshot(&origin).unwrap();
        let theirs = snapshot(&auth.path).unwrap();
        assert!(conflicts(&origin, &ours, &theirs).unwrap().is_empty());

        // The same line, and the same new file with different content, do not.
        edit(&origin, 5, "5 origin");
        edit(&auth.path, 5, "5 auth");
        std::fs::write(origin.join("new.rs"), "// origin\n").unwrap();
        std::fs::write(auth.path.join("new.rs"), "// auth\n").unwrap();
        let ours = snapshot(&origin).unwrap();
        let theirs = snapshot(&auth.path).unwrap();
        assert_eq!(
            conflicts(&origin, &ours, &theirs).unwrap(),
            ["a.txt", "new.rs"]
        );
    }

    #[test]
    fn conflicts_compare_committed_with_uncommitted_work() {
        let origin = repo("committed");
        let group = Group::discover(&origin).unwrap();
        let auth = create(&group, &"auth".parse().unwrap()).unwrap();
        edit(&origin, 5, "5 origin");
        sh(
            &origin,
            &["commit", "--quiet", "--all", "--message", "origin work"],
        );
        edit(&auth.path, 5, "5 auth");

        let ours = snapshot(&origin).unwrap();
        let theirs = snapshot(&auth.path).unwrap();
        assert_eq!(conflicts(&origin, &ours, &theirs).unwrap(), ["a.txt"]);
    }

    #[test]
    fn check_reports_each_conflicting_pair_once() {
        let origin = repo("check");
        let group = Group::discover(&origin).unwrap();
        let auth = create(&group, &"auth".parse().unwrap()).unwrap();
        let group = Group::discover(&origin).unwrap();
        let docs = create(&group, &"docs".parse().unwrap()).unwrap();
        let group = Group::discover(&origin).unwrap();
        assert!(check(&group).unwrap().is_empty(), "nothing changed yet");

        // The origin and auth both change line 5; docs only changes line 2,
        // which merges with both.
        edit(&origin, 5, "5 origin");
        edit(&auth.path, 5, "5 auth");
        edit(&docs.path, 2, "2 docs");
        let found = check(&group).unwrap();
        let auth_side = Side::Spinoff("auth".to_string());
        let docs_side = Side::Spinoff("docs".to_string());
        let expected = Conflict {
            sides: (Side::Origin, auth_side.clone()),
            files: vec!["a.txt".to_string()],
        };
        assert_eq!(found, vec![expected]);
        assert_eq!(found[0].other(&Side::Origin), Some(&auth_side));
        assert_eq!(found[0].other(&docs_side), None);

        // Two spinoffs conflict with each other too.
        edit(&docs.path, 5, "5 docs");
        let found = check(&group).unwrap();
        let pairs: Vec<_> = found.iter().map(|conflict| &conflict.sides).collect();
        assert_eq!(
            pairs,
            [
                &(Side::Origin, auth_side.clone()),
                &(Side::Origin, docs_side.clone()),
                &(auth_side, docs_side),
            ]
        );
    }
}
