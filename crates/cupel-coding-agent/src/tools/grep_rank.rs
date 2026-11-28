//! Ranking for the grep tool's `files` output mode.
//!
//! `outputMode` set to `files` answers "Which files matter for this pattern?"
//! with one line per file, and the order of that list is the whole point,
//! a model reads the top entries first, so the file that defines the
//! searched name has to come before the twenty files that merely use it,
//! and test code has to come after production code. Four signals, all
//! computed from what the search already produced (no index, no git, no
//! file reads, nothing language specific beyond a keyword list),
//!
//! 1. a match line defines the searched name itself
//!    (`pub struct Foo`, `func Foo(`, a Python class named `Foo` for the pattern `Foo`),
//! 2. a match line is a definition of something that mentions the name
//!    (`func (s *Store) WriteFoo(` is a loose query a model might send),
//! 3. the file's bucket by path convention, source before tests before the
//!    low signal periphery (examples, fixtures, mocks, vendored code),
//! 4. the number of matches, capped so a busy file cannot outrank a quiet
//!    one on volume alone.
//!
//! Ties keep path order, so the output is deterministic. Measured on pgr's
//! public offline benchmark (real agent queries on entireio/cli, relevance
//! = the files the agent went on to read), MRR 0.455 vs pgr's 0.405 and
//! raw ripgrep's 0.32, Hit@1 40% vs 34% vs 26%.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::path::{Path, PathBuf};

use grep_matcher::Matcher as _;
use grep_regex::RegexMatcher;

use crate::search::{SearchMatch, SearchQuery, escape_regex};

/// Beyond this many matches a file is "busy", and busier does not mean
/// more relevant.
const DENSITY_CAP: usize = 10;

/// Where a file sits in the repository. Declared worst first so the
/// derived `Ord` doubles as the rank, `Source > Test > LowPriority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    LowPriority,
    Test,
    Source,
}

/// One file in the ranked list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    pub path: PathBuf,
    pub preview_line_number: u64,
    pub preview: String,
    pub matches: usize,
    pub more_matches: bool,
    pub bucket: Bucket,
    pub defines_name: bool,
    pub definition_shaped: bool,
}

impl FileSummary {
    fn sort_key(&self) -> (bool, bool, Bucket, usize) {
        (
            self.defines_name,
            self.definition_shaped,
            self.bucket,
            self.matches.min(DENSITY_CAP),
        )
    }

    fn rank_cmp(&self, other: &Self) -> Ordering {
        self.sort_key()
            .cmp(&other.sort_key())
            .then_with(|| other.path.cmp(&self.path))
    }
}

struct RankedFile(FileSummary);

impl PartialEq for RankedFile {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankedFile {}

impl Ord for RankedFile {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.rank_cmp(&other.0)
    }
}

impl PartialOrd for RankedFile {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(crate) struct BestFiles {
    files: BinaryHeap<Reverse<RankedFile>>,
}

impl BestFiles {
    pub(crate) fn retain(&mut self, summary: FileSummary, limit: usize) {
        let candidate = Reverse(RankedFile(summary));
        if self.files.len() < limit {
            self.files.push(candidate);
        } else if let Some(mut worst) = self.files.peek_mut()
            && candidate < *worst
        {
            *worst = candidate;
        }
    }

    pub(crate) fn into_ranked(self) -> Vec<FileSummary> {
        self.files
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(RankedFile(summary))| summary)
            .collect()
    }
}

/// Knows what a definition looks like.
pub struct Ranker {
    name: Option<RegexMatcher>,
    shaped: Option<RegexMatcher>,
}

