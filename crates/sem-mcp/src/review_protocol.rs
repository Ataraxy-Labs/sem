//! Centralized "loop discipline" text for the review-listener tools
//! (`join_review` / `wait_for_branch` / `reply_to_branch` /
//! `list_open_branches`, defined in `server.rs`).
//!
//! Every short instruction an agent sees telling it what to do next after a
//! tool call — plus the two typed terminal-result shapes (`unanswerable`,
//! `review_gone`) — used to live scattered as inline literals across
//! `server.rs`'s match arms and (for the two result builders)
//! `agent_review.rs`. Centralized here as one source so the wording behind
//! "keep calling wait_for_branch" can't drift between call sites that are
//! supposed to say the same thing.
//!
//! Tool *descriptions* (the longer schema text shown to the model before it
//! ever calls a tool) still carry their own narrative prose next to each
//! `#[tool(...)]` in `server.rs` — they're necessarily longer and serve a
//! different moment (before the loop starts) than these short per-result
//! instructions (after each call returns) — but [`REVIEW_LISTENER_PROTOCOL`],
//! the full protocol `join_review` hands back, lives here since it's the one
//! piece of narrative text a tool *result* (not just a description) carries.

use serde_json::Value;

use crate::agent_review::AgentReviewError;

/// Protocol text handed back verbatim by `join_review` — the loop discipline
/// that keeps a headless listener session alive.
pub const REVIEW_LISTENER_PROTOCOL: &str = "You are now a review listener. Loop: call wait_for_branch. \
    When it returns a branch, investigate the question IN THIS REPOSITORY (read code, grep, run \
    tests if needed — the diff context attached is a starting point, not the whole truth), then \
    reply_to_branch with partial:true chunks as you compose (2-4 sentences each, content is \
    CUMULATIVE) and a final call with partial:false. Then IMMEDIATELY call wait_for_branch again. \
    If wait_for_branch returns status timeout, call it again immediately — this is normal, you are \
    listening. If reply_to_branch returns status unanswerable, that question's target vanished \
    (deleted, already answered, or its claim expired) — do not retry it, just call wait_for_branch \
    again. If wait_for_branch returns status review_gone, the diff itself was deleted or expired: \
    stop listening, this is the one legitimate reason to end the session. If you're ever unsure \
    what's still outstanding (e.g. after a confusing reply or a resumed session), call \
    list_open_branches — a read-only check that never claims anything — to see what's still open \
    before deciding what to do next. Never end the session yourself for any other reason.";

/// Instruction carried in `wait_for_branch`'s `status: timeout` result — a
/// timeout is the normal idle state of a listener, never a stopping
/// condition.
pub const TIMEOUT_INSTRUCTION: &str = "Call wait_for_branch again now. You are still listening.";

/// Follow-up text appended after `wait_for_branch` returns a branch,
/// telling the agent what to do with the question it just received.
pub const BRANCH_FOUND_FOLLOWUP: &str = "\n\nA reviewer asked a question. Investigate it in this repository, then answer \
     with reply_to_branch (partial:true chunks while composing, a final partial:false \
     call to commit). As soon as you've replied, call wait_for_branch again immediately.";

/// `reply_to_branch` success message when `partial: true` (still composing).
pub const PARTIAL_REPLY_POSTED: &str = "Partial reply posted.";

/// `reply_to_branch` success message on the committing (non-partial) call.
pub const REPLY_POSTED: &str = "Answer posted. Call wait_for_branch again now.";

/// Typed "unanswerable" tool result for `reply_to_branch` on a terminal
/// error (see [`AgentReviewError::is_terminal`]): the loop must never wedge
/// retrying a reply whose target vanished, so this is a successful tool
/// result, not an error — the same shape `wait_for_branch`'s timeout uses,
/// with an explicit reason and the same "keep listening" instruction.
pub fn unanswerable_result(err: &AgentReviewError) -> Value {
    serde_json::json!({
        "status": "unanswerable",
        "reason": err.to_string(),
        "instruction": "Call wait_for_branch again now.",
    })
}

/// Typed "review_gone" result for `wait_for_branch` when the DIFF itself
/// 404s (deleted or expired) — the one legitimate reason for the loop to
/// stop itself, distinct from every other error (which says "try again").
pub fn review_gone_result() -> Value {
    serde_json::json!({
        "status": "review_gone",
        "instruction": "Stop listening; the review no longer exists.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unanswerable_result_carries_the_error_reason_and_keep_listening_instruction() {
        let err = AgentReviewError::Http {
            status: 404,
            body: "comment not found".to_string(),
        };
        let result = unanswerable_result(&err);
        assert_eq!(result["status"], Value::String("unanswerable".to_string()));
        assert_eq!(result["instruction"], Value::String("Call wait_for_branch again now.".to_string()));
        assert!(result["reason"].as_str().unwrap().contains("404"));
    }

    #[test]
    fn review_gone_result_instructs_the_loop_to_stop() {
        let result = review_gone_result();
        assert_eq!(result["status"], Value::String("review_gone".to_string()));
        assert_eq!(
            result["instruction"],
            Value::String("Stop listening; the review no longer exists.".to_string())
        );
    }
}
