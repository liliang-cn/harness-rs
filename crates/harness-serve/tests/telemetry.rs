//! A served turn is observable without the host wiring anything: the loop's
//! spans hang off a `chat_request` span carrying the request id — the same id
//! the audit trail and the replay recording use — and the run's events and
//! totals are there because the service attaches a `TelemetryHook` per request.

use futures::StreamExt;
use harness_models::{MockModel, MockResponse};
use harness_serve::{ChatChunk, ChatService, InMemorySessions, OpenAuth};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

#[derive(Debug, Clone)]
struct SeenSpan {
    name: String,
    parent: Option<String>,
    fields: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct Capture {
    spans: Arc<Mutex<HashMap<u64, SeenSpan>>>,
    events: Arc<Mutex<Vec<String>>>,
}

struct Fields<'a>(&'a mut HashMap<String, String>);
impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let parent = ctx
            .span(id)
            .and_then(|s| s.parent())
            .map(|p| p.name().to_string());
        let mut fields = HashMap::new();
        attrs.record(&mut Fields(&mut fields));
        self.spans.lock().unwrap().insert(
            id.into_u64(),
            SeenSpan {
                name: attrs.metadata().name().to_string(),
                parent,
                fields,
            },
        );
    }
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        event.record(&mut Fields(&mut fields));
        if let Some(name) = fields.get("event") {
            self.events.lock().unwrap().push(name.clone());
        }
    }
}

fn service(model: MockModel) -> ChatService {
    ChatService::new(
        Arc::new(model),
        Arc::new(OpenAuth::new("alice")),
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("serve-telemetry-test"),
    )
}

fn assert_request_tree(capture: &Capture, request_id: &str) {
    let spans: Vec<SeenSpan> = capture.spans.lock().unwrap().values().cloned().collect();
    let request = spans
        .iter()
        .find(|s| s.name == "chat_request")
        .unwrap_or_else(|| panic!("no chat_request span:\n{spans:#?}"));
    assert_eq!(request.parent, None);
    assert_eq!(
        request.fields.get("request_id").map(String::as_str),
        Some(request_id),
        "the trace carries the id the reply, audit and replay use"
    );
    assert_eq!(
        request.fields.get("session.id").map(String::as_str),
        Some("s1")
    );
    let run = spans
        .iter()
        .find(|s| s.name == "agent_run")
        .expect("agent_run span");
    assert_eq!(run.parent.as_deref(), Some("chat_request"));
    let events = capture.events.lock().unwrap();
    assert!(
        events.iter().any(|e| e == "run.end"),
        "TelemetryHook is on by default: {events:?}"
    );
}

#[tokio::test]
async fn a_served_turn_is_traced_under_its_request() {
    let capture = Capture::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));

    let reply = service(MockModel::new().script(MockResponse::text("hi")))
        .chat(None, "s1", "hello")
        .await
        .unwrap();
    assert_request_tree(&capture, &reply.request_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_turn_is_traced_under_its_request() {
    let capture = Capture::default();
    // The streamed run executes on a spawned task, possibly another thread, so
    // the subscriber has to be global for this test binary's process — which is
    // why it lives in its own test with nothing else installing one.
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(capture.clone()),
    );

    let svc = service(MockModel::new().script(MockResponse::text("hi")));
    let mut stream = Box::pin(svc.chat_stream(None, "s1", "hello").unwrap());
    let mut request_id = None;
    while let Some(chunk) = stream.next().await {
        if let ChatChunk::Done { request_id: id, .. } = chunk.unwrap() {
            request_id = Some(id);
        }
    }
    assert_request_tree(&capture, &request_id.expect("Done chunk"));
}

#[tokio::test]
async fn telemetry_can_be_turned_off() {
    let capture = Capture::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));

    service(MockModel::new().script(MockResponse::text("hi")))
        .without_telemetry()
        .chat(None, "s1", "hello")
        .await
        .unwrap();
    let events = capture.events.lock().unwrap();
    assert!(!events.iter().any(|e| e == "run.end"), "{events:?}");
}