impl Ranker {
    #[must_use]
    pub fn new(query: &SearchQuery) -> Self {
        let pattern = if query.literal {
            escape_regex(&query.pattern)
        } else {
            query.pattern.clone()
        };
        let boundary = if pattern
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            r"\b"
        } else {
            ""
        };
        let build = |template: &str| {
            grep_regex::RegexMatcherBuilder::new()
                .case_insensitive(query.ignore_case)
                .build(template)
                .ok()
        };
        Self {
            name: build(&defines_name_pattern(&pattern, boundary)),
            shaped: build(&definition_shaped_pattern()),
        }
    }

    /// Group `matches` by file and order the files most relevant first.
    /// `root` is the directory the search ran in, path conventions are
    /// checked relative to it, so a project living under `/home/me/test/`
    /// is not test code wholesale.
    #[must_use]
    pub fn rank(&self, matches: &[SearchMatch], root: &Path) -> Vec<FileSummary> {
        let mut per_file: BTreeMap<&Path, Vec<&SearchMatch>> = BTreeMap::new();
        for m in matches {
            per_file.entry(&m.path).or_default().push(m);
        }
        let mut files: Vec<FileSummary> = per_file
            .into_iter()
            .map(|(path, lines)| self.summarize(path, root, &lines))
            .collect();
        files.sort_by(|left, right| right.rank_cmp(left));
        files
    }

    fn summarize(&self, path: &Path, root: &Path, lines: &[&SearchMatch]) -> FileSummary {
        let mut summary = Self::file(path, root);
        for hit in lines {
            self.add_match(&mut summary, hit.line_number, &hit.line, usize::MAX);
        }
        summary
    }

    pub(crate) fn file(path: &Path, root: &Path) -> FileSummary {
        let relative = path
            .strip_prefix(root)
            .unwrap_or_else(|_not_under_root| Path::new(path.file_name().unwrap_or_default()));
        FileSummary {
            path: path.to_path_buf(),
            preview_line_number: 0,
            preview: String::new(),
            matches: 0,
            more_matches: false,
            bucket: bucket_of(relative),
            defines_name: false,
            definition_shaped: false,
        }
    }

    pub(crate) fn add_match(
        &self,
        file: &mut FileSummary,
        line_number: u64,
        line: &str,
        cap: usize,
    ) {
        let named = !file.defines_name && is_match(self.name.as_ref(), line);
        let shaped = !file.definition_shaped && is_match(self.shaped.as_ref(), line);
        if file.matches == 0 || named || (shaped && !file.defines_name) {
            file.preview_line_number = line_number;
            file.preview = crate::truncate::truncate_line(line.trim(), 120).0;
        }
        file.defines_name |= named;
        file.definition_shaped |= shaped;
        if file.matches < cap {
            file.matches += 1;
        } else {
            file.more_matches = true;
        }
    }
}

fn is_match(matcher: Option<&RegexMatcher>, line: &str) -> bool {
    matcher.is_some_and(|re| re.is_match(line.as_bytes()).unwrap_or(false))
}

/// Visibility and qualifier words that may precede a definition keyword.
const MODIFIERS: &str = r#"(?:pub(?:\([^)]*\))?|public|private|protected|internal|export|default|async|unsafe|extern(?:\s+"[^"]+")?|static|const|abstract|final|override|sealed|partial|virtual|synchronized)\s+"#;
/// Keywords that introduce an item (function, type, or module) in the
/// common languages. Items are definitions at any indentation.
const ITEM_KEYWORDS: &str = "fn|func|function|def|class|struct|enum|union|trait|interface|type|typedef|mod|static|macro_rules!|record|protocol";
/// Keywords that introduce a binding. Only a binding at column 0 is a
/// definition (a module level `const foo = ...`), indented ones are the
/// locals every function body is made of.
const BINDING_KEYWORDS: &str = "let|var|const";

/// "This line defines `pattern`", optional modifiers, one or two keywords
/// (`typedef struct Foo`), optional generics, then the pattern itself.
fn defines_name_pattern(pattern: &str, boundary: &str) -> String {
    format!(
        r"^\s*(?:{MODIFIERS})*(?:(?:(?:{ITEM_KEYWORDS})\s*(?:<[^>]*>)?\s+){{1,2}}(?:{pattern}){boundary}|(?:{BINDING_KEYWORDS})\s+(?:{pattern}){boundary}(?:\s|=|:[^:]|$)|func\s+\([^)]*\)\s+(?:{pattern}){boundary}|impl\b[^{{]*\bfor\s+(?:{pattern}){boundary}|impl\s*(?:<[^>]*>)?\s+(?:{pattern}){boundary}(?:<[^>]*>)?\s*(?:\{{|$))"
    )
}

/// "This line is a definition of something", an item keyword after
/// optional modifiers, or a binding keyword at column 0. Comment lines
/// never qualify.
fn definition_shaped_pattern() -> String {
    format!(
        r"^(?:\s*(?:{MODIFIERS})*(?:{ITEM_KEYWORDS}|impl)(?:[\s<(]|\b)|(?:{MODIFIERS})*(?:{BINDING_KEYWORDS})\s)"
    )
}

