//! Who may steer a session, and when.
//!
//! # The principal is not the gate
//!
//! A bearer token proves exactly one thing: the caller could read `$GROK_HOME/gx-remote.token`,
//! which is 0600 and owned by the person running the leader. There is one such principal —
//! [`Principal::Human`] — and it is the same principal the agent stamps on the `session/prompt`
//! this lane sends (`acp_agent.rs`). So the principal is **provenance**, recorded on every
//! authorized verb so an audit line says who a prompt came from; it is never the thing that decides
//! admission. `Principal::Agent` is never granted here at all.
//!
//! # What is the gate
//!
//! The *session's* state. A phone can always queue, but it cannot interject into a session that has
//! no turn to interject into, and it cannot cancel one that is not running. That policy is
//! expressed in the shared vocabulary — [`Operation`] and [`OperationSet`], evaluated by
//! [`authorize_operation`] — rather than in an `if` ladder of its own, so the lane and the TUI
//! cannot drift on what "interject" means:
//!
//! | roster activity | `Queue` | `Interject` | `InterruptAndSend` |
//! |---|---|---|---|
//! | `working` | allow | allow | allow |
//! | `needs_input` | allow (queues behind the approval) | deny | allow |
//! | `idle`, `completed` | allow | deny | deny |
//! | `dormant`, `dead` | allow (the attach that precedes it makes the session resident) | deny | deny |
//!
//! The activity that table is read with is [`effective_activity`], not the roster's own: a session
//! this lane is holding an unanswered interaction for is `needs_input` whatever the roster says.
//!
//! Mapping `POST …/cancel` to [`Operation::InterruptAndSend`] is a vocabulary choice and is
//! recorded as one: the lane sends a bare `session/cancel` notification with nothing following it.
//! There is no "interrupt-only" operation in the vocabulary, and inventing one here would put a
//! fork-local variant into a shared type.

use xai_message_delivery_core::{Operation, OperationSet, Principal, authorize_operation};

use crate::approvals::ApprovalStore;
use crate::error::ApiError;

/// Wire spellings of [`xai_grok_shell::agent::roster::RosterActivity`], which is what a
/// [`crate::routes::sessions::SessionSummary`] carries. Kept as `&str` rather than re-deriving the
/// enum so the policy reads the same string a client does.
pub mod activity {
    pub const WORKING: &str = "working";
    pub const IDLE: &str = "idle";
    pub const NEEDS_INPUT: &str = "needs_input";
    pub const DORMANT: &str = "dormant";
    pub const COMPLETED: &str = "completed";
    pub const DEAD: &str = "dead";
}

/// The operations a session in `activity` admits.
///
/// Every row includes [`Operation::Queue`]: queueing is the one thing that is always safe, because
/// the agent's own queue is what absorbs it. An activity this build does not recognise — a newer
/// leader with a new state — collapses to queue-only, which is the conservative reading rather than
/// the permissive one.
///
/// [`Operation::Steer`] is never granted: the lane has no steering verb, and a set that claimed it
/// would authorize a route that does not exist.
pub fn allowed_operations(activity: &str) -> OperationSet {
    match activity {
        activity::WORKING => OperationSet::QUEUE
            .with(Operation::Interject)
            .with(Operation::InterruptAndSend),
        activity::NEEDS_INPUT => OperationSet::QUEUE.with(Operation::InterruptAndSend),
        _ => OperationSet::QUEUE,
    }
}

/// Admit `operation` against `activity`, or explain the refusal.
///
/// A refusal is `409 not_accepting`, never `403`: the caller's credential is fine and retrying with
/// a better one changes nothing. What has to change is the session — answer its pending approval,
/// or wait for a turn to start.
pub fn authorize(session_id: &str, activity: &str, operation: Operation) -> Result<(), ApiError> {
    let allowed = allowed_operations(activity);
    if authorize_operation(allowed, operation).is_ok() {
        // Provenance, not the gate: one local human authority, recorded so an audit trail can say
        // where a prompt came from. See the module docs.
        tracing::info!(
            principal = ?Principal::Human,
            session_id,
            activity,
            operation = operation_label(operation),
            "gx-remote-api: authorized"
        );
        return Ok(());
    }
    Err(ApiError::NotAccepting(denial_message(
        session_id, activity, operation, allowed,
    )))
}

