mod compare;
mod git;

use std::env;
use std::io;
use std::io::Write as _;
use std::process::ExitCode;

use anyhow::{Context, Result};

const USAGE: &str = "Usage: no-new-comments --base <ref> --head <ref> [--github-actions]";
const MESSAGE: &str = "new or rewritten Rust comment";

struct Args {
    base: String,
    head: String,
    format: Format,
}

#[derive(Clone, Copy)]
enum Format {
    Plain,
    GitHubActions,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("no-new-comments: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Result<Vec<_>> = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .ok()
                .context("arguments must be valid UTF-8")
        })
        .collect();
    let mut args = args?.into_iter().peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }

    let Args { base, head, format } = Args::parse(args)?;
    let mut output = io::stdout().lock();
    for change in git::changes(&base, &head)? {
        let old = change
            .old_blob
            .as_deref()
            .map(git::read_blob)
            .transpose()
            .with_context(|| format!("read baseline for {:?}", change.path))?;
        let new = git::read_blob(&change.new_blob)
            .with_context(|| format!("read head file {:?}", change.path))?;

        for line in compare::added_comment_lines(old.as_deref().unwrap_or_default(), &new) {
            writeln!(output, "{}", format.warning(&change.path, line))
                .context("write comment warning")?;
        }
    }
    output.flush().context("flush comment warnings")
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut base = None;
        let mut head = None;
        let mut format = Format::Plain;
        while let Some(arg) = args.next() {
            let slot = match arg.as_str() {
                "--base" => &mut base,
                "--head" => &mut head,
                "--github-actions" => {
                    format = Format::GitHubActions;
                    continue;
                }
                _ => anyhow::bail!("unexpected argument {arg:?}\n{USAGE}"),
            };
            anyhow::ensure!(slot.is_none(), "duplicate {arg}\n{USAGE}");
            *slot = Some(
                args.next()
                    .filter(|value| !value.starts_with('-') && !value.is_empty())
                    .with_context(|| format!("missing value for {arg}\n{USAGE}"))?,
            );
        }
        Ok(Self {
            base: base.context(USAGE)?,
            head: head.context(USAGE)?,
            format,
        })
    }
}

impl Format {
    fn warning(self, path: &str, line: usize) -> String {
        match self {
            Self::Plain => format!(
                "{}:{line}: warning: no-new-comments: {MESSAGE}",
                path.escape_debug()
            ),
            Self::GitHubActions => {
                let path = path
                    .replace('%', "%25")
                    .replace('\r', "%0D")
                    .replace('\n', "%0A")
                    .replace(':', "%3A")
                    .replace(',', "%2C");
                format!("::warning file={path},line={line},title=no-new-comments::{MESSAGE}")
            }
        }
    }
}
