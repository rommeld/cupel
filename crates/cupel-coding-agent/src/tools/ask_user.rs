//! The `ask_user` tool, the agent stops at a design decision and lets the user decide.
//!
//! The model sends one question with at least two options and marks the one it
//! recommends. The tool hands the question to the frontend and waits until the user
//! answers, with one of the options, or in their own words. Esc aborts the run, and
//! with it the wait.
//!
//! The tool reaches the frontend over a channel. `new` returns the tool
//! together with the channel's receiving end, and whoever keeps the receiver answers.
//! Only the TUI can, so only the TUI gets the tool
//! (see `Frontend`).

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode, ToolUpdateFn};

/// One option is no choice.
const MIN_OPTIONS: usize = 2;

/// More options than this area hard to weigh at a glance.
const MAX_OPTIONS: usize = 5;

/// What the model asks. Deserializing the tool's arguments into this struct checks
/// their shape, `problem` checks the rest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Question {
    pub question: String,
    pub options: Vec<Choice>,
}

/// One way forward the user can pick.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Choice {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub recommended: bool,
}

impl Question {
    /// What is wrong with the question, in words the model can act on, or `None`
    /// when it can be asked. The schema says the same, but models don't always
    /// follow schemas.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if self.question.trim().is_empty() {
            return Some("the question is empty".to_string());
        }
        let count = self.options.len();
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&count) {
            return Some(format!(
                "offer {MIN_OPTIONS} to {MAX_OPTIONS} options, not {count}"
            ));
        }
        if self
            .options
            .iter()
            .any(|choice| choice.label.trim().is_empty())
        {
            return Some("every option needs a label".to_string());
        }
        let recommended = self
            .options
            .iter()
            .filter(|choice| choice.recommended)
            .count();
        if recommended != 1 {
            return Some(format!(
                "mark exactly one option as recommended, not {recommended}"
            ));
        }
        None
    }

    /// The index of the recommended option. A frontend starts its selection there.
    #[must_use]
    pub fn recommended(&self) -> usize {
        self.options
            .iter()
            .position(|choice| choice.recommended)
            .unwrap_or(0)
    }
}

/// How the user answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Picked(usize),
    Typed(String),
}

/// A question on its way to the user, with the channel its answer goes back on.
pub struct PendingQuestion {
    question: Question,
    reply: oneshot::Sender<Answer>,
}

impl PendingQuestion {
    /// A question, plus the receiving end its answer arrives on. The tool waits on
    /// that receiver.
    #[must_use]
    pub fn new(question: Question) -> (Self, oneshot::Receiver<Answer>) {
        let (reply, answer) = oneshot::channel();
        (Self { question, reply }, answer)
    }

    /// The question, for the frontend to show.
    #[must_use]
    pub fn question(&self) -> &Question {
        &self.question
    }

    /// Send the answer back. It takes `self` by value, a question is answered once,
    /// and the compiler refects a second call.
    pub fn answer(self, answer: Answer) {
        // The receiver is gone when the run was aborted in the meantime. Then nobody
        // waits for the answer anymore.
        let _ = self.reply.send(answer);
    }
}

/// THe receiving end of the tool's questions. The frontend keeps it.
pub type QuestionReceiver = mpsc::UnboundedReceiver<PendingQuestion>;

pub struct AskUserTool {
    questions: mpsc::UnboundedSender<PendingQuestion>,
    description: String,
}

impl AskUserTool {
    /// The tool and the receiver its questions arrive on. They are made together,
    /// so every tool starts out with someone who can answer.
    #[must_use]
    pub fn new() -> (Self, QuestionReceiver) {
        let (questions, receiver) = mpsc::unbounded_channel();
        let tool = Self {
            questions,
            description: format!(
                "Ask the user to decide a desgin question before you go on. Use it when \
                several approaches are reasonable and the choice is the user's to make: \
                architecture, trade-offs, behavior users will notice, scope. Offer \
                {MIN_OPTIONS} to {MAX_OPTIONS} options, each with a short label and a \
                one-sentence description of what it means costs, and mark exactly one as \
                recommended. The user picks an option or answers in their own words; the \
                answer is this tool's result. Don't ask what the code or the project \
                instructions already answer, and don't ask for permission to do routine \
                work."
            ),
        };
        (tool, receiver)
    }
}

#[async_trait::async_trait]
impl AgentTool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // Kept in sync with `Question` and `Choice` by hand.
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The decision to make, as one question"
                },
                "options": {
                    "type": "array",
                    "minItems": MIN_OPTIONS,
                    "maxItems": MAX_OPTIONS,
                    "description": "The ways forward",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "A few words that name the option"
                            },
                            "description": {
                                "type": "string",
                                "description": "What the option means and what it costs, in one to two sentences"
                            },
                            "recommended": {
                                "type": "boolean",
                                "description": "true for the one option you recommend, false for the others"
                            }
                        },
                        "required": ["label", "description", "recommended"]
                    }
                }
            },
            "required": ["question", "options"]
        })
    }

    /// One question at a time, and never next to a running command, the loop runs a
    /// batch with this tool in it one call after the other.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    /// the ask prefix, a colon and the question.
    fn describe_call(&self, args: &Value) -> String {
        match args.get("question").and_then(Value::as_str) {
            Some(question) => format!("ask: {question}"),
            None => self.name().to_string(),
        }
    }

    /// A user's answer can't be had again by asking again.
    fn reproducible(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let question: Question = serde_json::from_value(args)?;
        if let Some(problem) = question.problem() {
            return Err(problem.into());
        }
        let (pending, answer) = PendingQuestion::new(question.clone());
        // Sending fails only when the frontend dropped its receiver.
        self.questions
            .send(pending)
            .map_err(|_unsent| "nobody can answer questions in this session")?;
        // Wait for the answer or for Esc, whichever comes first. `biased` checks the
        // branches in order, so when both are ready, Esc wins.
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err("the user aborted the run instead of answering".into()),
                answer = answer => {
                    let answer = answer.map_err(|_closed| "the question closed wihtout a answer")?;
                    Ok(AgentToolResult::text(answer_text(&question, &answer)))
                }
        }
    }
}

