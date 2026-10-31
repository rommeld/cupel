//! Conservative role evidence from trusted names/descriptions, never paid inference.

use crate::crew::AgentSpec;
use crate::truncate::{TruncationOptions, truncate_head};
use std::collections::BTreeSet;
use std::str::FromStr;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Review,
    Research,
    Implement,
    Test,
}
impl Role {
    pub const ALL: [Self; 4] = [Self::Review, Self::Research, Self::Implement, Self::Test];
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Research => "research",
            Self::Implement => "implement",
            Self::Test => "test",
        }
    }
    fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Review => &[
                "review",
                "reviewer",
                "reviews",
                "reviewing",
                "critic",
                "critique",
            ],
            Self::Research => &[
                "research",
                "researcher",
                "researches",
                "researching",
                "scout",
                "explore",
                "explores",
                "explorer",
                "exploring",
            ],
            Self::Implement => &[
                "implement",
                "implements",
                "implementer",
                "implementing",
                "implementation",
                "code",
                "coder",
            ],
            Self::Test => &["test", "tests", "tester", "testing"],
        }
    }
}
impl core::fmt::Display for Role {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Role {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|role|role.aliases().contains(&text.as_str())).ok_or_else(||format!("unknown role {text:?}; valid roles: review (critic), research (scout), implement (coder), test (tester)"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoleSelection {
    pub role: Role,
    pub agent: String,
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleMatch {
    None,
    Unique(RoleSelection),
    Uncertain(Vec<String>),
    Ambiguous(Vec<String>),
}
impl RoleMatch {
    pub fn selection(self, role: Role) -> Result<RoleSelection, String> {
        let result = match self {
            Self::Unique(selection) => Ok(selection),
            Self::None => Err(format!(
                "no eligible on-team {role}; add/configure a described teammate or select an agent explicitly"
            )),
            Self::Uncertain(candidates) => Err(format!(
                "uncertain {role} match; select an agent explicitly or clarify its description:\n{}",
                candidates.join("\n")
            )),
            Self::Ambiguous(candidates) => Err(format!(
                "ambiguous {role} match; select an agent explicitly:\n{}",
                candidates.join("\n")
            )),
        };
        result.map_err(|problem| {
            let cut = truncate_head(
                &problem,
                TruncationOptions {
                    max_lines: Some(30),
                    max_bytes: Some(3800),
                },
            );
            if cut.truncated {
                format!(
                    "{}\n[more evidence omitted; inspect /team roles]",
                    cut.content
                )
            } else {
                cut.content
            }
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessFilter {
    Any,
    ReadOnly,
    Writer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Supported,
    Uncertain,
}
impl Confidence {
    fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Uncertain => "uncertain",
        }
    }
}
#[derive(Debug, Clone)]
pub struct Candidate {
    pub role: Role,
    pub agent: String,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    pub writes: bool,
    pub eligible: bool,
}
#[derive(Default)]
pub struct RoleIndex {
    candidates: Vec<Candidate>,
}

impl RoleIndex {
    #[must_use]
    pub fn new<'a>(specs: impl Iterator<Item = &'a AgentSpec>) -> Self {
        let mut candidates = Vec::new();
        for spec in specs {
            let name = spec.name.to_ascii_lowercase();
            let names = tokens(&name);
            let description = spec.definition.description().unwrap_or_default();
            for role in Role::ALL {
                let mut evidence = Vec::new();
                let mut supported = false;
                let mut denied = false;
                if names.iter().any(|token| role.aliases().contains(token)) {
                    evidence.push(format!("name tokens in {:?}", spec.name));
                }
                for clause in description
                    .split(['.', ';', ',', '\n'])
                    .filter(|clause| !clause.trim().is_empty())
                {
                    let normalized = clause
                        .to_ascii_lowercase()
                        .replace("can't", "cannot")
                        .replace("don't", "not")
                        .replace("doesn't", "not");
                    let words = tokens(&normalized);
                    let negated = words.iter().enumerate().any(|(index, token)| {
                        matches!(*token, "no" | "not" | "never" | "without" | "cannot")
                            && capability(role, &words[index + 1..], false)
                    });
                    let positive = capability(role, &words, true);
                    let mention = positive
                        || negated
                        || match role {
                            Role::Review | Role::Research => {
                                words.iter().any(|token| role.aliases().contains(token))
                            }
                            Role::Implement => words
                                .iter()
                                .any(|token| matches!(*token, "implementer" | "coder")),
                            Role::Test => words.contains(&"tester"),
                        };
                    if !mention {
                        continue;
                    }
                    if negated {
                        denied = true;
                    } else if positive {
                        supported = true;
                    }
                    evidence.push(format!(
                        "description{}: {}",
                        if negated {
                            " (negated/uncertain)"
                        } else if positive {
                            ""
                        } else {
                            " (uncertain mention)"
                        },
                        short(clause.trim())
                    ));
                }
                // Incidental mentions do not overturn an affirmative capability.
                // An excluded responsibility is not a competing candidate, while
                // contradictory affirmative/negative claims remain uncertain.
                if evidence.is_empty() || (denied && !supported) {
                    continue;
                }
                evidence.truncate(8);
                candidates.push(Candidate {
                    role,
                    agent: spec.name.clone(),
                    confidence: if supported && !denied {
                        Confidence::Supported
                    } else {
                        Confidence::Uncertain
                    },
                    evidence,
                    writes: spec.definition.writes(),
                    eligible: spec.definition.description().is_some(),
                });
            }
        }
        candidates.sort_by(|a, b| (a.role, &a.agent).cmp(&(b.role, &b.agent)));
        Self { candidates }
    }
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    #[must_use]
    pub fn resolve(&self, role: Role, team: &BTreeSet<String>, access: AccessFilter) -> RoleMatch {
        let candidates = self
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.role == role
                    && candidate.eligible
                    && team.contains(&candidate.agent)
                    && match access {
                        AccessFilter::Any => true,
                        AccessFilter::ReadOnly => !candidate.writes,
                        AccessFilter::Writer => candidate.writes,
                    }
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return RoleMatch::None;
        }
        let names = candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{} ({}: {})",
                    candidate.agent,
                    candidate.confidence.label(),
                    candidate.evidence.join("; ")
                )
            })
            .collect();
        if candidates
            .iter()
            .any(|candidate| candidate.confidence == Confidence::Uncertain)
        {
            return RoleMatch::Uncertain(names);
        }
        if candidates.len() != 1 {
            return RoleMatch::Ambiguous(names);
        }
        let candidate = candidates[0];
        RoleMatch::Unique(RoleSelection {
            role,
            agent: candidate.agent.clone(),
            evidence: candidate.evidence.clone(),
        })
    }
    #[must_use]
    pub fn describe(&self, team: &BTreeSet<String>) -> String {
        if self.candidates.is_empty() {
            return "no inferred roles; define described agents in settings.json".into();
        }
        self.candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{}: {} — {}, {}, {}, {}\n  {}",
                    candidate.role,
                    candidate.agent,
                    candidate.confidence.label(),
                    if team.contains(&candidate.agent) {
                        "on-team"
                    } else {
                        "off-team"
                    },
                    if candidate.writes {
                        "can write"
                    } else {
                        "read-only"
                    },
                    if candidate.eligible {
                        "eligible"
                    } else {
                        "manual-only"
                    },
                    candidate.evidence.join("; ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[must_use]
    pub fn tool_hints(&self, members: &BTreeSet<String>) -> String {
        Self {
            candidates: self
                .candidates
                .iter()
                .filter(|candidate| members.contains(&candidate.agent))
                .cloned()
                .collect(),
        }
        .describe(members)
    }
}
fn tokens(text: &str) -> Vec<&str> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect()
}
fn short(text: &str) -> String {
    let cut = text.chars().take(256).collect::<String>();
    if cut.len() < text.len() {
        format!("{cut}…")
    } else {
        cut
    }
}
fn capability(role: Role, words: &[&str], anchored: bool) -> bool {
    let verb_position = |index: usize| {
        !anchored
            || index == 0
            || matches!(
                words[index - 1],
                "and" | "also" | "then" | "can" | "will" | "that"
            )
            || words[..index].ends_with(&["responsible", "for"])
    };
    let role_action = |verbs: &[&str]| {
        words
            .iter()
            .enumerate()
            .any(|(index, word)| verbs.contains(word) && verb_position(index))
    };
    let action = |verbs: &[&str], objects: &[&str]| {
        words.iter().enumerate().any(|(index, word)| {
            verbs.contains(word)
                && verb_position(index)
                && words[index + 1..]
                    .iter()
                    .take(3)
                    .any(|word| objects.contains(word))
        })
    };
    match role {
        Role::Review => {
            words
                .iter()
                .any(|word| matches!(*word, "reviewer" | "critic"))
                || role_action(&["review", "reviews", "reviewing", "critique"])
        }
        Role::Research => {
            words
                .iter()
                .any(|word| matches!(*word, "researcher" | "explorer" | "scout"))
                || role_action(&[
                    "research",
                    "researches",
                    "researching",
                    "explore",
                    "explores",
                    "exploring",
                ])
        }
        Role::Implement => {
            words
                .iter()
                .any(|word| matches!(*word, "implementer" | "coder"))
                || role_action(&["implement", "implements", "implementing"])
                || action(
                    &[
                        "write",
                        "writes",
                        "writing",
                        "edit",
                        "edits",
                        "editing",
                        "modify",
                        "modifies",
                        "modifying",
                    ],
                    &["code", "changes", "patches"],
                )
        }
        Role::Test => {
            words.contains(&"tester")
                || words
                    .first()
                    .is_some_and(|word| matches!(*word, "tests" | "testing"))
                || action(
                    &["write", "writes", "writing", "run", "runs", "running"],
                    &["test", "tests"],
                )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRoute {
    Lead(Option<String>),
    Explicit(String),
    Discovered(RoleSelection),
    Ordinary,
}
#[must_use]
pub fn command_route(
    command: &str,
    explicit: bool,
    validated: Option<&String>,
    index: &RoleIndex,
    team: &BTreeSet<String>,
) -> CommandRoute {
    if explicit {
        return validated.map_or_else(
            || {
                CommandRoute::Lead(Some(format!(
                    "/{command}: invalid explicit agentCommands binding; using the lead"
                )))
            },
            |name| CommandRoute::Explicit(name.clone()),
        );
    }
    let Ok(role) = Role::from_str(command) else {
        return CommandRoute::Ordinary;
    };
    match index.resolve(role, team, AccessFilter::Any) {
        RoleMatch::Unique(selection) => CommandRoute::Discovered(selection),
        RoleMatch::None => CommandRoute::Lead(None),
        problem => CommandRoute::Lead(problem.selection(role).err().map(|reason| {
            format!("/{command}: {reason}; using the lead; set agentCommands.{command} explicitly")
        })),
    }
}

#[cfg(test)]
mod tests {
    use crate::roles::*;
    use crate::settings::{AgentDefinition, AgentOrigin, Preset, ThinkingSetting, ToolName};

    fn spec(name: &str, description: &str, writes: bool) -> AgentSpec {
        let model = cupel_core::catalog::builtin_models().remove(0);
        AgentSpec {
            name: name.into(),
            definition: AgentDefinition {
                preset: Preset {
                    provider: model.provider.as_str().into(),
                    model: model.id.clone(),
                    thinking_level: ThinkingSetting::Off,
                    prompt: None,
                },
                description: (!description.is_empty()).then(|| description.into()),
                tools: writes.then(|| vec![ToolName::Bash]),
                max_turns: None,
                remember: None,
                team: None,
                origin: AgentOrigin::Home,
            },
            model,
        }
    }
    fn team(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).into()).collect()
    }

    #[test]
    fn descriptions_support_arbitrary_names_without_construction_or_tool_grants() {
        let specs = [
            spec("lens_42", "Reviews changes for correctness bugs.", false),
            spec("green", "Writes tests.", true),
            spec("blue", "Implements code.", false),
        ];
        let index = RoleIndex::new(specs.iter());
        let selected = index
            .resolve(Role::Review, &team(&["lens_42"]), AccessFilter::Any)
            .selection(Role::Review)
            .unwrap();
        assert_eq!(selected.agent, "lens_42");
        assert!(selected.evidence[0].contains("Reviews changes"));
        assert!(matches!(
            index.resolve(Role::Review, &team(&[]), AccessFilter::Any),
            RoleMatch::None
        ));
        assert!(matches!(
            index.resolve(Role::Implement, &team(&["blue"]), AccessFilter::Writer),
            RoleMatch::None
        ));
        assert!(matches!(
            index.resolve(Role::Implement, &team(&["blue"]), AccessFilter::ReadOnly),
            RoleMatch::Unique(_)
        ));
        assert!(matches!(
            index.resolve(Role::Test, &team(&["green"]), AccessFilter::Writer),
            RoleMatch::Unique(_)
        ));
    }

    #[test]
    fn names_alone_negations_and_incidental_mentions_never_auto_run() {
        let specs = [
            spec("reviewer", "Finds files.", false),
            spec("red", "Reviews changes. Does not review code.", false),
            spec("lens", "Reviews research tools.", false),
            spec("finder", "Researches how to implement code.", false),
        ];
        let index = RoleIndex::new(specs.iter());
        assert!(matches!(
            index.resolve(Role::Review, &team(&["reviewer"]), AccessFilter::Any),
            RoleMatch::Uncertain(_)
        ));
        assert!(matches!(
            index.resolve(Role::Review, &team(&["red"]), AccessFilter::Any),
            RoleMatch::Uncertain(_)
        ));
        assert!(!matches!(
            index.resolve(Role::Research, &team(&["lens"]), AccessFilter::Any),
            RoleMatch::Unique(_)
        ));
        assert!(!matches!(
            index.resolve(Role::Implement, &team(&["finder"]), AccessFilter::Any),
            RoleMatch::Unique(_)
        ));
    }

    #[test]
    fn affirmative_responsibilities_survive_mentions_and_excluded_other_roles() {
        let specs = [
            spec(
                "developer",
                "The Developer is responsible for implementing code changes.",
                true,
            ),
            spec(
                "reviewer",
                "Reviews substantial code diffs. Writes an actionable Markdown review with code evidence; does not implement fixes.",
                true,
            ),
        ];
        let index = RoleIndex::new(specs.iter());
        let members = team(&["developer", "reviewer"]);
        for (role, expected) in [(Role::Implement, "developer"), (Role::Review, "reviewer")] {
            let selection = index
                .resolve(role, &members, AccessFilter::Writer)
                .selection(role)
                .unwrap();
            assert_eq!(selection.agent, expected);
        }
        assert!(matches!(
            index.resolve(Role::Review, &members, AccessFilter::ReadOnly),
            RoleMatch::None
        ));
    }

    #[test]
    fn negation_applies_to_the_excluded_capability() {
        let specs = [spec("lens", "Reviews diffs without editing code.", false)];
        let index = RoleIndex::new(specs.iter());
        let members = team(&["lens"]);
        assert!(matches!(
            index.resolve(Role::Review, &members, AccessFilter::Any),
            RoleMatch::Unique(_)
        ));
        assert!(matches!(
            index.resolve(Role::Implement, &members, AccessFilter::Any),
            RoleMatch::None
        ));
    }

    #[test]
    fn ambiguity_and_live_membership_are_explicit() {
        let specs = [
            spec("one", "Reviews diffs.", false),
            spec("two", "Reviews diffs.", false),
            spec("review", "", false),
        ];
        let index = RoleIndex::new(specs.iter());
        assert!(matches!(
            index.resolve(Role::Review, &team(&["one", "two"]), AccessFilter::Any),
            RoleMatch::Ambiguous(_)
        ));
        assert!(matches!(
            index.resolve(Role::Review, &team(&["one"]), AccessFilter::Any),
            RoleMatch::Unique(_)
        ));
        assert!(matches!(
            index.resolve(Role::Review, &team(&["review"]), AccessFilter::Any),
            RoleMatch::None
        ));
        assert!(index.describe(&team(&["one"])).contains("off-team"));
    }

    #[test]
    fn explicit_choices_win_even_when_invalid_and_aliases_are_parseable() {
        let specs = [spec("lens", "Reviews diffs.", false)];
        let index = RoleIndex::new(specs.iter());
        let members = team(&["lens"]);
        assert_eq!(
            command_route("review", true, Some(&"off-team".into()), &index, &members),
            CommandRoute::Explicit("off-team".into())
        );
        assert!(matches!(
            command_route("review", true, None, &index, &members),
            CommandRoute::Lead(Some(_))
        ));
        assert!(matches!(
            command_route("review", false, None, &index, &members),
            CommandRoute::Discovered(_)
        ));
        assert_eq!(
            command_route("explain", false, None, &index, &members),
            CommandRoute::Ordinary
        );
        assert_eq!("critic".parse::<Role>().unwrap(), Role::Review);
        assert_eq!("TESTER".parse::<Role>().unwrap(), Role::Test);
        assert!(
            "unknown"
                .parse::<Role>()
                .unwrap_err()
                .contains("valid roles")
        );
    }
}
