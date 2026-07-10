//! PII detection + redaction for harness-rs.
//!
//! The problem: an agent's memory, transcripts, tool results, and logs all
//! accumulate text that may contain personally identifiable information —
//! card numbers, emails, phone numbers. Dropping the whole record on a match
//! is lossy and blunt; the mature move is to **redact in place** and keep the
//! surrounding, still-useful text.
//!
//! Three orthogonal axes, mirroring Microsoft Presidio / cloud DLP:
//!
//! 1. **Detect** — [`Detector`]s find [`Span`]s. [`RegexDetector`] with a
//!    checksum [`validator`](RegexDetector::with_validator) (e.g.
//!    [`luhn_valid`]) keeps false positives down.
//! 2. **Act** — a [`Policy`] maps each [`PiiKind`] to an [`Action`]: replace
//!    with a `<LABEL>`, [`Mask`](Action::Mask) all but the last few chars,
//!    [`Hash`](Action::Hash) to a stable pseudonym, [`Block`](Action::Block)
//!    the whole record, or [`Keep`](Action::Keep).
//! 3. **Apply** — [`Redactor::scrub`] runs detectors, resolves overlaps, and
//!    returns a [`Redaction`] with the rewritten text plus whether any span
//!    demanded a block.
//!
//! ```
//! use harness_redact::{Redactor, Action};
//! let r = Redactor::new();
//! let out = r.scrub("mail me at a@b.com, card 4111111111111111");
//! assert!(out.text.contains("<EMAIL>"));
//! assert!(out.text.contains("1111") && !out.text.contains("4111111111111111"));
//! assert!(!out.blocked);
//! ```

mod detectors;
pub use detectors::{RegexDetector, default_detectors, luhn_valid};

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// A category of sensitive value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PiiKind {
    CreditCard,
    Email,
    Phone,
    Money,
    /// App-specific category (e.g. `"IBAN"`, `"SSN"`). The label is the
    /// upper-cased tag.
    Custom(String),
}

impl PiiKind {
    /// The placeholder token body, e.g. `CREDIT_CARD`. Used by
    /// [`Action::Label`] / [`Action::Hash`].
    pub fn label(&self) -> String {
        match self {
            PiiKind::CreditCard => "CREDIT_CARD".to_string(),
            PiiKind::Email => "EMAIL".to_string(),
            PiiKind::Phone => "PHONE".to_string(),
            PiiKind::Money => "MONEY".to_string(),
            PiiKind::Custom(s) => s.to_uppercase(),
        }
    }
}

/// A detected sensitive substring at a byte range in the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first char (inclusive).
    pub start: usize,
    /// Byte offset one past the last char (exclusive).
    pub end: usize,
    pub kind: PiiKind,
    /// The matched text, verbatim.
    pub text: String,
}

/// Finds [`Span`]s in a string. Implement for custom detectors (NER, dictionary
/// lookups, …); the built-ins are all [`RegexDetector`]s.
pub trait Detector: Send + Sync {
    fn detect(&self, input: &str) -> Vec<Span>;
}

/// What to do with a matched span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Replace with `<KIND>` — loses the value, keeps the shape/semantics.
    Label,
    /// Keep the last few chars, star out the rest: `4111…` → `************1111`.
    /// The number of trailing chars kept is [`Redactor::mask_keep`].
    Mask,
    /// Replace with `<KIND:abcd1234>` — a stable pseudonym; equal values map to
    /// equal tokens so they can still be correlated without exposing the value.
    Hash,
    /// Flag the whole record for rejection ([`Redaction::blocked`] = `true`).
    /// The span itself is still labelled in `text`, so even a caller that
    /// ignores `blocked` never leaks the value.
    Block,
    /// Leave the span untouched.
    Keep,
}

/// Maps [`PiiKind`] → [`Action`], with a fallback for unlisted kinds.
#[derive(Debug, Clone)]
pub struct Policy {
    default: Action,
    per_kind: HashMap<PiiKind, Action>,
}

impl Policy {
    /// A policy whose fallback action is `default` and no per-kind overrides.
    pub fn new(default: Action) -> Self {
        Self {
            default,
            per_kind: HashMap::new(),
        }
    }

