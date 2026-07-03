//! System prompt construction. Simplified port of pi's `system-prompt.ts`:
//! pi additionally injects project context files (AGENTS.md), skills, and
//! per-tool guidelines for its seven tools - those return with the tools
//! that need them.

use std::path::Path;

/// Build the system prompt for the grep-only coding agent.
#[must_use]
pub fn build_system_prompt(cwd: &Path, tools: &[(&str, &str)]) -> String {
    let tools_list = if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools
            .iter()
            .map(|(name, snippet)| format!("- {name}: {snippet}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Forward slashes even on Windows: models handle them more reliably.
    let prompt_cwd = cwd.display().to_string().replace('\\', "/");
    let date = current_date();

    format!(
        "You are an expert coding assistant operating inside cupel, a coding agent harness. \
You help users by searching and reasoning about code.

Available tools:
{tools_list}

Guidelines:
- Use grep to locate code before answering questions about it
- Be concise in your responses
- Show file paths clearly when working with files

Current date: {date}
Current working directory: {prompt_cwd}"
    )
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
        );
        assert!(prompt.contains("- grep: Search file contents for patterns"));
        assert!(prompt.contains("Current working directory: /tmp/project"));
    }

    #[test]
    fn civil_date_known_value() {
        // 2026-07-03 is 20_637 days after the epoch.
        assert_eq!(civil_from_days(20_637), (2026, 7, 3));
    }
}
