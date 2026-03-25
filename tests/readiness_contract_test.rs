use std::hint::black_box;
use std::time::Instant;

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

fn ready_record_with_epoch(
    state: &ReadinessState,
    target: ReadinessTarget,
    epoch: u64,
) -> ReadinessRecord {
    let mut record = ready_record(state, target);
    record.epoch = epoch;
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
fn readiness_contract_test_ready_records_keep_an_explicit_null_reason_field() {
    let state = ReadinessState::new();
    let mut record = ready_record(&state, ReadinessTarget::EditSync);
    record.reason = None;

    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        json!({
            "target": "edit_sync",
            "ready": true,
            "epoch": 0,
            "incarnation_id": "inc-1",
            "status_class": "ready",
            "code": "ready",
            "reason": null
        })
    );
}

#[test]
fn readiness_contract_test_preview_ready_rejects_without_edit_sync_ready() {
    let mut state = ReadinessState::new();
    let result = state.update_readiness(ready_record(&state, ReadinessTarget::Preview));

    assert!(matches!(
        result,
        Err(ReadinessRejection::DependencyViolation {
            target: ReadinessTarget::Preview,
            prerequisite: ReadinessTarget::EditSync,
            ..
        })
    ));
    assert!(!state.current_readiness(ReadinessTarget::Preview).ready);
    assert!(!state.current_readiness(ReadinessTarget::EditSync).ready);
}

#[test]
fn readiness_contract_test_full_bake_start_ready_rejects_without_edit_sync_ready() {
    let mut state = ReadinessState::new();
    let result = state.update_readiness(ready_record(&state, ReadinessTarget::FullBakeStart));

    assert!(matches!(
        result,
        Err(ReadinessRejection::DependencyViolation {
            target: ReadinessTarget::FullBakeStart,
            prerequisite: ReadinessTarget::EditSync,
            ..
        })
    ));
    assert!(
        !state
            .current_readiness(ReadinessTarget::FullBakeStart)
            .ready
    );
    assert!(!state.current_readiness(ReadinessTarget::EditSync).ready);
}

#[test]
fn readiness_contract_test_full_bake_result_requires_success_marker_not_only_current_start_state() {
    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::FullBakeStart))
        .unwrap();

    let result = state.update_readiness(ready_record(&state, ReadinessTarget::FullBakeResult));

    assert!(matches!(
        result,
        Err(ReadinessRejection::DependencyViolation {
            target: ReadinessTarget::FullBakeResult,
            prerequisite: ReadinessTarget::FullBakeStart,
            ..
        })
    ));
    assert!(
        !state
            .current_readiness(ReadinessTarget::FullBakeResult)
            .ready
    );
}

#[test]
fn readiness_contract_test_full_bake_result_succeeds_only_after_explicit_success_marker() {
    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::FullBakeStart))
        .unwrap();

    state
        .record_successful_full_bake_start_for_current_incarnation()
        .unwrap();

    assert!(
        state
            .update_readiness(ready_record(&state, ReadinessTarget::FullBakeResult))
            .is_ok()
    );
    assert!(
        state
            .current_readiness(ReadinessTarget::FullBakeResult)
            .ready
    );
}

#[test]
fn readiness_contract_test_full_bake_success_marker_rejects_fresh_state_bypass() {
    let mut state = ReadinessState::new();

    assert!(matches!(
        state.record_successful_full_bake_start_for_current_incarnation(),
        Err(ReadinessRejection::DependencyViolation {
            target: ReadinessTarget::FullBakeStart,
            prerequisite: ReadinessTarget::EditSync,
            ..
        })
    ));
    assert!(matches!(
        state.update_readiness(ready_record(&state, ReadinessTarget::FullBakeResult)),
        Err(ReadinessRejection::DependencyViolation {
            target: ReadinessTarget::FullBakeResult,
            prerequisite: ReadinessTarget::FullBakeStart,
            ..
        })
    ));
}

#[test]
fn readiness_contract_test_update_readiness_rejects_epoch_rewrites() {
    let mut state = ReadinessState::new();
    let result =
        state.update_readiness(ready_record_with_epoch(&state, ReadinessTarget::Preview, 7));

    assert!(matches!(
        result,
        Err(ReadinessRejection::EpochMismatch {
            target: ReadinessTarget::Preview,
            expected: 7,
            actual: 0,
        })
    ));
    assert_eq!(state.current_readiness(ReadinessTarget::Preview).epoch, 0);
}

#[test]
fn readiness_contract_test_rotate_incarnation_invalidates_cached_readiness_without_changing_epoch()
{
    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
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
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::Preview))
        .unwrap();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::FullBakeStart))
        .unwrap();
    state
        .record_successful_full_bake_start_for_current_incarnation()
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

#[test]
fn readiness_contract_test_profiling_checkpoint_records_hot_path_timings() {
    let iterations = 25_000u64;

    let state = ReadinessState::new();
    let start = Instant::now();
    let mut sink = 0u64;
    for _ in 0..iterations {
        let record = state.current_readiness(ReadinessTarget::Preview);
        sink ^= record.epoch;
        black_box(&record);
    }
    let lookup_ns_per_op = start.elapsed().as_nanos() as f64 / iterations as f64;

    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
    let start = Instant::now();
    for _ in 0..iterations {
        let record = state.advance_epoch_if_invalidated(ReadinessTarget::EditSync, true);
        sink ^= record.epoch;
        black_box(&record);
    }
    let epoch_update_ns_per_op = start.elapsed().as_nanos() as f64 / iterations as f64;

    let mut state = ReadinessState::new();
    state
        .update_readiness(ready_record(&state, ReadinessTarget::EditSync))
        .unwrap();
    let start = Instant::now();
    for i in 0..iterations {
        let inc = state.rotate_incarnation(format!("profile-{i}"));
        sink ^= inc.len() as u64;
        black_box(&inc);
    }
    let rollover_ns_per_op = start.elapsed().as_nanos() as f64 / iterations as f64;

    eprintln!(
        "readiness profiling checkpoint: lookup_ns_per_op={lookup_ns_per_op:.2} epoch_update_ns_per_op={epoch_update_ns_per_op:.2} rollover_ns_per_op={rollover_ns_per_op:.2}"
    );
    black_box(sink);
}