/// The tool result the model reads.
fn answer_text(question: &Question, answer: &Answer) -> String {
    match answer {
        Answer::Picked(index) => {
            let label = question
                .options
                .get(*index)
                .map_or("", |choice| choice.label.as_str());
            format!("The user picked option {}: {label}", index + 1)
        }
        Answer::Typed(text) => format!("The user answered in their own words:\n{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cupel_core::types::ToolResultContent;

    /// Arguments as the model sends them, two options, the first recommended.
    fn presets() -> Value {
        json!({
            "question": "Where should presets live?",
            "options": [
                {"label": "settings.json", "description": "No new file.", "recommended": true},
                {"label": "presets.toml", "description": "A second loader.", "recommended": false}
            ]
        })
    }

    fn question(args: Value) -> Question {
        serde_json::from_value(args).unwrap()
    }

    fn text(result: &AgentToolResult) -> &str {
        match &result.content[0] {
            ToolResultContent::Text(text) => &text.text,
            ToolResultContent::Image(_) => panic!("expected text"),
        }
    }

    #[test]
    fn a_question_needs_two_options_and_one_recommendation() {
        assert_eq!(question(presets()).problem(), None);

        let mut one_option = presets();
        one_option["options"].as_array_mut().unwrap().pop();
        assert_eq!(
            question(one_option).problem().as_deref(),
            Some("offer 2 to 5 options, not 1")
        );

        let mut two_picks = presets();
        two_picks["options"][1]["recommended"] = json!(true);
        assert_eq!(
            question(two_picks).problem().as_deref(),
            Some("mark exactly one option as recommended, not 2")
        );

        let mut no_pick = presets();
        no_pick["options"][0]["recommended"] = json!(false);
        assert_eq!(
            question(no_pick).problem().as_deref(),
            Some("mark exactly one option as recommended, not 0")
        );
    }

    #[tokio::test]
    async fn the_picked_option_comes_back_as_the_result() {
        let (tool, mut questions) = AskUserTool::new();
        // Play the user in a second task, wait for the question, pick option 2.
        let user = tokio::spawn(async move {
            let pending = questions.recv().await.unwrap();
            assert_eq!(pending.question().recommended(), 0);
            pending.answer(Answer::Picked(1));
        });
        let result = tool
            .execute("call_1", presets(), CancellationToken::new(), None)
            .await
            .unwrap();
        user.await.unwrap();
        assert_eq!(text(&result), "The user picked option 2: presets.toml");
    }

    #[test]
    fn a_typed_answer_reaches_the_model_in_full() {
        let answer = Answer::Typed("Neither. Use environment variables.".to_string());
        assert_eq!(
            answer_text(&question(presets()), &answer),
            "The user answered in their own words:\nNeither. Use environment variables."
        );
    }

    #[tokio::test]
    async fn esc_ends_the_wait() {
        let (tool, mut questions) = AskUserTool::new();
        let cancel = CancellationToken::new();
        let esc = cancel.clone();
        let user = tokio::spawn(async move {
            let pending = questions.recv().await.unwrap();
            esc.cancel();
            // Returned, not dropped, the question stays open, so only the
            // cancel can end the wait.
            pending
        });
        // A broken cancel would wait forever. The timeout turns that into a
        // failure.
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.execute("call_1", presets(), cancel, None),
        )
        .await
        .expect("Esc ends the wait")
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the user aborted the run instead of answering"
        );
        drop(user.await.unwrap());
    }

    #[tokio::test]
    async fn a_bad_question_never_reaches_the_user() {
        let (tool, questions) = AskUserTool::new();
        // Nobody listens, a question that got past the check would fail with
        // "nobody can answer ..." instead of the problem.
        drop(questions);
        let args = json!({"question": "Which one?", "options": []});
        let error = tool
            .execute("call_1", args, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "offer 2 to 5 options, not 0");
    }

    #[test]
    fn the_tool_runs_alone_and_its_answers_survive_compaction() {
        let (tool, _questions) = AskUserTool::new();
        assert_eq!(tool.execution_mode(), Some(ToolExecutionMode::Sequential));
        assert!(!tool.reproducible());
    }

    #[test]
    fn the_description_is_one_line() {
        // A string literal wrapped without `\` at a line end would put the line
        // break and the next line's indentation into the text.
        let (tool, _questions) = AskUserTool::new();
        assert!(!tool.description().contains('\n'));
        assert!(!tool.description().contains("  "));
    }
}