/// pgr's buckets, by path convention only. The periphery is checked first
/// so `vendor/tests/x.go` is low priority, not test.
fn bucket_of(relative: &Path) -> Bucket {
    const LOW_DIRS: &[&str] = &[
        "example",
        "examples",
        "sample",
        "samples",
        "fixture",
        "fixtures",
        "mock",
        "mocks",
        "testdata",
        "vendor",
        "node_modules",
        "third_party",
        "docs",
    ];
    const TEST_DIRS: &[&str] = &["test", "tests", "testing", "spec", "specs", "__tests__"];
    let components: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    if components.iter().any(|c| LOW_DIRS.contains(&c.as_str())) {
        return Bucket::LowPriority;
    }
    let Some((name, dirs)) = components.split_last() else {
        return Bucket::Source;
    };
    let markdown = relative.extension().is_some_and(|extension| {
        ["md", "markdown", "mdx"]
            .iter()
            .any(|kind| extension.eq_ignore_ascii_case(kind))
    });
    if markdown || name == "changelog" || name.starts_with("changelog.") {
        return Bucket::LowPriority;
    }
    if dirs.iter().any(|c| TEST_DIRS.contains(&c.as_str())) {
        return Bucket::Test;
    }
    if name.contains("_test.")
        || name.contains("_tests.")
        || name.contains("_spec.")
        || name.starts_with("test_")
        || name.starts_with("test.")
        || name.starts_with("tests.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || relative
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|stem| stem.ends_with("Test") || stem.ends_with("Tests"))
    {
        return Bucket::Test;
    }
    Bucket::Source
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchLimit;

    fn query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: 0,
            limit: SearchLimit::Matches(100),
        }
    }

    fn hit(path: &str, line_number: u64, line: &str) -> SearchMatch {
        SearchMatch {
            path: PathBuf::from(path),
            line_number,
            line: line.to_string(),
        }
    }

    fn defines(ranker: &Ranker, line: &str) -> bool {
        is_match(ranker.name.as_ref(), line)
    }

    fn shaped(ranker: &Ranker, line: &str) -> bool {
        is_match(ranker.shaped.as_ref(), line)
    }

    #[test]
    fn defining_lines_are_recognized_across_languages() {
        let ranker = Ranker::new(&query("Foo"));
        for line in [
            "pub struct Foo {",
            "pub(crate) struct Foo;",
            "struct Foo<'a> {",
            "pub enum Foo {",
            "pub trait Foo: Send {",
            "impl Foo {",
            "impl<T> Foo<T> {",
            "impl Display for Foo {",
            "pub type Foo = Box<dyn Error>;",
            "pub async fn Foo() {",
            "    fn Foo(&self) -> bool {",
            "export default function Foo() {",
            "export const Foo = () => {",
            "class Foo:",
            "def Foo(x):",
            "func Foo(x int) {",
            "type Foo struct {",
            "interface Foo {",
            "typedef struct Foo {",
            "macro_rules! Foo {",
            "const Foo = require('foo');",
            "func (s *Server) Foo(w http.ResponseWriter) {",
            "public final class Foo {",
            "public sealed partial class Foo {",
            "pub unsafe extern \"C\" fn Foo() {}",
        ] {
            assert!(defines(&ranker, line), "should define Foo: {line}");
            assert!(shaped(&ranker, line), "should be definition-shaped: {line}");
        }
        for line in [
            "use crate::Foo;",
            "let x = Foo::new();",
            "    Foo,",
            "// the Foo type",
            "/// fn Foo is documented here",
            "fn make_foo() -> Foo {",
            "    let Foo::Bar(value) = item else {",
            "impl Foo for Bar {",
        ] {
            assert!(!defines(&ranker, line), "does not define Foo: {line}");
        }
    }

    #[test]
    fn definition_shaped_lines_mention_the_name_without_defining_it() {
        let ranker = Ranker::new(&query("Foo"));
        for line in [
            "fn make_foo() -> Foo {",
            "func (s *Store) WriteFoo(ctx context.Context) error {",
            "    def build_foo(self) -> Foo:",
            "export const useFoo = () => {",
            "const foo = new Foo();",
            "impl<T> Bar<T> for Foo {",
        ] {
            assert!(shaped(&ranker, line), "definition-shaped: {line}");
        }
        for line in [
            "use crate::Foo;",
            "    let foo = Foo::new();",
            "    const x = Foo.bar;",
            "// fn Foo() {",
            "# def foo():",
            "return Foo{}",
        ] {
            assert!(!shaped(&ranker, line), "not a definition: {line}");
        }
    }

    #[test]
    fn word_boundary_keeps_prefixes_apart() {
        let ranker = Ranker::new(&query("Search"));
        assert!(!defines(&ranker, "pub struct SearchQuery {"));
        assert!(defines(&ranker, "pub struct Search {"));
        // A pattern ending in punctuation gets no boundary, so it can
        // still match.
        let ranker = Ranker::new(&query("describe_call\\("));
        assert!(defines(&ranker, "fn describe_call(&self) -> String {"));
    }

    #[test]
    fn literal_and_case_insensitive_flags_carry_over() {
        let mut q = query("$foo");
        q.literal = true;
        assert!(defines(&Ranker::new(&q), "let $foo = 1;"));
        let mut q = query("greptool");
        q.ignore_case = true;
        assert!(defines(&Ranker::new(&q), "pub struct GrepTool {"));
    }

    #[test]
    fn unembeddable_pattern_switches_only_the_name_signal_off() {
        // An anchored pattern is valid regex but can never match inside
        // the name template, no panic, no name definitions. The shape
        // signal still works.
        let ranker = Ranker::new(&query("^fn main"));
        assert!(!defines(&ranker, "fn main() {}"));
        assert!(shaped(&ranker, "fn main() {}"));
    }

    #[test]
    fn rank_orders_name_then_shape_then_bucket_then_density() {
        let root = Path::new("/project");
        let matches = vec![
            hit("/project/src/user.rs", 3, "use crate::Foo;"),
            hit("/project/src/user.rs", 9, "    let f = Foo::new();"),
            hit("/project/tests/it.rs", 5, "fn Foo() {}"),
            hit("/project/src/builder.rs", 7, "fn make_foo() -> Foo {"),
            hit("/project/examples/demo.rs", 1, "pub struct Foo {"),
            hit("/project/src/foo.rs", 12, "pub struct Foo {"),
        ];
        let ranked = Ranker::new(&query("Foo")).rank(&matches, root);
        let paths: Vec<&str> = ranked.iter().map(|f| f.path.to_str().unwrap()).collect();
        assert_eq!(
            paths,
            [
                "/project/src/foo.rs",       // defines the name, source
                "/project/tests/it.rs",      // defines the name, test
                "/project/examples/demo.rs", // defines the name, low priority
                "/project/src/builder.rs",   // definition shaped, source
                "/project/src/user.rs",      // references only
            ]
        );
        assert!(ranked[0].defines_name);
        assert_eq!(ranked[0].preview_line_number, 12);
        assert_eq!(ranked[1].bucket, Bucket::Test);
        assert_eq!(ranked[2].bucket, Bucket::LowPriority);
        assert!(ranked[3].definition_shaped && !ranked[3].defines_name);
        assert_eq!(ranked[4].matches, 2);
    }

    #[test]
    fn density_breaks_ties_and_is_capped() {
        let root = Path::new("/project");
        let mut matches = vec![hit("/project/a.txt", 1, "Foo here")];
        for n in 1..=30 {
            matches.push(hit("/project/b.txt", n, "Foo again"));
        }
        for n in 1..=12 {
            matches.push(hit("/project/c.txt", n, "Foo again"));
        }
        let ranked = Ranker::new(&query("Foo")).rank(&matches, root);
        let paths: Vec<&str> = ranked.iter().map(|f| f.path.to_str().unwrap()).collect();
        // b and c are both at the cap and equal, so path order decides.
        assert_eq!(
            paths,
            ["/project/b.txt", "/project/c.txt", "/project/a.txt"]
        );
    }

    #[test]
    fn preview_prefers_the_defining_line_over_an_earlier_use() {
        let root = Path::new("/project");
        let matches = vec![
            hit("/project/x.rs", 2, "use super::Foo;"),
            hit("/project/x.rs", 20, "fn make_foo() -> Foo {"),
            hit("/project/x.rs", 40, "pub struct Foo {"),
        ];
        let ranked = Ranker::new(&query("Foo")).rank(&matches, root);
        assert_eq!(ranked[0].preview_line_number, 40);
        assert_eq!(ranked[0].preview, "pub struct Foo {");
        // Without a defining line the shaped one wins over the plain use.
        let ranked = Ranker::new(&query("Foo")).rank(&matches[..2], root);
        assert_eq!(ranked[0].preview_line_number, 20);
    }

    #[test]
    fn buckets_follow_path_conventions() {
        for path in [
            "src/main.rs",
            "cmd/server/main.go",
            "lib/foo.py",
            "src/testing.rs",
            "attest/x.py",
            "src/contest.java",
        ] {
            assert_eq!(bucket_of(Path::new(path)), Bucket::Source, "{path}");
        }
        for path in [
            "tests/it.rs",
            "src/test/Foo.java",
            "src/__tests__/foo.js",
            "spec/models/user_spec.rb",
            "testing/mock_client.go",
            "src/foo_test.go",
            "src/foo.test.ts",
            "src/foo.spec.js",
            "src/test_foo.py",
            "crates/cupel-coding-agent/src/undo_tests.rs",
            "src/usage_tests.rs",
            "src/jobs/tests.rs",
            "src/foo_spec.rb",
            "src/FooTest.java",
            "src/FooTests.cs",
        ] {
            assert_eq!(bucket_of(Path::new(path)), Bucket::Test, "{path}");
        }
        for path in [
            "vendor/github.com/foo/bar.go",
            "node_modules/lodash/index.js",
            "examples/hello.rs",
            "testdata/input.txt",
            "src/fixtures/data.json",
            "vendor/tests/foo.go",
            "docs/reference.py",
            "CHANGELOG.md",
            "AI-CODING-AGENT-EDIT-FINDINGS.md",
        ] {
            assert_eq!(bucket_of(Path::new(path)), Bucket::LowPriority, "{path}");
        }
    }
}