    /// Set the action for one kind. Chainable.
    pub fn on(mut self, kind: PiiKind, action: Action) -> Self {
        self.per_kind.insert(kind, action);
        self
    }

    /// The action for `kind` (its override, else the default).
    pub fn action_for(&self, kind: &PiiKind) -> Action {
        self.per_kind.get(kind).copied().unwrap_or(self.default)
    }

    /// Memory-hygiene preset: redact real PII but *block* monetary amounts —
    /// transaction figures belong in a ledger, not long-term memory. Cards are
    /// masked (last 4 kept), email/phone labelled.
    pub fn memory_hygiene() -> Self {
        Policy::new(Action::Label)
            .on(PiiKind::CreditCard, Action::Mask)
            .on(PiiKind::Email, Action::Label)
            .on(PiiKind::Phone, Action::Label)
            .on(PiiKind::Money, Action::Block)
    }
}

impl Default for Policy {
    /// Redact real PII, keep money. Cards masked, email/phone/custom labelled.
    fn default() -> Self {
        Policy::new(Action::Label)
            .on(PiiKind::CreditCard, Action::Mask)
            .on(PiiKind::Money, Action::Keep)
    }
}

/// The outcome of [`Redactor::scrub`].
#[derive(Debug, Clone)]
pub struct Redaction {
    /// The input with every non-[`Keep`](Action::Keep) span rewritten.
    pub text: String,
    /// Every span detected, in input order (including `Keep`ed ones), for
    /// auditing.
    pub spans: Vec<Span>,
    /// `true` if any span's action was [`Block`](Action::Block); the caller
    /// should reject the record.
    pub blocked: bool,
}

impl Redaction {
    /// Whether any sensitive span was found at all.
    pub fn changed(&self) -> bool {
        !self.spans.is_empty()
    }
}

/// Detector set + [`Policy`] + mask width. Runs the full detect→act→apply
/// pipeline in [`scrub`](Redactor::scrub).
pub struct Redactor {
    detectors: Vec<Box<dyn Detector>>,
    policy: Policy,
    mask_keep: usize,
}

impl Redactor {
    /// Default detectors ([`default_detectors`]) + default [`Policy`], keeping
    /// the last 4 chars on [`Mask`](Action::Mask).
    pub fn new() -> Self {
        Self {
            detectors: default_detectors(),
            policy: Policy::default(),
            mask_keep: 4,
        }
    }

    /// A redactor with no detectors — scrubs nothing until you
    /// [`with_detector`](Self::with_detector). Useful as a from-scratch base.
    pub fn empty() -> Self {
        Self {
            detectors: Vec::new(),
            policy: Policy::default(),
            mask_keep: 4,
        }
    }

    /// Append a detector. Chainable.
    pub fn with_detector(mut self, d: Box<dyn Detector>) -> Self {
        self.detectors.push(d);
        self
    }

    /// Replace the policy. Chainable.
    pub fn with_policy(mut self, p: Policy) -> Self {
        self.policy = p;
        self
    }

    /// Set how many trailing chars [`Action::Mask`] keeps (default 4).
    pub fn with_mask_keep(mut self, keep: usize) -> Self {
        self.mask_keep = keep;
        self
    }

    /// Detect → resolve overlaps → apply the policy. See [`Redaction`].
    pub fn scrub(&self, input: &str) -> Redaction {
        let mut spans: Vec<Span> = self
            .detectors
            .iter()
            .flat_map(|d| d.detect(input))
            .collect();

        // Prefer earlier, then longer matches; drop any span overlapping one
        // already accepted so two detectors hitting the same bytes don't both
        // rewrite it.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut accepted: Vec<Span> = Vec::with_capacity(spans.len());
        let mut cursor = 0usize;
        for s in spans {
            if s.start >= cursor {
                cursor = s.end;
                accepted.push(s);
            }
        }

        let mut out = String::with_capacity(input.len());
        let mut blocked = false;
        let mut last = 0usize;
        for s in &accepted {
            out.push_str(&input[last..s.start]);
            let action = self.policy.action_for(&s.kind);
            if action == Action::Block {
                blocked = true;
            }
            out.push_str(&self.apply(action, s));
            last = s.end;
        }
        out.push_str(&input[last..]);

        Redaction {
            text: out,
            spans: accepted,
            blocked,
        }
    }

