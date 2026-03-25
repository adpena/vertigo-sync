use serde_json::json;
use vertigo_sync::{
    ReadinessExpectation, ReadinessRecord, ReadinessRejection, ReadinessState,
    ReadinessStatusClass, ReadinessTarget,
};

fn ready_record(state: &ReadinessState, target: ReadinessTarget) -> ReadinessRecord {
    let mut record = state.current_readiness(target);
    record.ready = true;
    record.status_class = ReadinessStatusClass::Ready;
    record.code = "ready".to_string();
    record.reason = None;
    record
}

#[test]
fn readiness_contract_test_default_preview_readiness_uses_the_public_contract_shape() {
    let state = ReadinessState::new();
    let record = state.current_readiness(ReadinessTarget::Preview);

    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        json!({
            "target": "preview",
            "ready": false,
            "epoch": 0,
            "incarnation_id": "inc-1",
            "status_class": "transient",
            "code": "plugin_unavailable",
            "reason": "plugin_unavailable"
        })
    );
}

#[test]
fn readiness_contract_test_rotate_incarnation_invalidates_cached_readiness_without_changing_epoch()
{
    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::Preview))
        .unwrap();

    let cached = state.current_readiness(ReadinessTarget::Preview);
    let cached_expectation = ReadinessExpectation {
        target: ReadinessTarget::Preview,
        epoch: cached.epoch,
        incarnation_id: cached.incarnation_id.clone(),
    };

    let next_incarnation = state.rotate_incarnation("studio_restart");
    assert_eq!(next_incarnation, "inc-2");

    let current = state.current_readiness(ReadinessTarget::Preview);
    assert_eq!(current.epoch, cached.epoch);
    assert_ne!(current.incarnation_id, cached_expectation.incarnation_id);
    assert!(matches!(
        state.validate_expectation(ReadinessTarget::Preview, &cached_expectation),
        Err(ReadinessRejection::IncarnationMismatch { .. })
    ));
}

#[test]
fn readiness_contract_test_dependent_targets_do_not_remain_ready_after_edit_sync_invalidates() {
    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::Preview))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::FullBakeStart))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::FullBakeResult))
        .unwrap();

    state.advance_epoch_if_invalidated(ReadinessTarget::EditSync, true);

    assert!(!state.current_readiness(ReadinessTarget::EditSync).ready);
    assert!(!state.current_readiness(ReadinessTarget::Preview).ready);
    assert!(
        !state
            .current_readiness(ReadinessTarget::FullBakeStart)
            .ready
    );
    assert!(
        !state
            .current_readiness(ReadinessTarget::FullBakeResult)
            .ready
    );
}

#[test]
fn readiness_contract_test_validate_expectation_rejects_target_mismatch() {
    let state = ReadinessState::new();
    let mismatch = ReadinessExpectation {
        target: ReadinessTarget::EditSync,
        epoch: 0,
        incarnation_id: "inc-1".to_string(),
    };

    assert!(matches!(
        state.validate_expectation(ReadinessTarget::Preview, &mismatch),
        Err(ReadinessRejection::TargetMismatch { .. })
    ));
}

#[test]
fn readiness_contract_test_validate_expectation_rejects_epoch_mismatch() {
    let state = ReadinessState::new();
    let mismatch = ReadinessExpectation {
        target: ReadinessTarget::Preview,
        epoch: 1,
        incarnation_id: "inc-1".to_string(),
    };

    assert!(matches!(
        state.validate_expectation(ReadinessTarget::Preview, &mismatch),
        Err(ReadinessRejection::EpochMismatch { .. })
    ));
}

#[test]
fn readiness_contract_test_validate_expectation_rejects_incarnation_mismatch() {
    let state = ReadinessState::new();
    let mismatch = ReadinessExpectation {
        target: ReadinessTarget::Preview,
        epoch: 0,
        incarnation_id: "inc-99".to_string(),
    };

    assert!(matches!(
        state.validate_expectation(ReadinessTarget::Preview, &mismatch),
        Err(ReadinessRejection::IncarnationMismatch { .. })
    ));
}

#[test]
fn readiness_contract_test_validate_expectation_rejects_not_ready() {
    let state = ReadinessState::new();
    let expectation = ReadinessExpectation {
        target: ReadinessTarget::Preview,
        epoch: 0,
        incarnation_id: "inc-1".to_string(),
    };

    assert!(matches!(
        state.validate_expectation(ReadinessTarget::Preview, &expectation),
        Err(ReadinessRejection::NotReady { .. })
    ));
}
