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

mod readiness_contract_test {
    pub mod query_and_events {
        use super::super::*;
        use std::sync::Arc;
        use std::time::Duration;

        use axum::http::StatusCode;
        use reqwest::Client;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::net::TcpListener;
        use tokio::time::timeout;
        use vertigo_sync::server::build_router;
        use vertigo_sync::{ServerState, ServerStateOptions, Snapshot};

        fn empty_snapshot() -> Snapshot {
            Snapshot {
                version: 1,
                include: Vec::new(),
                fingerprint: "test-fingerprint".to_string(),
                entries: Vec::new(),
            }
        }

        fn test_server_state() -> (tempfile::TempDir, Arc<ServerState>) {
            let root = tempdir().expect("tempdir");
            let state = ServerState::with_full_config(
                root.path().to_path_buf(),
                Vec::new(),
                empty_snapshot(),
                ServerStateOptions {
                    channel_capacity: 64,
                    turbo: false,
                    coalesce_ms: 50,
                    binary_models: false,
                    glob_ignores: vertigo_sync::GlobIgnoreSet::empty(),
                    project_path: Some(root.path().join("default.project.json")),
                },
            );

            (root, state)
        }

        async fn spawn_server(state: Arc<ServerState>) -> (String, tokio::task::JoinHandle<()>) {
            let app = build_router(state);
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind listener");
            let addr = listener.local_addr().expect("listener addr");
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{}:{}", addr.ip(), addr.port()), server)
        }

        fn ready_record_for(state: &ServerState, target: ReadinessTarget) -> ReadinessRecord {
            let mut record = state.current_readiness(target);
            record.ready = true;
            record.status_class = ReadinessStatusClass::Ready;
            record.code = "ready".to_string();
            record.reason = None;
            record
        }

        async fn get_json(client: &Client, url: &str) -> Value {
            client
                .get(url)
                .send()
                .await
                .expect("request")
                .error_for_status()
                .expect("successful response")
                .json::<Value>()
                .await
                .expect("json body")
        }

        async fn get_status(client: &Client, url: &str) -> StatusCode {
            client.get(url).send().await.expect("request").status()
        }

        fn take_next_sse_payload(buffer: &mut String) -> Option<String> {
            let normalized = buffer.replace("\r\n", "\n");
            let end = normalized.find("\n\n")?;
            let block = normalized[..end].to_string();
            let remainder = normalized[end + 2..].to_string();
            *buffer = remainder;

            let payload = block
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");

            if payload.is_empty() {
                None
            } else {
                Some(payload)
            }
        }

        async fn read_next_sse_record(
            response: &mut reqwest::Response,
            buffer: &mut String,
        ) -> Value {
            loop {
                if let Some(payload) = take_next_sse_payload(buffer) {
                    return serde_json::from_str(&payload).expect("readiness event json");
                }

                let chunk = timeout(Duration::from_secs(5), response.chunk())
                    .await
                    .expect("timed out waiting for SSE chunk")
                    .expect("response chunk");
                let chunk = chunk.expect("SSE stream ended before readiness event");
                buffer.push_str(std::str::from_utf8(&chunk).expect("utf8 readiness event"));
            }
        }

        async fn open_readiness_stream(
            client: &Client,
            base_url: &str,
            target: ReadinessTarget,
        ) -> reqwest::Response {
            let target = serde_json::to_value(target)
                .expect("target json")
                .as_str()
                .expect("target string")
                .to_string();
            client
                .get(format!("{base_url}/readiness/events?target={target}"))
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .send()
                .await
                .expect("readiness events response")
                .error_for_status()
                .expect("successful readiness events response")
        }