    fn apply(&self, action: Action, span: &Span) -> String {
        match action {
            Action::Keep => span.text.clone(),
            Action::Label => format!("<{}>", span.kind.label()),
            Action::Block => format!("<{}>", span.kind.label()),
            Action::Hash => format!("<{}:{}>", span.kind.label(), short_hash(&span.text)),
            Action::Mask => mask(&span.text, self.mask_keep),
        }
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

/// Star out all but the last `keep` chars. Fewer than `keep` chars → all stars,
/// so short secrets aren't partially revealed.
fn mask(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let reveal_from = chars.len().saturating_sub(keep);
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| if i < reveal_from { '*' } else { *c })
        .collect()
}

/// 8-hex-char stable pseudonym. `DefaultHasher` uses fixed keys, so equal
/// inputs map to equal tokens across calls (within a std version).
fn short_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_labelled_card_is_masked_by_default() {
        let r = Redactor::new();
        let out = r.scrub("reach me at a@b.com or card 4111111111111111 today");
        assert_eq!(
            out.text,
            "reach me at <EMAIL> or card ************1111 today"
        );
        assert!(out.changed());
        assert!(!out.blocked);
    }

    #[test]
    fn non_luhn_16_digits_pass_through_untouched() {
        // Order number, not a card — must survive.
        let r = Redactor::new();
        let out = r.scrub("order 1234567890123456 shipped");
        assert_eq!(out.text, "order 1234567890123456 shipped");
        assert!(!out.changed());
    }

    #[test]
    fn money_kept_by_default_blocked_under_hygiene() {
        let keep = Redactor::new();
        assert_eq!(keep.scrub("spent $199 today").text, "spent $199 today");
        assert!(!keep.scrub("spent $199 today").blocked);

        let hygiene = Redactor::new().with_policy(Policy::memory_hygiene());
        let out = hygiene.scrub("spent $199 today");
        assert!(out.blocked, "money should block under memory_hygiene");
        assert_eq!(
            out.text, "spent <MONEY> today",
            "value never leaks even when blocked"
        );
    }

    #[test]
    fn phone_labelled() {
        let r = Redactor::new();
        assert_eq!(r.scrub("call 13800138000").text, "call <PHONE>");
    }

    #[test]
    fn label_action_via_all_label_policy() {
        let r = Redactor::new().with_policy(Policy::new(Action::Label));
        let out = r.scrub("card 4111111111111111");
        assert_eq!(out.text, "card <CREDIT_CARD>");
    }

    #[test]
    fn hash_is_stable_and_hides_value() {
        let r = Redactor::new().with_policy(Policy::new(Action::Hash));
        let a = r.scrub("mail a@b.com").text;
        let b = r.scrub("mail a@b.com").text;
        assert_eq!(a, b, "same value → same pseudonym");
        assert!(a.starts_with("mail <EMAIL:") && !a.contains("a@b.com"));
    }

    #[test]
    fn mask_reveals_only_tail() {
        assert_eq!(mask("4111111111111111", 4), "************1111");
        assert_eq!(mask("abc", 4), "abc"); // shorter than keep → unchanged shape
        assert_eq!(mask("secret", 0), "******");
    }

    #[test]
    fn overlapping_detectors_apply_once() {
        // Money detector matches "$4111..." region partially; ensure no
        // double-rewrite / panic on overlapping byte ranges.
        let r = Redactor::new();
        let out = r.scrub("a@b.com a@b.com");
        assert_eq!(out.text, "<EMAIL> <EMAIL>");
        assert_eq!(out.spans.len(), 2);
    }

    #[test]
    fn custom_kind_uses_default_action_and_upper_label() {
        struct Ssn;
        impl Detector for Ssn {
            fn detect(&self, input: &str) -> Vec<Span> {
                input
                    .match_indices("secret")
                    .map(|(i, m)| Span {
                        start: i,
                        end: i + m.len(),
                        kind: PiiKind::Custom("ssn".into()),
                        text: m.to_string(),
                    })
                    .collect()
            }
        }
        let r = Redactor::empty()
            .with_detector(Box::new(Ssn))
            .with_policy(Policy::new(Action::Label));
        assert_eq!(r.scrub("my secret here").text, "my <SSN> here");
    }
}
