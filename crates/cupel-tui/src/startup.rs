//! Typed startup commands, dispatched through the ordinary TUI handlers once.

use cupel_coding_agent::commands::TeamCommand;
use std::iter::Peekable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupCommand {
    Team(TeamCommand),
    Agent { name: String, task: Option<String> },
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamInitialization {
    ConfiguredDefaults,
    Restore,
}

impl StartupCommand {
    pub fn parse(
        flag: &str,
        args: &mut Peekable<impl Iterator<Item = String>>,
        action: &mut Option<Self>,
    ) -> Result<(), String> {
        if flag == "--add" {
            let Some(Self::Team(command)) = action else {
                return Err("--add requires --team".into());
            };
            let names = take_words(args);
            if names.is_empty() {
                return Err("--add requires at least one agent name".into());
            }
            match command {
                TeamCommand::List => *command = TeamCommand::Add(names),
                TeamCommand::Add(existing) => existing.extend(names),
                _ => return Err("only listing and adding are supported at startup".into()),
            }
            return Ok(());
        }
        if action.is_some() {
            return Err("choose one startup command: --team, --agent, or --review".into());
        }
        *action = Some(match flag {
            "--team" => Self::Team(TeamCommand::List),
            "--review" => Self::Review,
            "--agent" => {
                let mut words = take_words(args).into_iter();
                let name = words.next().ok_or("--agent requires an agent name")?;
                let task = words.collect::<Vec<_>>().join(" ");
                Self::Agent {
                    name,
                    task: (!task.trim().is_empty()).then_some(task),
                }
            }
            _ => return Err(format!("unknown startup argument: {flag}")),
        });
        Ok(())
    }
}

fn take_words(args: &mut Peekable<impl Iterator<Item = String>>) -> Vec<String> {
    let mut words = Vec::new();
    while let Some(word) = args.peek() {
        if word == "--" {
            args.next();
            words.extend(args);
            break;
        }
        if word.starts_with('-') {
            break;
        }
        if let Some(word) = args.next() {
            words.push(word);
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use crate::startup::*;

    fn parse(words: &[&str]) -> Result<Option<StartupCommand>, String> {
        let mut args = words.iter().map(ToString::to_string).peekable();
        let mut action = None;
        while let Some(flag) = args.next() {
            StartupCommand::parse(&flag, &mut args, &mut action)?;
        }
        Ok(action)
    }

    #[test]
    fn forms_and_conflicts() {
        assert_eq!(
            parse(&["--team"]).unwrap(),
            Some(StartupCommand::Team(TeamCommand::List))
        );
        assert_eq!(parse(&["--review"]).unwrap(), Some(StartupCommand::Review));
        assert_eq!(
            parse(&["--team", "--add", "one", "two", "--add", "three"]).unwrap(),
            Some(StartupCommand::Team(TeamCommand::Add(vec![
                "one".into(),
                "two".into(),
                "three".into()
            ])))
        );
        assert_eq!(
            parse(&["--agent", "one"]).unwrap(),
            Some(StartupCommand::Agent {
                name: "one".into(),
                task: None
            })
        );
        assert_eq!(
            parse(&["--agent", "one", "--", "check", "--review"]).unwrap(),
            Some(StartupCommand::Agent {
                name: "one".into(),
                task: Some("check --review".into())
            })
        );
        for invalid in [
            &["--add", "one"][..],
            &["--team", "--add"],
            &["--agent"],
            &["--agent", "--review"],
            &["--team", "--review"],
            &["--team", "--team"],
        ] {
            assert!(parse(invalid).is_err());
        }
    }
}
