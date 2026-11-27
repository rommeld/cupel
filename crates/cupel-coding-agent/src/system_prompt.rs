//! System prompt construction, tool list, per tool guidelines

use std::path::Path;

use crate::resources::ContextFile;
use crate::skills::Skill;

const TOOL_HEADER: &str = "\n\nAvailable tools:\n";
const GUIDELINES_HEADER: &str = "\n\nGuidelines:\n";

fn format_tools(tools: &[&str]) -> String {
    if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[must_use]
pub fn reload_tool_list(base: &str, tools: &[&str]) -> String {
    let Some((prefix, rest)) = base.split_once(TOOL_HEADER) else {
        return base.to_string();
    };
    let Some((_, suffix)) = rest.split_once(GUIDELINES_HEADER) else {
        return base.to_string();
    };
    format!(
        "{prefix}{TOOL_HEADER}{}{GUIDELINES_HEADER}{suffix}",
        format_tools(tools)
    )
}

#[must_use]
pub fn build_system_prompt(cwd: &Path, tools: &[&str], context_files: &[ContextFile]) -> String {
    let tools_list = format_tools(tools);

    let mut guidelines = vec![
        "For review or advice, inspect and explain without editing. For implementation, make \
        the smallest change that meets the goal.",
    ];
    if tools.contains(&"read") {
        guidelines.push("Use read directly when you know the file path.");
    }
    if tools.contains(&"grep") {
        guidelines.push("Use grep when the location or relevant callers are unknown.");
    }
    guidelines.push(
        "Search hits and partial reads are not complete evidence. Read applicable project \
        instructions in full.",
    );
    guidelines.push(
        "Before a nontrivial edit, identify what should change, what must stay true, and what \
        check would catch a mistake.",
    );
    if tools.contains(&"apply_patch") {
        guidelines.push("Use apply_patch for file changes.");
        if tools.contains(&"read") {
            guidelines.push(
                "For edits based on inspected source, pass expectedObservations. After a stale \
                or ambiguous match, reread and retarget rather than weakening the patch.",
            );
        }
    }
    guidelines.push("Parallelize only independent work.");
    if tools.contains(&"bash") {
        guidelines.push(
            "Preserve check exit statuses. Confirm the intended tests ran against the final \
            change; zero tests is not a pass.",
        );
    }
    if tools.contains(&"jobs") {
        guidelines.push(
            "On Unix, use bash with yield_after_ms to wait briefly or background: true to return \
            immediately, not shell &. Use jobs wait when you need the result. A job ID is not \
            readiness or success, and a test started before an edit does not validate that edit. \
            Shell side effects cannot be undone.",
        );
    }
    if tools.contains(&"ask_user") {
        guidelines.push(
            "Use ask_user for consequential choices the request and repository don't resolve; \
            recommend an option.",
        );
    }
    guidelines.push(
        "Lead with the result. Cite relevant paths, explain non-obvious decisions, and \
        distinguish what you observed from what remains unverified.",
    );
    let guidelines = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Forward slashes even on Windows, models handle them more reliably.
    let prompt_cwd = cwd.display().to_string().replace('\\', "/");
    let date = current_date();

    let mut prompt = format!(
        "You are a coding assistant in cupel.{TOOL_HEADER}{tools_list}{GUIDELINES_HEADER}{guidelines}"
    );

    if !context_files.is_empty() {
        prompt.push_str(
            "\n\n<project_context>\n\nProject-specific instructions and guidelines. These files \
             are already loaded; do not reread them unless editing them. Use read for needed \
             guides linked from these instructions.\n",
        );
        for file in context_files {
            prompt.push_str(&format!(
                "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
                file.path.display(),
                file.content.trim_end(),
            ));
        }
        prompt.push_str("\n</project_context>");
    }

    prompt.push_str(&format!(
        "\n\nCurrent date: {date}\nCurrent working directory: {prompt_cwd}"
    ));
    prompt
}

/// Append only skill metadata. Skill instructions are retrieved through `read` on demand.
#[must_use]
pub fn with_skill_catalog(base: &str, skills: &[Skill]) -> String {
    let mut prompt = base.to_string();
    if !skills.is_empty() {
        prompt.push_str(
            "\n\nAvailable skills:\n\
             If the user requests an available skill by name, use it. Otherwise, select a skill \
             by its description only if it fits the task, or proceed without one. \
             Use read with the selected skill's path to load its instructions if they are not \
             already available in context. Resolve file references relative to the directory \
             containing its SKILL.md and read them only as needed. Loading a skill does not run \
             its scripts; normal tool rules still apply.",
        );
        for skill in skills {
            prompt.push('\n');
            prompt.push_str(&serde_json::json!(skill).to_string());
        }
    }
    prompt
}

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
    fn base_prompt_is_short_with_all_tools_enabled() {
        let prompt = build_system_prompt(
            Path::new("/tmp/project"),
            &["read", "bash", "apply_patch", "grep", "jobs", "ask_user"],
            &[],
        );
        let words = prompt.split_whitespace().count();
        assert!(words < 250, "base prompt grew to {words} words");
        assert!(prompt.contains("Current working directory: /tmp/project"));
        assert!(!prompt.contains("<project_context>"));
    }

    #[test]
    fn context_files_are_embedded_eagerly() {
        let prompt = build_system_prompt(
            Path::new("/tmp/project"),
            &["read"],
            &[ContextFile {
                path: "/tmp/project/AGENTS.md".into(),
                content: "Always run cargo clippy.\n\n- Preserve existing behavior.\n".into(),
            }],
        );
        assert!(prompt.contains(concat!(
            "<project_instructions path=\"/tmp/project/AGENTS.md\">\n",
            "Always run cargo clippy.\n\n- Preserve existing behavior.\n",
            "</project_instructions>"
        )));
    }

    #[test]
    fn tool_guidance_respects_availability() {
        let cwd = Path::new("/project");
        let no_tools = build_system_prompt(cwd, &[], &[]);
        assert!(no_tools.contains("Available tools:\n(none)\n"));
        assert!(!no_tools.contains("expectedObservations"));
        for (tool, guidance) in [
            ("read", "Use read "),
            ("grep", "Use grep "),
            ("apply_patch", "Use apply_patch "),
            ("bash", "Preserve check exit statuses"),
            ("jobs", "Use jobs wait"),
            ("ask_user", "Use ask_user "),
        ] {
            assert!(!no_tools.contains(guidance), "{tool} is unavailable");
            let prompt = build_system_prompt(cwd, &[tool], &[]);
            assert!(prompt.contains(&format!("Available tools:\n- {tool}\n\n")));
            assert!(prompt.contains(guidance), "{tool} is available");
            assert!(!prompt.contains("expectedObservations"));
        }
        let edit_prompt = build_system_prompt(cwd, &["read", "apply_patch"], &[]);
        assert!(edit_prompt.contains("expectedObservations"));
    }
}
