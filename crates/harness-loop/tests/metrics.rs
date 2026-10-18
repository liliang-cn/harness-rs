//! The loop's metrics (`harness_loop::meters`), read back from an in-memory
//! recorder: one sample per model call, tool call and run, with the labels a
//! dashboard groups by.

use harness_context::default_world;
use harness_core::Task;
use harness_loop::{AgentLoop, meters};
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::ReadFile;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

/// `(name, sorted labels)` → samples (histogram values, or one counter total).
type Seen = HashMap<(String, Vec<(String, String)>), Vec<f64>>;

fn run_and_collect(model: MockModel, files: &[(&str, &str)]) -> Seen {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        // Current-thread, so every sample is recorded on the thread that holds
        // the local recorder.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ws = std::env::temp_dir().join(format!(
                "metrics-test-{}-{}",
                std::process::id(),
                files.len()
            ));
            std::fs::create_dir_all(&ws).unwrap();
            for (name, body) in files {
                std::fs::write(ws.join(name), body).unwrap();
            }
            let mut world = default_world(&ws);
            AgentLoop::new(model)
                .with_tool(Arc::new(ReadFile))
                .run(
                    Task {
                        description: "read".into(),
                        source: None,
                        deadline: None,
                    },
                    &mut world,
                )
                .await
                .unwrap();
            let _ = std::fs::remove_dir_all(&ws);
        });
    });

    let mut seen = Seen::new();
    for (key, _unit, _desc, value) in snapshotter.snapshot().into_vec() {
        let mut labels: Vec<(String, String)> = key
            .key()
            .labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect();
        labels.sort();
        let samples = match value {
            DebugValue::Histogram(v) => v.into_iter().map(|f| f.into_inner()).collect(),
            DebugValue::Counter(n) => vec![n as f64],
            DebugValue::Gauge(g) => vec![g.into_inner()],
        };
        seen.insert((key.key().name().to_string(), labels), samples);
    }
    seen
}

fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<_> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    v.sort();
    v
}

#[test]
fn a_run_records_model_tool_and_run_metrics() {
    let model = MockModel::new()
        .script(
            MockResponse::tool_calls(vec![
                ("read_file".into(), json!({"path": "a.txt"})),
                ("read_file".into(), json!({"path": "missing.txt"})),
            ])
            .with_usage(100, 20),
        )
        .script(MockResponse::text("done").with_usage(50, 10));
    let seen = run_and_collect(model, &[("a.txt", "a")]);
    let get = |name: &str, l: &[(&str, &str)]| {
        seen.get(&(name.to_string(), labels(l)))
            .unwrap_or_else(|| panic!("no {name} {l:?} in {:#?}", seen.keys()))
            .clone()
    };

    let model_labels = [
        ("gen_ai.operation.name", "chat"),
        ("gen_ai.provider.name", "mock"),
        ("gen_ai.request.model", "mock-1"),
    ];
    assert_eq!(get(meters::OPERATION_DURATION, &model_labels).len(), 2);

    let tokens = |kind| {
        get(
            meters::TOKEN_USAGE,
            &[
                ("gen_ai.token.type", kind),
                ("gen_ai.provider.name", "mock"),
                ("gen_ai.request.model", "mock-1"),
            ],
        )
    };
    assert_eq!(tokens("input"), vec![100.0, 50.0]);
    assert_eq!(tokens("output"), vec![20.0, 10.0]);

    // Successes and failures of one tool are separate series, so an error
    // rate is a ratio of two counts.
    let tool = |ok| {
        get(
            meters::TOOL_DURATION,
            &[("gen_ai.tool.name", "read_file"), ("ok", ok)],
        )
    };
    assert_eq!(tool("true").len(), 1);
    assert_eq!(tool("false").len(), 1);

    assert_eq!(get(meters::RUN_DURATION, &[("outcome", "done")]).len(), 1);
}

#[test]
fn nothing_identifies_a_request_or_a_user() {
    let seen = run_and_collect(MockModel::new().script(MockResponse::text("done")), &[]);
    let allowed = [
        "gen_ai.operation.name",
        "gen_ai.provider.name",
        "gen_ai.request.model",
        "gen_ai.token.type",
        "gen_ai.tool.name",
        "error.type",
        "ok",
        "outcome",
        "stage",
    ];
    for (name, labels) in seen.keys() {
        for (k, _) in labels {
            assert!(
                allowed.contains(&k.as_str()),
                "{name} has unbounded label {k}"
            );
        }
    }
}
