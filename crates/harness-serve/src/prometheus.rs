//! Prometheus metrics for a served agent (feature `prometheus`).
//!
//! The loop records its metrics through the `metrics` facade (see
//! [`harness_loop::meters`] for the list); this installs a Prometheus recorder
//! for them and serves the text exposition at `GET /metrics`.
//!
//! ```ignore
//! let metrics = harness_serve::prometheus::install()?;
//! let app = harness_serve::http::router(Arc::new(service))
//!     .merge(harness_serve::prometheus::router(metrics));
//! ```
//!
//! Durations are exported as histograms with fixed buckets — not as summaries,
//! the exporter's default — because a summary's quantiles cannot be added up
//! across replicas, and p95 over a fleet is the question a dashboard asks.

use axum::{Router, routing::get};
use harness_loop::meters;
pub use metrics_exporter_prometheus::PrometheusHandle;
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder};

/// Seconds: a tool round-trip in milliseconds up to a model call measured in
/// minutes.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];
/// Tokens per call, powers of four from a one-line reply to a full window.
const TOKEN_BUCKETS: &[f64] = &[
    16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0,
];

/// The builder [`install`] uses, buckets set. Exposed so a host composing its
/// own recorder (or a test) gets the same histograms.
pub fn builder() -> Result<PrometheusBuilder, BuildError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Full(meters::TOKEN_USAGE.into()), TOKEN_BUCKETS)?
        .set_buckets_for_metric(Matcher::Suffix("duration".into()), DURATION_BUCKETS)
}

/// Install the Prometheus recorder as the process's global `metrics` recorder,
/// describe the agent's metrics, and return the handle `/metrics` renders from.
///
/// Also starts a background thread that runs the recorder's upkeep every five
/// seconds. Without a scrape nothing else would, and histogram samples would
/// accumulate in memory for as long as nobody looked.
pub fn install() -> Result<PrometheusHandle, BuildError> {
    let handle = builder()?.install_recorder()?;
    meters::describe();
    let upkeep = handle.clone();
    std::thread::Builder::new()
        .name("prometheus-upkeep".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                upkeep.run_upkeep();
            }
        })
        .expect("spawn prometheus upkeep thread");
    Ok(handle)
}

/// `GET /metrics` in the Prometheus text format. Merge it into the chat router,
/// or serve it on a separate, internal-only listener.
pub fn router(handle: PrometheusHandle) -> Router {
    Router::new().route("/metrics", get(move || async move { handle.render() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatService, InMemorySessions, OpenAuth};
    use harness_models::{MockModel, MockResponse};
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn a_served_turn_shows_up_at_metrics() {
        let recorder = builder().unwrap().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let service = ChatService::new(
                    Arc::new(MockModel::new().script(MockResponse::text("hi").with_usage(30, 5))),
                    Arc::new(OpenAuth::new("alice")),
                    Arc::new(InMemorySessions::new()),
                    std::env::temp_dir().join("serve-prometheus-test"),
                );
                service.chat(None, "s1", "hello").await.unwrap();
            });
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let body = rt.block_on(async {
            let resp = router(handle)
                .oneshot(
                    axum::http::Request::get("/metrics")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        });

        assert!(
            body.contains("gen_ai_client_operation_duration_bucket"),
            "durations are histograms, not summaries:\n{body}"
        );
        assert!(
            body.contains(r#"harness_run_duration_count{outcome="done"} 1"#),
            "{body}"
        );
        assert!(
            body.contains(r#"gen_ai_client_token_usage_sum{gen_ai_token_type="input",gen_ai_provider_name="mock",gen_ai_request_model="mock-1"} 30"#),
            "{body}"
        );
    }
}
