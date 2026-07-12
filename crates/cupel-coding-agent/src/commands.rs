//! Slash commands: `/name args` in the input, pi-style.
//!
//! Two kinds, dispatched in this order by the frontends:
//!
//! 1. **Built-ins** (`/help`, `/new`, `/model`, ...) - intercepted by the UI
//!    and never sent to the model.
//! 2. **Prompt templates** - markdown files in `<root>/prompts/*.md` (same
//!    resource roots as AGENTS.md: cupel home, project `.cupel/`, project
//!    root). `/name args` expands the file's
//!    body with bash-style argument substitution and sends THAT as the
//!    prompt. This is how users package reusable prompts.
//!
//! Anything else starting with `/` passes through to the model as literal
//! text - a typo becomes a question, not an error.

use std::path::{Path, PathBuf};

use crate::resources::split_frontmatter;

// ---------------------------------------------------------------------------
// Argument parsing + substitution
// ---------------------------------------------------------------------------

/// Split a command's argument string bash-style: whitespace separates,
/// single or double quotes group.
#[must_use]
pub fn parse_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for c in args.chars() {
        match in_quote {
            Some(quote) if c == quote => in_quote = None,
            None if c == '"' || c == '\'' => in_quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(core::mem::take(&mut current));
                }
            }
            // Quoted content and ordinary characters both accumulate.
            Some(_) | None => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Substitute argument placeholders in a template body. Supported forms
/// (all 1-indexed, matching bash and pi):
///
/// - `$1`, `$2`, ... - positional argument (empty when missing)
/// - `$@` / `$ARGUMENTS` - all arguments joined with spaces
/// - `${N:-default}` - positional N, or `default` when missing/empty
/// - `${@:N}` - arguments from N onward
/// - `${@:N:L}` - L arguments starting at N
///
/// Replacement is single-pass over the template only: argument VALUES that
/// contain `$1` etc. are not recursively substituted.
#[must_use]
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // ${...} forms.
        if chars.get(i + 1) == Some(&'{') {
            if let Some(close) = chars[i + 2..].iter().position(|c| *c == '}') {
                let inner: String = chars[i + 2..i + 2 + close].iter().collect();
                if let Some(replacement) = substitute_braced(&inner, args) {
                    out.push_str(&replacement);
                    i += close + 3; // ${ + inner + }
                    continue;
                }
            }
            // Unrecognized ${...} stays literal, like pi's regex miss.
            out.push('$');
            i += 1;
            continue;
        }

        let rest: String = chars[i + 1..].iter().collect();
        if rest.starts_with("ARGUMENTS") {
            out.push_str(&all_args);
            i += 1 + "ARGUMENTS".len();
        } else if rest.starts_with('@') {
            out.push_str(&all_args);
            i += 2;
        } else {
            let digits: String = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if digits.is_empty() {
                out.push('$');
                i += 1;
            } else {
                let index = digits.parse::<usize>().unwrap_or(0);
                if let Some(value) = index.checked_sub(1).and_then(|idx| args.get(idx)) {
                    out.push_str(value);
                }
                i += 1 + digits.len();
            }
        }
    }
    out
}

/// The `${...}` forms: `N:-default`, `@:N`, `@:N:L`. `None` = leave literal.
fn substitute_braced(inner: &str, args: &[String]) -> Option<String> {
    if let Some((number, default)) = inner.split_once(":-") {
        let index = number.parse::<usize>().ok()?;
        let value = index.checked_sub(1).and_then(|idx| args.get(idx));
        return Some(match value {
            Some(v) if !v.is_empty() => v.clone(),
            _ => default.to_string(),
        });
    }
    if let Some(slice) = inner.strip_prefix("@:") {
        let (start_str, length) = match slice.split_once(':') {
            Some((s, l)) => (s, Some(l.parse::<usize>().ok()?)),
            None => (slice, None),
        };
        // Bash convention: args are 1-indexed and ${@:0} behaves like ${@:1}.
        let start = start_str.parse::<usize>().ok()?.saturating_sub(1);
        let sliced = args.get(start..).unwrap_or(&[]);
        let sliced = match length {
            Some(len) => &sliced[..len.min(sliced.len())],
            None => sliced,
        };
        return Some(sliced.join(" "));
    }
    None
}

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

/// One `/name`-invocable prompt loaded from `<root>/prompts/<name>.md`.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    /// The body (frontmatter stripped) with `$N` placeholders intact.
    pub content: String,
    pub path: PathBuf,
}

/// Load templates from `<root>/prompts/*.md` (non-recursive) for each
/// resource root. Later roots win name collisions (project overrides the
/// binary-installation defaults), mirroring the context-file precedence.
#[must_use]
pub fn load_prompt_templates(roots: &[PathBuf]) -> Vec<PromptTemplate> {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.join("prompts")) else {
            continue; // No prompts directory - perfectly normal.
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") || !path.is_file() {
                continue;
            }
            if let Some(template) = load_template(&path) {
                // Same-name template from a later root replaces the earlier.
                templates.retain(|t| t.name != template.name);
                templates.push(template);
            }
        }
    }
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    templates
}