/// The activity the policy actually evaluates.
///
/// The roster is a cached projection and lags a turn boundary by up to one broadcast, so a session
/// that has just raised a permission prompt can still read `working` here. The approval store does
/// not lag: it holds the reverse-request itself. So a session with a pending interaction is
/// [`activity::NEEDS_INPUT`] whatever the roster says, which is what turns "interject into a
/// blocked turn" from a silent no-op into a `409` that names the approval.
///
/// The override only ever moves a session *into* `needs_input`, never out of it: an empty store is
/// no evidence that nothing is pending — the lane may simply not have attached — so the roster
/// still speaks for every session this lane holds no interaction for.
pub fn effective_activity(
    approvals: &ApprovalStore,
    session_id: &str,
    roster_activity: &str,
) -> String {
    if approvals.has_pending(session_id) {
        return activity::NEEDS_INPUT.to_string();
    }
    roster_activity.to_string()
}

/// The verb name a client used, for the message.
fn operation_label(operation: Operation) -> &'static str {
    match operation {
        Operation::Queue => "queue",
        Operation::Steer => "steer",
        Operation::Interject => "interject",
        Operation::InterruptAndSend => "cancel",
    }
}

/// Every operation this API can be asked for, in the order a message should list them.
const OFFERED: [Operation; 3] = [
    Operation::Queue,
    Operation::Interject,
    Operation::InterruptAndSend,
];

