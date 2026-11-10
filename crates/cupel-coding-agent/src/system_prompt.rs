//! System prompt construction, tool list, per tool guidelines

use std::path::Path;

use crate::resources::ContextFile;

#[must_use]
pub fn build_system_prompt(
    cwd: &Path,
    tools: &[(&str, &str)],
    context_files: &[ContextFile],
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
    guidelines.push("Establish the repository layout before guessing source directories.");
    if has("grep") {
        guidelines.push("Use grep to locate code before answering questions about it");
        guidelines.push(
            "Use grep outputMode=files to find which files define or use a name before reading them",
        );
    }
    if has("read") {
        guidelines.push("Use read to examine files instead of cat or sed");
        guidelines.push(
            "Read headers identify the observed source bytes and actual returned range. \
            Partial views are not complete code units; search locations are navigation hints, \
            not source revisions. Do not skip required policy text or necessary rereads.",
        );
    }
    if has("apply_patch") {
        guidelines.push(
            "Use apply_patch for all file changes: create, delete, or rename files and to \
            change several files at once; give every hunk 3 lines of context so it is \
            unique in the file",
        );
        guidelines.push(
            "Use distinctive named @@ anchors for nontrivial patches, repeated blocks, and large \
            files. Ambiguous targets are rejected. Re-establish the current target text after \
            formatting or other relevant mutations; do not guess context or loosen matching.",
        );
        if has("read") {
            guidelines.push(
                "For edits that depend on inspected source, pass read observation ids in \
                apply_patch expectedObservations. These optional whole-file preconditions \
                reject stale bytes, including unrelated edits; unavailable ids require a new read.",
            );
        }
        guidelines.push(
            "Before a nontrivial edit, briefly state the intended behavior change, what must \
            remain unchanged, and what evidence would expose a mistake. This is reasoning, \
            not runtime proof or a mandatory approval step.",
        );
    }
    if has("bash") {
        guidelines.push(
            "Preserve validation command exit statuses: avoid piping checks through tail/head \
            without pipefail or explicitly capturing the original status. Confirm the intended \
            tests actually ran; exit zero or an empty test selection is not proof of correctness.",
        );
        guidelines.push(
            "Parallelize independent work, not competing builds on the same Cargo target or \
            reads that race with formatting/edits. Do not assume larger batches are faster.",
        );
    }
    if has("ask_user") {
        guidelines.push(
            "Use ask_user when a design decision is the user's to make and several options \
            are reasonable, and recommend one of them; don't ask what the code, the project \
            instructions, or an earlier answer already tells you.",
        );
    }
    guidelines.push("Be concise in your responses");
    guidelines.push("Explain non-obvious decisions and risks; skip routine status updates.");
    guidelines.push("Never apologize; acknowledge mistakes directly.");
    guidelines.push("Show file paths clearly when working with files");
    guidelines.push(
        "In the final answer distinguish applied changes, observed check outcomes, and \
        requirement-level conclusions. Keep missing results, stale evidence, ambiguous targets, \
        and unverified cases explicit; earlier checks may no longer apply after later edits.",
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
        "You are an expert coding assistant operating inside cupel, a coding agent harness. \
You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

Guidelines:
{guidelines}"
    );

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

    prompt.push_str(&format!(
        "\n\nCurrent date: {date}\nCurrent working directory: {prompt_cwd}"
    ));
    prompt
}

/// Extend the system prompt with a preset's `prompt`, base, a blank line,
/// the prompt. No prompt (or an empty one) returns `base` unchanged, which
/// is also how a switch to a preset without a prompt drops the previous one.
#[must_use]
pub fn with_preset_prompt(base: &str, preset_prompt: Option<&str>) -> String {
    match preset_prompt.filter(|prompt| !prompt.is_empty()) {
        Some(prompt) => format!("{base}\n\n{prompt}"),
        None => base.to_string(),
    }
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
    fn includes_tools_and_cwd() {
        let prompt = build_system_prompt(
            Path::new("/tmp/project"),
            &[("grep", "Search file contents for patterns")],
            &[],
        );
        assert!(prompt.contains("- grep: Search file contents for patterns"));
        assert!(prompt.contains("Current working directory: /tmp/project"));
        assert!(
            prompt.contains(
                "- Explain non-obvious decisions and risks; skip routine status updates."
            )
        );
        assert!(prompt.contains("- Never apologize; acknowledge mistakes directly."));
        // Empty inputs add no empty sections.
        assert!(!prompt.contains("<project_context>"));
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
        );
        assert!(prompt.contains("<project_instructions path=\"/tmp/project/AGENTS.md\">"));
        assert!(prompt.contains("Always run cargo clippy."));
    }

    #[test]
    fn coding_guidance_connects_navigation_observations_edits_and_checks() {
        let prompt = build_system_prompt(
            Path::new("/project"),
            &[
                ("read", "Read files"),
                ("grep", "Search"),
                ("apply_patch", "Patch"),
                ("bash", "Shell"),
            ],
            &[],
        );
        for instruction in [
            "Establish the repository layout",
            "distinctive named @@ anchors",
            "Ambiguous targets are rejected",
            "expectedObservations",
            "formatting or other relevant mutations",
            "not runtime proof",
            "pipefail",
            "Confirm the intended tests actually ran",
            "Parallelize independent work",
            "unverified cases explicit",
        ] {
            assert!(prompt.contains(instruction), "missing: {instruction}");
        }
        let no_tools = build_system_prompt(Path::new("/project"), &[], &[]);
        assert!(!no_tools.contains("expectedObservations"));
        assert!(!no_tools.contains("pipefail"));
    }

    #[test]
    fn civil_date_known_value() {
        // 2026 07 03 is 20_637 days after the epoch.
        assert_eq!(civil_from_days(20_637), (2026, 7, 3));
    }

    #[test]
    fn a_preset_prompt_is_appended_only_when_defined() {
        assert_eq!(
            with_preset_prompt("BASE", Some("Be brief.")),
            "BASE\n\nBe brief."
        );
        assert_eq!(with_preset_prompt("BASE", None), "BASE");
        assert_eq!(with_preset_prompt("BASE", Some("")), "BASE");
    }
}
