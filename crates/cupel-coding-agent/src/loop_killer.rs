//! The loop killer: blocks a tool call that repeats backt-to-back with
//! IDENTICAL argumetns more often than settings allow, and steers the
//! model onto a new track va the error text it receives instead of a
//! result.
//!
//! Detection is deliberately CONSECUTIVE-only: `read x, edit x, read x`
//! is legitimate verify loop and must stay allowed - any different
//! call resets the chain. The canoncial key is `name + argumetns JSON`:
//! this workspace's serde_json is built without preserve_order, so
//! object keys serialize sorted and two structurally equal argument
//! object always yield the same string, regardless of the field order
//! the model streamed.
//!
//! Why a veto (and not the alternatives): injecting a steering message
//! would be silently dropped (the Agent's RunHooks decorator does not
//! forward steering_messages, yet), and should_stop_after_turn would
//! end the run without any event - a silent stop the user cannot
//! distinguish from a finished answer. The before_tool_call veto is
//! the machnism the bash guard already proves out: the model sees an
//! error tool result and the run continues, redirected.

use std::sync::Mutex;

use cupel_core::types::ToolCall;

/// Consecutive-repeat tracker. One per session, shared across runs (a
/// /hot-reald rebuilds it, which also resets the chain).
pub struct LoopKiller {
    /// None = disabled (no loopKiller section, or maxRepeats 0).
    limit: Option<u32>,
    state: Mutex<RepeatState>,
}

#[derive(Default)]
struct RepeatState {
    /// Canonical `name\n{args}` of the most recent call.
    last_key: Option<String>,
    /// How many times in a row that key has been seen, counting the
    /// first occurrence.
    seen: u32,
}

impl LoopKiller {
    #[must_use]
    pub fn new(limit: Option<u32>) -> Self {
        Self {
            limit,
            state: Mutex::new(RepeatState::default()),
        }
    }

    /// Record one request tool call; Some(reason) = block this one.
    ///
    /// With maxRepeats = N the first N identical consecutive calls run;
    /// the N+1th (and every further identical repeat) is blocked.
    /// Blocked attempts keep counting - hammering the same call after a
    /// block stays blocked until the model tries something else.
    pub fn note_call(&self, tool_call: &ToolCall) -> Option<String> {
        // Disabled killer: not even the bookkeeping runs.
        let limit = self.limit?;
        let key = format!("{}\n{}", tool_call.name, tool_call.arguments);
        let seen = {
            // Tight lock scope: compare-and-count only, no I/O, no await.
            let mut state = self.state.lock().expect("loop killer state poisoned");
            if state.last_key.as_deref() == Some(key.as_str()) {
                state.seen += 1;
            } else {
                state.last_key = Some(key);
                state.seen = 1;
            }
            state.seen
        };
        (seen > limit).then(|| {
            format!(
                "Loop killer: this exact call ({} with identical arguments) has now
                been \
                requested {seen} times in a row; settings allow {limit} repeats \
                (loopKiller.maxRepeats). Repeating it will keep producing the same \
                result. Take a DIFFERENT approach: change the arguments, use another
                \
                tool, work with what you already learned, or ask user how to \
                proceed.",
                tool_call.name
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn allows_up_to_the_limit_then_blocks() {
        let killer = LoopKiller::new(Some(2));
        let c = call("read", serde_json::json!({"path": "x"}));
        assert!(killer.note_call(&c).is_none());
        assert!(killer.note_call(&c).is_none());
        let reason = killer.note_call(&c).expect("third identical call blocks");
        assert!(reason.contains("Loop killer"), "{reason}");
        assert!(
            killer.note_call(&c).is_some(),
            "stays blocked while hammering"
        );
    }

    #[test]
    fn a_different_call_resets_the_chain() {
        let killer = LoopKiller::new(Some(1));
        let a = call("read", serde_json::json!({"path": "a"}));
        let b = call("read", serde_json::json!({"path": "b"}));
        assert!(killer.note_call(&a).is_none());
        assert!(killer.note_call(&b).is_none(), "different args = new chain");
        assert!(killer.note_call(&a).is_none(), "read-edit-read stays legal");
        assert!(killer.note_call(&a).is_some(), "now a consecutive repeat");
    }

    #[test]
    fn key_ignores_argument_order_and_call_id() {
        let killer = LoopKiller::new(Some(1));
        let mut first = call("grep", serde_json::json!({"pattern": "x", "path": "src"}));
        first.id = "call_1".into();
        let mut second = call("grep", serde_json::json!({"path": "src", "pattern": "x"}));
        second.id = "call_2".into();
        assert!(killer.note_call(&first).is_none());
        assert!(
            killer.note_call(&second).is_some(),
            "new id + reordered keys must still count as the same call"
        );
    }

    #[test]
    fn disabled_killer_never_blocks() {
        let killer = LoopKiller::new(None);
        let c = call("read", serde_json::json!({"path": "x"}));
        for _ in 0..100 {
            assert!(killer.note_call(&c).is_none());
        }
    }
}
