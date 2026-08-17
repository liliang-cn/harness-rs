//! What the run can show for itself.
//!
//! A finished run leaves a diff and a transcript. Neither says *this was
//! checked, against this contract, and it held* — so the reviewer of a large
//! change is back to reading everything, which is the job delegating the work
//! was supposed to remove. The transcript is not a substitute: it is long, it
//! is the agent's own account, and nothing in it distinguishes a check that
//! passed from a check that was never run.
//!
//! A [`Receipt`] is the short answer. One JSON object per run: what was asked,
//! which model answered, what the acceptance contract was, what the verdict
//! was, and whether the contract survived the run. Small enough to attach to a
//! pull request, and structured enough to fail a build on.
//!
//! **What the digest is for.** [`Receipt::digest`] is a hash of the receipt's
//! own content. It tells you two receipts are identical, and it catches a file
//! that was edited by hand or truncated in transit. It is *not* a signature:
//! anyone who can rewrite the receipt can recompute it. If you need the trail
//! itself to be tamper-evident, chain it —
//! `harness_hooks::audit::HashChainSink` already does that, and
//! [`Receipt::audit_request`] is where you put the id that points at it.

use crate::{Outcome, Verdict, seal::SealSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The one-page account of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Schema marker, so a reader can refuse a shape it does not know rather
    /// than silently misread one.
    pub schema: String,
    /// What was asked.
    pub task: String,
    /// Model handle that answered, as the provider names it.
    pub model: String,
    /// Wall-clock, milliseconds since the epoch. Supplied by the caller — this
    /// crate does not read the clock, so a receipt is reproducible in tests.
    pub finished_ms: i64,
    pub iters: u32,
    pub tools_called: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// `true` only when a check was asked *and* agreed. `false` covers both
    /// "checked and refused" and "nobody looked" — [`Self::checked`]
    /// distinguishes them, and the distinction matters more than the flag.
    pub passed: bool,
    /// Whether any acceptance check ran at all.
    pub checked: bool,
    /// Why it failed, verbatim from the check. Empty on a pass.
    pub reason: String,
    /// The sealed contract, path → digest, as it stood before the first turn.
    pub contract: SealSet,
    /// Set when a sealed file moved during the run. A receipt carrying this is
    /// evidence of tampering, not of work.
    pub seal_breach: Option<String>,
    /// The `audit.request` id, when the host runs an audit trail. Follow it to
    /// the full record; the receipt is the summary, not the evidence itself.
    pub audit_request: Option<String>,
    /// Hash over every field above. See the module docs for what it proves.
    pub digest: String,
}

/// Current [`Receipt::schema`].
pub const SCHEMA: &str = "harness.receipt.v1";

/// Assembles a [`Receipt`] from a finished run plus the things the loop does
/// not know: the clock, the model's name, and the audit id.
pub struct ReceiptBuilder {
    task: String,
    model: String,
    finished_ms: i64,
    audit_request: Option<String>,
}

impl ReceiptBuilder {
    pub fn new(task: impl Into<String>, model: impl Into<String>, finished_ms: i64) -> Self {
        Self {
            task: task.into(),
            model: model.into(),
            finished_ms,
            audit_request: None,
        }
    }

    pub fn with_audit_request(mut self, id: impl Into<String>) -> Self {
        self.audit_request = Some(id.into());
        self
    }

    /// Build from the outcome.
    ///
    /// Outcomes other than `Done` are receipted too, and as failures: a run
    /// that exhausted its budget produced no verified result, and a receipt
    /// that quietly omitted it would let "no receipt" and "a bad receipt" look
    /// the same to whatever is reading them.
    pub fn build(self, outcome: &Outcome) -> Receipt {
        let (iters, tools_called, usage, verified, contract, breach) = match outcome {
            Outcome::Done {
                iters,
                tools_called,
                usage,
                verified,
                contract,
                seal_breach,
                ..
            } => (
                *iters,
                *tools_called,
                usage.clone(),
                verified.clone(),
                contract.clone(),
                seal_breach.clone(),
            ),
            Outcome::BudgetExhausted {
                iters,
                tools_called,
                usage,
                ..
            } => (
                *iters,
                *tools_called,
                usage.clone(),
                Some(Verdict::failed("the run hit its budget before finishing")),
                SealSet::default(),
                None,
            ),
            _ => (
                0,
                0,
                harness_core::Usage::default(),
                Some(Verdict::failed("the run did not complete")),
                SealSet::default(),
                None,
            ),
        };

        let checked = verified.is_some();
        let passed = verified.as_ref().is_some_and(|v| v.passed) && breach.is_none();
        let reason = verified
            .as_ref()
            .filter(|v| !v.passed)
            .map(|v| v.reason.clone())
            .unwrap_or_default();

        let mut r = Receipt {
            schema: SCHEMA.to_string(),
            task: self.task,
            model: self.model,
            finished_ms: self.finished_ms,
            iters,
            tools_called,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            passed,
            checked,
            reason,
            contract,
            seal_breach: breach,
            audit_request: self.audit_request,
            digest: String::new(),
        };
        r.digest = r.compute_digest();
        r
    }
}

