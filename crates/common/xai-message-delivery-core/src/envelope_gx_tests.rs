//! gx: tests for the gx-added `OperationSet::with` builder, in a gx-owned file so upstream's
//! `envelope_tests.rs` never conflicts on a rebase.
use super::*;

#[test]
fn with_adds_an_operation_without_dropping_the_existing_ones() {
    let set = OperationSet::QUEUE.with(Operation::Interject);
    assert!(set.contains(Operation::Interject));
    assert!(
        set.contains(Operation::Queue),
        "`with` must preserve the operations already in the set"
    );
    assert!(!set.contains(Operation::Steer));
    assert!(!set.contains(Operation::InterruptAndSend));
}

#[test]
fn with_is_idempotent() {
    let once = OperationSet::QUEUE.with(Operation::InterruptAndSend);
    let twice = once.with(Operation::InterruptAndSend);
    assert_eq!(
        once, twice,
        "adding the same operation twice must be a no-op"
    );
}

/// `with` and `contains` must agree for every operation, which is what sharing `bit` buys.
#[test]
fn with_and_contains_agree_for_every_operation() {
    for op in [
        Operation::Queue,
        Operation::Steer,
        Operation::Interject,
        Operation::InterruptAndSend,
    ] {
        assert!(
            OperationSet::QUEUE.with(op).contains(op),
            "`with` and `contains` disagree on {op:?}"
        );
    }
}

/// The builder composes into an authorization set the same way the const constants do.
#[test]
fn with_composes_a_set_authorize_operation_accepts() {
    let allowed = OperationSet::QUEUE
        .with(Operation::Interject)
        .with(Operation::InterruptAndSend);
    assert!(authorize_operation(allowed, Operation::Queue).is_ok());
    assert!(authorize_operation(allowed, Operation::Interject).is_ok());
    assert!(authorize_operation(allowed, Operation::InterruptAndSend).is_ok());
    assert!(authorize_operation(allowed, Operation::Steer).is_err());
    assert_eq!(
        OperationSet::QUEUE.with(Operation::Steer),
        OperationSet::QUEUE_AND_STEER,
        "the builder must produce the same bits as the const constant"
    );
}