        #[tokio::test]
        async fn readiness_contract_test_query_returns_authoritative_payload_and_rejects_invalid_targets()
         {
            let (_root, state) = test_server_state();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                .unwrap();

            let (base_url, server) = spawn_server(state.clone()).await;
            let client = Client::new();

            let payload = get_json(&client, &format!("{base_url}/readiness?target=preview")).await;

            assert_eq!(
                payload,
                serde_json::to_value(state.current_readiness(ReadinessTarget::Preview)).unwrap()
            );

            assert_eq!(
                get_status(&client, &format!("{base_url}/readiness?target=bogus")).await,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                get_status(
                    &client,
                    &format!("{base_url}/readiness/events?target=bogus")
                )
                .await,
                StatusCode::BAD_REQUEST
            );

            server.abort();
        }

        #[tokio::test]
        async fn readiness_contract_test_query_covers_all_required_targets() {
            let (_root, state) = test_server_state();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::FullBakeStart))
                .unwrap();
            state
                .record_successful_full_bake_start_for_current_incarnation()
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::FullBakeResult))
                .unwrap();

            let (base_url, server) = spawn_server(state.clone()).await;
            let client = Client::new();

            for target in ReadinessTarget::ALL {
                let target = serde_json::to_value(target)
                    .expect("target json")
                    .as_str()
                    .expect("target string")
                    .to_string();
                let payload =
                    get_json(&client, &format!("{base_url}/readiness?target={target}")).await;
                let expected = match target.as_str() {
                    "edit_sync" => state.current_readiness(ReadinessTarget::EditSync),
                    "preview" => state.current_readiness(ReadinessTarget::Preview),
                    "full_bake_start" => state.current_readiness(ReadinessTarget::FullBakeStart),
                    "full_bake_result" => state.current_readiness(ReadinessTarget::FullBakeResult),
                    other => panic!("unexpected target {other}"),
                };
                assert_eq!(payload, serde_json::to_value(expected).unwrap());
            }

            server.abort();
        }

        #[tokio::test]
        async fn readiness_contract_test_event_stream_matches_query_shape_and_rejects_stale_epoch()
        {
            let (_root, state) = test_server_state();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                .unwrap();

            let (base_url, server) = spawn_server(state.clone()).await;
            let client = Client::new();

            let query_payload =
                get_json(&client, &format!("{base_url}/readiness?target=preview")).await;
            let mut response =
                open_readiness_stream(&client, &base_url, ReadinessTarget::Preview).await;
            let mut buffer = String::new();
            let first_event = read_next_sse_record(&mut response, &mut buffer).await;

            assert_eq!(first_event, query_payload);

            let stale_expectation = ReadinessExpectation {
                target: ReadinessTarget::Preview,
                epoch: first_event["epoch"].as_u64().expect("epoch"),
                incarnation_id: first_event["incarnation_id"]
                    .as_str()
                    .expect("incarnation")
                    .to_string(),
            };

            state.advance_readiness_epoch_if_invalidated(ReadinessTarget::Preview, true);

            let second_event = read_next_sse_record(&mut response, &mut buffer).await;
            assert_eq!(
                second_event,
                get_json(&client, &format!("{base_url}/readiness?target=preview")).await
            );
            assert!(matches!(
                state.validate_readiness_expectation(ReadinessTarget::Preview, &stale_expectation),
                Err(ReadinessRejection::EpochMismatch { .. })
            ));

            server.abort();
        }

        #[tokio::test]
        async fn readiness_contract_test_event_stream_rejects_stale_incarnation() {
            let (_root, state) = test_server_state();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                .unwrap();

            let (base_url, server) = spawn_server(state.clone()).await;
            let client = Client::new();

            let mut response =
                open_readiness_stream(&client, &base_url, ReadinessTarget::Preview).await;
            let mut buffer = String::new();
            let first_event = read_next_sse_record(&mut response, &mut buffer).await;
            let stale_expectation = ReadinessExpectation {
                target: ReadinessTarget::Preview,
                epoch: first_event["epoch"].as_u64().expect("epoch"),
                incarnation_id: first_event["incarnation_id"]
                    .as_str()
                    .expect("incarnation")
                    .to_string(),
            };

            state.rotate_readiness_incarnation("studio_restart");

            let second_event = read_next_sse_record(&mut response, &mut buffer).await;
            assert_eq!(
                second_event,
                get_json(&client, &format!("{base_url}/readiness?target=preview")).await
            );
            assert!(matches!(
                state.validate_readiness_expectation(ReadinessTarget::Preview, &stale_expectation),
                Err(ReadinessRejection::IncarnationMismatch { .. })
            ));

            server.abort();
        }

        #[tokio::test]
        async fn readiness_contract_test_event_stream_resyncs_after_lag() {
            let (_root, state) = test_server_state();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                .unwrap();
            state
                .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                .unwrap();

            let (base_url, server) = spawn_server(state.clone()).await;
            let client = Client::new();

            let mut response =
                open_readiness_stream(&client, &base_url, ReadinessTarget::Preview).await;
            let mut buffer = String::new();
            let _initial = read_next_sse_record(&mut response, &mut buffer).await;

            for i in 0..128 {
                if i % 2 == 0 {
                    let mut record = state.current_readiness(ReadinessTarget::Preview);
                    record.ready = false;
                    record.status_class = ReadinessStatusClass::Blocked;
                    record.code = "preview_not_ready".to_string();
                    record.reason = Some("preview_not_ready".to_string());
                    state.update_readiness(record).unwrap();
                } else {
                    state
                        .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                        .unwrap();
                }
            }

            let expected = get_json(&client, &format!("{base_url}/readiness?target=preview")).await;
            let resynced = read_next_sse_record(&mut response, &mut buffer).await;

            assert_eq!(
                resynced, expected,
                "lagged readiness consumers must resync to the authoritative snapshot instead of replaying stale events"
            );

            server.abort();
        }

        #[test]
        fn readiness_contract_test_profiling_checkpoint_records_query_and_sse_costs() {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async {
                let (_root, state) = test_server_state();
                state
                    .update_readiness(ready_record_for(&state, ReadinessTarget::EditSync))
                    .unwrap();
                state
                    .update_readiness(ready_record_for(&state, ReadinessTarget::Preview))
                    .unwrap();

                let (base_url, server) = spawn_server(state.clone()).await;
                let client = Client::new();
                let target_url = format!("{base_url}/readiness?target=preview");
                let iterations = 250u64;

                let query_start = Instant::now();
                let mut query_sink = 0u64;
                for _ in 0..iterations {
                    let payload = get_json(&client, &target_url).await;
                    query_sink ^= payload["epoch"].as_u64().unwrap_or_default();
                }
                let query_ns_per_op = query_start.elapsed().as_nanos() as f64 / iterations as f64;

                let serialization_record = state.current_readiness(ReadinessTarget::Preview);
                let serialization_start = Instant::now();
                let mut serialization_sink = 0usize;
                for _ in 0..25_000u64 {
                    let json = serde_json::to_string(&serialization_record).expect("serialize");
                    serialization_sink ^= json.len();
                    black_box(&json);
                }
                let serialization_ns_per_op =
                    serialization_start.elapsed().as_nanos() as f64 / 25_000f64;

                let mut streams = Vec::new();
                for _ in 0..4 {
                    streams.push(open_readiness_stream(&client, &base_url, ReadinessTarget::Preview).await);
                }
                let fanout_start = Instant::now();
                state
                    .advance_readiness_epoch_if_invalidated(ReadinessTarget::Preview, true)
                    ;
                for response in &mut streams {
                    let mut buffer = String::new();
                    let payload = read_next_sse_record(response, &mut buffer).await;
                    query_sink ^= payload["epoch"].as_u64().unwrap_or_default();
                }
                let fanout_ns_total = fanout_start.elapsed().as_nanos() as f64;

                eprintln!(
                    "readiness profiling checkpoint: query_ns_per_op={query_ns_per_op:.2} sse_fanout_ns_total={fanout_ns_total:.2} serialization_ns_per_op={serialization_ns_per_op:.2} hot_path_outside_rust=none"
                );

                black_box(query_sink);
                black_box(serialization_sink);
                server.abort();
            });
        }
    }
}