fn load_template(path: &Path) -> Option<PromptTemplate> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&raw);
    let name = path.file_stem()?.to_string_lossy().into_owned();

    // Description: frontmatter, else the first non-empty body line (60 chars).
    let description = frontmatter
        .iter()
        .find(|(key, _)| key == "description")
        .map(|(_, value)| value.clone())
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|line| {
                    let mut d: String = line.chars().take(60).collect();
                    if line.chars().count() > 60 {
                        d.push_str("...");
                    }
                    d
                })
        })
        .unwrap_or_default();

    Some(PromptTemplate {
        name,
        description,
        content: body.trim().to_string(),
        path: path.to_path_buf(),
    })
}

/// Expand `/name args` against the templates. `None` = not a template.
#[must_use]
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let (name, args_str) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let template = templates.iter().find(|t| t.name == name)?;
    let args = parse_command_args(args_str);
    Some(substitute_args(&template.content, &args))
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// A UI-handled command: never sent to the model. The frontends match on
/// the name; this table exists for `/help` and autocomplete.
pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "help",
        description: "List all commands and prompt templates",
    },
    BuiltinCommand {
        name: "new",
        description: "Clear the conversation and start fresh",
    },
    BuiltinCommand {
        name: "model",
        description: "Switch model: /model <id> (no argument lists them)",
    },
    BuiltinCommand {
        name: "provider",
        description: "Switch provider: /provider <name> [api-key] (no argument lists them)",
    },
    BuiltinCommand {
        name: "thinking",
        description: "Set thinking level: /thinking off|minimal|low|medium|high|xhigh",
    },
    BuiltinCommand {
        name: "usage",
        description: "Show session token and cost totals",
    },
    BuiltinCommand {
        name: "quit",
        description: "Quit cupel",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn command_args_split_bash_style() {
        assert_eq!(parse_command_args("a b  c"), args(&["a", "b", "c"]));
        assert_eq!(
            parse_command_args("fix \"the parser bug\" now"),
            args(&["fix", "the parser bug", "now"])
        );
        assert_eq!(parse_command_args("'a b' c"), args(&["a b", "c"]));
        assert_eq!(parse_command_args(""), Vec::<String>::new());
    }

    #[test]
    fn positional_and_all_args_substitute() {
        let a = args(&["one", "two", "three"]);
        assert_eq!(substitute_args("$1 and $2", &a), "one and two");
        assert_eq!(substitute_args("all: $@", &a), "all: one two three");
        assert_eq!(substitute_args("all: $ARGUMENTS", &a), "all: one two three");
        assert_eq!(substitute_args("missing: [$9]", &a), "missing: []");
        assert_eq!(substitute_args("price: $5.99", &args(&[])), "price: .99");
        assert_eq!(substitute_args("literal $ sign", &a), "literal $ sign");
    }

    #[test]
    fn braced_forms_substitute() {
        let a = args(&["one", "two", "three", "four"]);
        assert_eq!(substitute_args("${2:-fallback}", &a), "two");
        assert_eq!(substitute_args("${9:-fallback}", &a), "fallback");
        assert_eq!(substitute_args("${@:2}", &a), "two three four");
        assert_eq!(substitute_args("${@:2:2}", &a), "two three");
        assert_eq!(substitute_args("${@:0}", &a), "one two three four");
        // Unrecognized braces stay literal.
        assert_eq!(substitute_args("${HOME}", &a), "${HOME}");
    }

    #[test]
    fn templates_load_expand_and_project_overrides() {
        let global = std::env::temp_dir().join("cupel-commands-global");
        let project = std::env::temp_dir().join("cupel-commands-project");
        // The project's `.cupel/` directory - a root between home and cwd.
        let dot_cupel = project.join(".cupel");
        let _ = std::fs::remove_dir_all(&global);
        let _ = std::fs::remove_dir_all(&project);
        for dir in [&global, &dot_cupel, &project] {
            std::fs::create_dir_all(dir.join("prompts")).unwrap();
        }
        std::fs::write(
            global.join("prompts/review.md"),
            "---\ndescription: Review code\n---\nReview $1 carefully.",
        )
        .unwrap();
        std::fs::write(global.join("prompts/global-only.md"), "Global body.").unwrap();
        // `.cupel/` overrides the global template with the same name...
        std::fs::write(dot_cupel.join("prompts/review.md"), "Dot-cupel review.").unwrap();
        // ...and the project root, being the LAST root, overrides both.
        std::fs::write(project.join("prompts/review.md"), "Project review of $@.").unwrap();

        let templates = load_prompt_templates(&[global, dot_cupel, project]);
        assert_eq!(templates.len(), 2);
        let review = templates.iter().find(|t| t.name == "review").unwrap();
        assert_eq!(review.content, "Project review of $@.");

        let expanded = expand_prompt_template("/review src/main.rs quickly", &templates).unwrap();
        assert_eq!(expanded, "Project review of src/main.rs quickly.");
        assert!(expand_prompt_template("/unknown", &templates).is_none());
        assert!(expand_prompt_template("no slash", &templates).is_none());
    }
}
