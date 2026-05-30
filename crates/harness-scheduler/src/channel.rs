//! Delivery channels. A `Channel` takes a job's agent output and sends it
//! somewhere. Built-ins: `StdoutChannel`, `EmailChannel` (Resend). Apps add
//! their own (Telegram, Slack, …) by implementing `Channel`.

use crate::store::Job;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelError {
    #[error("channel send: {0}")]
    Send(String),
}

#[async_trait]
pub trait Channel: Send + Sync {
    /// Stable key matched against `Job.channel`.
    fn key(&self) -> &str;
    /// Deliver `output` for `job`. `job.target` is the recipient if relevant.
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError>;
}

/// Prints the output to stdout.
pub struct StdoutChannel;
impl StdoutChannel {
    pub fn new() -> Self {
        Self
    }
}
impl Default for StdoutChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for StdoutChannel {
    fn key(&self) -> &str {
        "stdout"
    }
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError> {
        println!("\n=== {} ===\n{}\n", job.name, output);
        Ok(())
    }
}

/// Sends the output as an email via Resend. Recipient = `job.target`.
pub struct EmailChannel {
    api_key: String,
    from: String,
    client: reqwest::Client,
}

/// Build the Resend POST body. Pure — unit-tested without network.
pub fn resend_body(from: &str, to: &str, subject: &str, text: &str) -> serde_json::Value {
    serde_json::json!({ "from": from, "to": [to], "subject": subject, "text": text })
}

impl EmailChannel {
    pub fn new(api_key: impl Into<String>, from: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            from: from.into(),
            client: reqwest::Client::new(),
        }
    }
    /// Construct from env: `RESEND_API_KEY` + `DIGEST_FROM` (falls back to a
    /// Resend test sender). Returns None if `RESEND_API_KEY` is absent.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("RESEND_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        let from = std::env::var("DIGEST_FROM")
            .unwrap_or_else(|_| "Scheduler <onboarding@resend.dev>".into());
        Some(Self::new(key, from))
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn key(&self) -> &str {
        "email"
    }
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError> {
        let to = job
            .target
            .as_deref()
            .ok_or_else(|| ChannelError::Send("email job has no target recipient".into()))?;
        let body = resend_body(&self.from, to, &job.name, output);
        let resp = self
            .client
            .post("https://api.resend.com/emails")
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| ChannelError::Send(format!("resend post: {e}")))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let code = resp.status();
            let t = resp.text().await.unwrap_or_default();
            Err(ChannelError::Send(format!("resend {code}: {t}")))
        }
    }
}

/// Maps channel keys to implementations.
#[derive(Default, Clone)]
pub struct ChannelRegistry {
    map: HashMap<String, Arc<dyn Channel>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, c: Arc<dyn Channel>) {
        self.map.insert(c.key().to_string(), c);
    }
    pub fn get(&self, key: &str) -> Option<&Arc<dyn Channel>> {
        self.map.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resend_body_shape() {
        let b = resend_body("Me <m@x.com>", "u@y.com", "Daily", "hello");
        assert_eq!(b["from"], "Me <m@x.com>");
        assert_eq!(b["to"][0], "u@y.com");
        assert_eq!(b["subject"], "Daily");
        assert_eq!(b["text"], "hello");
    }

    #[tokio::test]
    async fn email_without_target_errors() {
        let ch = EmailChannel::new("re_x", "Me <m@x.com>");
        let job = Job::new("j", "daily 08:00", "p", "email", 1);
        let err = ch.send("out", &job).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn registry_lookup() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(StdoutChannel::new()));
        assert!(reg.get("stdout").is_some());
        assert!(reg.get("email").is_none());
    }
}