impl Receipt {
    /// Hash of every field but `digest` itself.
    ///
    /// Over the serialised form with the field cleared, rather than over a
    /// hand-written concatenation: a concatenation drifts the moment a field is
    /// added and starts silently covering less than it claims to.
    pub fn compute_digest(&self) -> String {
        let mut bare = self.clone();
        bare.digest = String::new();
        let json = serde_json::to_string(&bare).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(json.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// Whether the receipt still matches its own digest.
    pub fn intact(&self) -> bool {
        self.digest == self.compute_digest()
    }

    /// One line, for a build log or a PR comment.
    pub fn summary(&self) -> String {
        if let Some(b) = &self.seal_breach {
            return format!("REFUSED — the acceptance contract moved during the run ({b})");
        }
        match (self.checked, self.passed) {
            (false, _) => format!(
                "UNCHECKED — the model stopped after {} tool call(s); nothing verified it",
                self.tools_called
            ),
            (true, true) if self.contract.entries.is_empty() => {
                format!(
                    "PASSED — checked, nothing sealed, {} iteration(s)",
                    self.iters
                )
            }
            (true, true) => format!(
                "PASSED — checked against {} sealed file(s), which did not move, {} iteration(s)",
                self.contract.entries.len(),
                self.iters
            ),
            (true, false) => format!("FAILED — {}", self.reason),
        }
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        if let Some(d) = path.as_ref().parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).unwrap_or_default() + "\n",
        )
    }

    pub fn read_json(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        serde_json::from_str(&s).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Usage;

    fn done(verified: Option<Verdict>, breach: Option<&str>, contract: SealSet) -> Outcome {
        Outcome::Done {
            text: Some("hi".into()),
            iters: 3,
            tools_called: 5,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Default::default()
            },
            verified,
            contract,
            seal_breach: breach.map(String::from),
        }
    }

    fn sealed() -> SealSet {
        let mut s = SealSet::default();
        s.entries
            .insert("contract.txt".into(), Some("abc123".into()));
        s
    }

    #[test]
    fn a_pass_is_only_a_pass_when_the_seal_also_held() {
        let ok =
            ReceiptBuilder::new("t", "m", 1).build(&done(Some(Verdict::passed()), None, sealed()));
        assert!(ok.passed && ok.checked);

        // Same verdict, breached contract. The check agreed; the receipt must
        // not, because by then it was measuring something else.
        let bad = ReceiptBuilder::new("t", "m", 1).build(&done(
            Some(Verdict::passed()),
            Some("contract.txt was modified"),
            sealed(),
        ));
        assert!(!bad.passed, "a breached run cannot be a pass");
        assert!(bad.summary().starts_with("REFUSED"), "{}", bad.summary());
    }

    #[test]
    fn unchecked_is_not_the_same_as_failed() {
        // The distinction the whole artifact exists for: "nobody looked" must
        // never render as a failure OR as a pass.
        let r = ReceiptBuilder::new("t", "m", 1).build(&done(None, None, SealSet::default()));
        assert!(!r.checked);
        assert!(!r.passed);
        assert!(r.summary().starts_with("UNCHECKED"), "{}", r.summary());
    }

    #[test]
    fn a_failure_carries_the_checks_own_words() {
        let r = ReceiptBuilder::new("t", "m", 1).build(&done(
            Some(Verdict::failed("answer.txt must contain \"42\"")),
            None,
            sealed(),
        ));
        assert!(!r.passed && r.checked);
        assert!(r.reason.contains("42"));
        assert!(r.summary().starts_with("FAILED"));
    }

    #[test]
    fn an_exhausted_budget_receipts_as_a_failure_not_as_silence() {
        let o = Outcome::BudgetExhausted {
            iters: 9,
            last_text: Some("partway".into()),
            tools_called: 12,
            usage: Usage::default(),
        };
        let r = ReceiptBuilder::new("t", "m", 1).build(&o);
        assert!(!r.passed);
        assert!(r.checked, "a budget-out run has a stated reason");
        assert!(r.reason.contains("budget"));
    }

    #[test]
    fn editing_a_receipt_breaks_its_digest() {
        let mut r = ReceiptBuilder::new("t", "m", 1).build(&done(
            Some(Verdict::failed("nope")),
            None,
            sealed(),
        ));
        assert!(r.intact());
        // The edit someone would actually make.
        r.passed = true;
        r.reason = String::new();
        assert!(!r.intact(), "a flipped verdict must not still verify");
    }

    #[test]
    fn a_receipt_round_trips_through_disk_intact() {
        let d = std::env::temp_dir().join(format!("harness-receipt-{}", std::process::id()));
        let p = d.join("receipt.json");
        let r = ReceiptBuilder::new("ship it", "gpt", 1730000000000)
            .with_audit_request("req-7")
            .build(&done(Some(Verdict::passed()), None, sealed()));
        r.write_json(&p).unwrap();
        let back = Receipt::read_json(&p).unwrap();
        assert_eq!(back, r);
        assert!(back.intact());
        assert_eq!(back.audit_request.as_deref(), Some("req-7"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_summary_does_not_claim_a_count_it_does_not_have() {
        // It said "1 check(s) held" whatever had run. A receipt that rounds
        // its own evidence is the thing this module exists to replace.
        let sealed_pass =
            ReceiptBuilder::new("t", "m", 1).build(&done(Some(Verdict::passed()), None, sealed()));
        assert!(
            sealed_pass.summary().contains("1 sealed file"),
            "{}",
            sealed_pass.summary()
        );

        let unsealed_pass = ReceiptBuilder::new("t", "m", 1).build(&done(
            Some(Verdict::passed()),
            None,
            SealSet::default(),
        ));
        assert!(
            unsealed_pass.summary().contains("nothing sealed"),
            "{}",
            unsealed_pass.summary()
        );
    }

    #[test]
    fn the_digest_covers_fields_added_later() {
        // Guards the reason `compute_digest` serialises rather than
        // concatenating: a new field must change the hash without anyone
        // remembering to add it to a list.
        let a =
            ReceiptBuilder::new("t", "m", 1).build(&done(Some(Verdict::passed()), None, sealed()));
        let mut b = a.clone();
        b.contract
            .entries
            .insert("extra.txt".into(), Some("deadbeef".into()));
        assert_ne!(a.compute_digest(), b.compute_digest());
    }
}