/// Name the activity, the refused verb, and what would work instead — a client that only prints the
/// message should still be able to tell its user what to do next.
fn denial_message(
    session_id: &str,
    activity: &str,
    operation: Operation,
    allowed: OperationSet,
) -> String {
    let permitted: Vec<&str> = OFFERED
        .iter()
        .filter(|op| allowed.contains(**op))
        .map(|op| operation_label(*op))
        .collect();
    let mut message = format!(
        "session {session_id} is {activity}; {} is not accepted (allowed here: {})",
        operation_label(operation),
        permitted.join(", ")
    );
    if activity == activity::NEEDS_INPUT && operation == Operation::Interject {
        message.push_str(" — answer the pending approval first");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every activity the leader can report, paired with the exact row of the plan's table.
    const MATRIX: [(&str, bool, bool, bool); 6] = [
        // activity, queue, interject, cancel
        (activity::WORKING, true, true, true),
        (activity::NEEDS_INPUT, true, false, true),
        (activity::IDLE, true, false, false),
        (activity::COMPLETED, true, false, false),
        (activity::DORMANT, true, false, false),
        (activity::DEAD, true, false, false),
    ];

    #[test]
    fn the_matrix_is_the_plans_matrix() {
        for (activity, queue, interject, cancel) in MATRIX {
            let allowed = allowed_operations(activity);
            assert_eq!(
                allowed.contains(Operation::Queue),
                queue,
                "{activity} queue"
            );
            assert_eq!(
                allowed.contains(Operation::Interject),
                interject,
                "{activity} interject"
            );
            assert_eq!(
                allowed.contains(Operation::InterruptAndSend),
                cancel,
                "{activity} cancel"
            );
        }
    }

    #[test]
    fn steering_is_never_granted_because_there_is_no_steering_verb() {
        for (activity, ..) in MATRIX {
            assert!(
                !allowed_operations(activity).contains(Operation::Steer),
                "{activity}"
            );
        }
    }

    #[test]
    fn an_activity_this_build_does_not_know_collapses_to_queue_only() {
        let allowed = allowed_operations("compacting");
        assert!(allowed.contains(Operation::Queue));
        assert!(!allowed.contains(Operation::Interject));
        assert!(!allowed.contains(Operation::InterruptAndSend));
    }

    #[test]
    fn a_denial_names_the_activity_the_verb_and_the_alternative() {
        let err = authorize("sess-1", activity::IDLE, Operation::Interject).unwrap_err();
        assert_eq!(err.code(), "not_accepting");
        assert_eq!(err.status().as_u16(), 409);
        let message = err.to_string();
        assert!(message.contains("sess-1"), "{message}");
        assert!(message.contains("idle"), "{message}");
        assert!(message.contains("interject"), "{message}");
        assert!(message.contains("queue"), "{message}");
    }

    #[test]
    fn a_needs_input_interject_points_at_the_approval() {
        let message = authorize("sess-1", activity::NEEDS_INPUT, Operation::Interject)
            .unwrap_err()
            .to_string();
        assert!(message.contains("pending approval"), "{message}");
        // …and it still names cancel, which *is* allowed while an approval is pending.
        assert!(message.contains("cancel"), "{message}");
    }

    #[test]
    fn authorize_agrees_with_the_matrix_for_every_cell() {
        for (activity, queue, interject, cancel) in MATRIX {
            for (operation, expected) in [
                (Operation::Queue, queue),
                (Operation::Interject, interject),
                (Operation::InterruptAndSend, cancel),
            ] {
                assert_eq!(
                    authorize("s", activity, operation).is_ok(),
                    expected,
                    "{activity} / {}",
                    operation_label(operation)
                );
            }
        }
    }

    /// A store holding one unanswered `session/request_permission` for `session_id`.
    fn store_with_a_pending_approval(session_id: &str) -> ApprovalStore {
        let attachments = std::sync::Arc::new(crate::state::Attachments::default());
        attachments.mark_attached(session_id);
        let store = ApprovalStore::new(attachments);
        store
            .intercept(
                crate::approvals::REQUEST_PERMISSION,
                &serde_json::json!(1),
                &serde_json::json!({
                    "sessionId": session_id,
                    "toolCall": { "toolCallId": "tc-1" },
                }),
            )
            .expect("the store must take custody of an attached session's interaction");
        store
    }

    #[test]
    fn a_pending_approval_outranks_whatever_the_roster_says() {
        let store = store_with_a_pending_approval("sess-1");
        for stale in [
            activity::WORKING,
            activity::IDLE,
            activity::COMPLETED,
            activity::DORMANT,
        ] {
            assert_eq!(
                effective_activity(&store, "sess-1", stale),
                activity::NEEDS_INPUT,
                "the roster said {stale}, but the lane is holding the request itself"
            );
        }
    }

    #[test]
    fn without_a_pending_approval_the_roster_still_speaks() {
        let store = store_with_a_pending_approval("sess-1");
        // An empty store is not evidence that nothing is pending — this lane may never have
        // attached — so it must not pull a session *out* of needs_input.
        assert_eq!(
            effective_activity(&store, "sess-other", activity::NEEDS_INPUT),
            activity::NEEDS_INPUT
        );
        assert_eq!(
            effective_activity(&store, "sess-other", activity::WORKING),
            activity::WORKING
        );
    }

    #[test]
    fn a_held_approval_is_what_denies_an_interject() {
        // The end-to-end point of the override: a stale `working` row would have allowed this.
        let store = store_with_a_pending_approval("sess-1");
        let activity = effective_activity(&store, "sess-1", activity::WORKING);
        let err = authorize("sess-1", &activity, Operation::Interject).unwrap_err();
        assert_eq!(err.code(), "not_accepting");
        assert!(err.to_string().contains("pending approval"), "{err}");
        // Cancel is still allowed while an approval is pending.
        assert!(authorize("sess-1", &activity, Operation::InterruptAndSend).is_ok());
        assert!(authorize("sess-1", &activity, Operation::Queue).is_ok());
    }
}
