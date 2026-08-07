//! Shared HTTP-failure classification for the image and speech adapters.
//!
//! Five adapters answer the same three questions — is this a rate limit, is it
//! worth retrying, and what does the operator need to see in the log. Five
//! copies of that logic would drift, and the way it drifts is silent: one
//! adapter maps `Throttling.RateQuota` to a rate limit and retries, another
//! calls it a provider error and fails the job.

/// Does this provider text indicate throttling / quota exhaustion?
///
/// Aliyun says `Throttling.RateQuota`, OpenAI says `rate_limit_exceeded`,
/// others just say "quota". Matching loosely is right here: a false positive
/// costs one wasted retry, a false negative fails a recoverable job.
pub(crate) fn looks_rate_limited(s: &str) -> bool {
    let b = s.to_ascii_lowercase();
    b.contains("ratequota")
        || b.contains("rate limit")
        || b.contains("rate_limit")
        || b.contains("throttling")
        || b.contains("quota")
        || b.contains("too many requests")
}

/// Does this provider text indicate the model/route cannot do what was asked?
pub(crate) fn looks_unsupported(s: &str) -> bool {
    let b = s.to_ascii_lowercase();
    b.contains("not supported")
        || b.contains("unsupported")
        || b.contains("unknown provider")
        || b.contains("does not support")
}

/// Which error variant an HTTP status + body deserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Retry: throttled.
    RateLimited,
    /// Retry: the far side wobbled.
    Transient,
    /// Do not retry: the request asks for something this route cannot do.
    Unsupported,
    /// Do not retry: the provider refused for some other reason.
    Permanent,
}

/// Classify an HTTP response.
///
/// Body-sniffing matters as much as the status: Aliyun returns
/// `Throttling.RateQuota` under a 4xx, not a 429, so status alone would send a
/// recoverable stall down the permanent path.
pub(crate) fn classify(status: u16, body: &str) -> Kind {
    if status == 429 || looks_rate_limited(body) {
        return Kind::RateLimited;
    }
    if (500..600).contains(&status) || status == 408 {
        return Kind::Transient;
    }
    if looks_unsupported(body) {
        return Kind::Unsupported;
    }
    Kind::Permanent
}

/// Trim a provider body down to something loggable.
///
/// These bodies routinely carry a megabyte of base64 image data. An untrimmed
/// error message makes the log unreadable and can itself become the problem.
pub(crate) fn truncate(s: &str) -> String {
    const MAX: usize = 400;
    if s.len() <= MAX {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .nth(MAX)
        .map(|(i, _)| i)
        .unwrap_or_else(|| s.len());
    format!("{}… ({} bytes total)", &s[..cut], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliyun_throttling_under_a_4xx_is_still_a_rate_limit() {
        // Measured 2026-08-07: this arrives with a 4xx status, in ~0.14s.
        let body = r#"{"code":"Throttling.RateQuota","message":"Requests rate limit exceeded"}"#;
        assert_eq!(classify(400, body), Kind::RateLimited);
    }

    #[test]
    fn statuses_map_as_expected() {
        assert_eq!(classify(429, "slow down"), Kind::RateLimited);
        assert_eq!(classify(503, "bad gateway"), Kind::Transient);
        assert_eq!(classify(408, "timeout"), Kind::Transient);
        assert_eq!(classify(401, "bad key"), Kind::Permanent);
    }

    #[test]
    fn unsupported_routes_are_recognised() {
        // Both measured on cpa.superleo.app, 2026-08-07.
        assert_eq!(
            classify(
                400,
                "Model gemini-3.1-flash-image is not supported on /v1/images/generations"
            ),
            Kind::Unsupported
        );
        assert_eq!(
            classify(502, "unknown provider for model gpt-image-1.5"),
            Kind::Transient,
            "5xx wins: a gateway hiccup and a bad model id look alike, and retrying is cheap"
        );
        assert_eq!(
            classify(400, "unknown provider for model gpt-image-1.5"),
            Kind::Unsupported
        );
    }

    #[test]
    fn truncate_is_utf8_safe_and_annotates_the_cut() {
        let long = "熊".repeat(1000); // 3 bytes per char
        let t = truncate(&long);
        assert!(t.contains("3000 bytes total"));
        assert!(t.starts_with('熊'), "must not split a multi-byte char");

        assert_eq!(truncate("short"), "short");
    }
}
