//! Token accounting from finalized replies and persisted session transcripts.

use std::path::Path;
use std::{
    collections::BTreeMap,
    io::{BufRead as _, BufReader},
};

use cupel_agent::AgentMessage;
use cupel_core::types::{AssistantMessage, Message, Usage};

use crate::session::{TRANSCRIPT_VERSION, TranscriptHeader};

/// The requested model, qualified by provider so identical IDS do not collide.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelKey {
    pub provider: String,
    pub model: String,
}

/// Matches the footer: cached means cache reads, not cache writes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cost: f64,
}

impl UsageTotals {
    fn add_usage(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cost += usage.cost.total;
    }

    fn merge(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cost += other.cost;
    }
}

/// Sorted rows; totals are derived from the same rows rather than counted twice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageByModel {
    pub models: BTreeMap<ModelKey, UsageTotals>,
}

impl UsageByModel {
    /// Count reported usage even on an errored or aborted reply.
    pub fn record(&mut self, assistant: &AssistantMessage) {
        self.record_usage(
            assistant.provider.as_str(),
            &assistant.model,
            &assistant.usage,
        );
    }

    pub fn record_usage(&mut self, provider: &str, model: &str, usage: &Usage) {
        let key = ModelKey {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        self.models.entry(key).or_default().add_usage(usage);
    }

    pub fn merge(&mut self, other: &Self) {
        for (key, totals) in &other.models {
            self.models.entry(key.clone()).or_default().merge(totals);
        }
    }

    #[must_use]
    pub fn totals(&self) -> UsageTotals {
        self.models
            .values()
            .fold(UsageTotals::default(), |mut sum, row| {
                sum.merge(row);
                sum
            })
    }
}

/// Partial results are usable, but their warnings must remain visible.
#[derive(Debug, Default)]
pub struct HistoryUsage {
    pub usage: UsageByModel,
    pub sessions: usize,
    pub warnings: Vec<String>,
}

impl HistoryUsage {
    fn warn(&mut self, warning: String) {
        tracing::warn!("{warning}");
        self.warnings.push(warning);
    }
}

/// All projects and archives under `<home>/sessions`, except the active file. The
/// caller adds its in-memory current-session snapshot exactly once. No environment
/// lookup, directory creation, symlink traversal, or writes.
#[must_use]
pub fn load_history(home: Option<&Path>, exclude: Option<&Path>) -> HistoryUsage {
    let mut history = HistoryUsage::default();
    let Some(home) = home else {
        history.warn("No cupel home: historical usage is unavailable".to_string());
        return history;
    };
    scan_dir(&home.join("sessions"), exclude, &mut history);
    history
}

fn scan_dir(dir: &Path, exclude: Option<&Path>, history: &mut HistoryUsage) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            history.warn(format!("Cannot list {}: {error}", dir.display()));
            return;
        }
    };
    // Stable ordering makes floating-point cost sums and warning order repeatable.
    let mut entries: Vec<_> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                history.warn(format!("Cannot read entry in {}: {error}", dir.display()));
                None
            }
        })
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                history.warn(format!("Cannot inspect {}: {error}", path.display()));
                continue;
            }
        };
        if kind.is_dir() {
            scan_dir(&path, exclude, history);
        } else if kind.is_file()
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && exclude != Some(path.as_path())
        {
            match read_usage(&path) {
                Ok((usage, warnings)) => {
                    history.usage.merge(&usage);
                    history.sessions += 1;
                    for warning in warnings {
                        history.warn(warning);
                    }
                }
                Err(error) => history.warn(error),
            }
        }
    }
}

/// Stream one file; do not retain its conversation content in memory.
fn read_usage(path: &Path) -> Result<(UsageByModel, Vec<String>), String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("Empty transcript: {}", path.display()))?
        .map_err(|error| format!("Cannot read header in {}: {error}", path.display()))?;
    let header: TranscriptHeader = serde_json::from_str(&header)
        .map_err(|error| format!("Invalid header in {}: {error}", path.display()))?;
    if header.version != TRANSCRIPT_VERSION {
        return Err(format!(
            "Unsupported transcript version {} in {}",
            header.version,
            path.display()
        ));
    }
    let mut usage = UsageByModel::default();
    let mut warnings = Vec::new();
    for (index, line) in lines.enumerate() {
        let line_number = index + 2;
        let line = line
            .map_err(|error| format!("Cannot read {}: {line_number}: {error}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AgentMessage>(&line) {
            Ok(AgentMessage::Llm(Message::Assistant(assistant))) => usage.record(&assistant),
            Ok(AgentMessage::Custom { kind, payload, .. }) if kind == "cupel.compaction-usage" => {
                if let (Some(provider), Some(model), Ok(tokens)) = (
                    payload["provider"].as_str(),
                    payload["model"].as_str(),
                    serde_json::from_value::<Usage>(payload["usage"].clone()),
                ) {
                    usage.record_usage(provider, model, &tokens);
                }
            }
            Ok(_) => {}
            Err(error) => {
                warnings.push(format!("Skipped {}:{line_number}: {error}", path.display()));
            }
        }
    }
    Ok((usage, warnings))
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
