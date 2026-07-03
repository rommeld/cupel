//! System prompt construction. Port of pi's `system-prompt.ts`: tool list,
//! per-tool guidelines, project context files (eager), a skills catalog
//! (lazy - see [`crate::resources`]), and date/cwd last.

use std::path::Path;

use crate::resources::{ContextFile, Skill};

/// Build the system prompt. Guidelines mirror pi's per-tool guidance and
/// only appear for tools that are actually available.
#[must_use]
pub fn build_system_prompt(
    cwd: &Path,
    tools: &[(&str, &str)],
    context_files: &[ContextFile],
    skills: &[Skill],
) -> String {
    let tools_list = if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools
            .iter()
            .map(|(name, snippet)| format!("- {name}: {snippet}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let has = |tool: &str| tools.iter().any(|(name, _)| *name == tool);
    let mut guidelines: Vec<&str> = Vec::new();
    if has("grep") {
        guidelines.push("Use grep to locate code before answering questions about it");
    }
    if has("read") {
        guidelines.push("Use read to examine files instead of cat or sed");
    }
    if has("edit") {
        guidelines.push("Use edit for precise changes (edits[].oldText must match exactly)");
        guidelines.push(
            "When changing multiple separate locations in one file, use one edit call with \
             multiple entries in edits[] instead of multiple edit calls",
        );
        guidelines.push(
            "Keep edits[].oldText as small as possible while still being unique in the file. \
             Do not pad with large unchanged regions.",
        );
    }
    if has("write") {
        guidelines.push("Use write only for new files or complete rewrites");
    }
    guidelines.push("Be concise in your responses");
    guidelines.push("Show file paths clearly when working with files");
    let guidelines = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Forward slashes even on Windows: models handle them more reliably.
    let prompt_cwd = cwd.display().to_string().replace('\\', "/");
    let date = current_date();

    let mut prompt = format!(
        "You are an expert coding assistant operating inside cupel, a coding agent harness. \
You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

Guidelines:
{guidelines}"
    );

    // ---- Project context (eager): full contents, every request ------------
    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\nProject-specific instructions and guidelines:\n");
        for file in context_files {
            prompt.push_str(&format!(
                "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
                file.path.display(),
                file.content.trim_end(),
            ));
        }
        prompt.push_str("\n</project_context>");
    }

    // ---- Skills (lazy): catalog only; the model reads the file on demand.
    // Only useful when the read tool exists to do that reading.
    if has("read") && !skills.is_empty() {
        prompt.push_str(
            "\n\nThe following skills provide specialized instructions for specific tasks.\n\
             Use the read tool to load a skill's file when the task matches its description.\n\
             When a skill file references a relative path, resolve it against the skill's \
             directory and use that absolute path in tool commands.\n\n<available_skills>",
        );
        for skill in skills {
            prompt.push_str(&format!(
                "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    \
                 <location>{}</location>\n  </skill>",
                escape_xml(&skill.name),
                escape_xml(&skill.description),
                escape_xml(&skill.path.display().to_string()),
            ));
        }
        prompt.push_str("\n</available_skills>");
    }

    prompt.push_str(&format!(
        "\n\nCurrent date: {date}\nCurrent working directory: {prompt_cwd}"
    ));
    prompt
}

/// Escape text placed inside the skills XML block, so a `<` in a skill
/// description can't break the structure the model parses.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Date as `YYYY-MM-DD` without pulling in chrono: days since the Unix epoch,
/// converted via the civil-from-days algorithm (Howard Hinnant's classic).
fn current_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_tools_and_cwd() {
        let prompt = build_system_prompt(
            Path::new("/tmp/project"),
            &[("grep", "Search file contents for patterns")],
            &[],
            &[],
        );
        assert!(prompt.contains("- grep: Search file contents for patterns"));
        assert!(prompt.contains("Current working directory: /tmp/project"));
        // Empty inputs add no empty sections.
        assert!(!prompt.contains("<project_context>"));
        assert!(!prompt.contains("<available_skills>"));
    }

    #[test]
    fn context_files_are_embedded_eagerly() {
        let prompt = build_system_prompt(
            Path::new("/tmp/project"),
            &[("read", "Read files")],
            &[ContextFile {
                path: "/tmp/project/AGENTS.md".into(),
                content: "Always run cargo clippy.".to_string(),
            }],
            &[],
        );
        assert!(prompt.contains("<project_instructions path=\"/tmp/project/AGENTS.md\">"));
        assert!(prompt.contains("Always run cargo clippy."));
    }

    #[test]
    fn skills_appear_as_catalog_only_when_read_tool_exists() {
        let skill = Skill {
            name: "commit<style>".to_string(),
            description: "How to write commits".to_string(),
            path: "/tmp/skills/commit-style/SKILL.md".into(),
        };
        let with_read = build_system_prompt(
            Path::new("/tmp"),
            &[("read", "Read files")],
            &[],
            std::slice::from_ref(&skill),
        );
        assert!(with_read.contains("<available_skills>"));
        assert!(with_read.contains("<location>/tmp/skills/commit-style/SKILL.md</location>"));
        // XML-sensitive characters in names are escaped.
        assert!(with_read.contains("<name>commit&lt;style&gt;</name>"));

        // No read tool -> the model couldn't load skills; omit the catalog.
        let without_read =
            build_system_prompt(Path::new("/tmp"), &[("grep", "Search")], &[], &[skill]);
        assert!(!without_read.contains("<available_skills>"));
    }

    #[test]
    fn civil_date_known_value() {
        // 2026-07-03 is 20_637 days after the epoch.
        assert_eq!(civil_from_days(20_637), (2026, 7, 3));
    }
}
