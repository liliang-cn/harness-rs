//! Built-in PII detectors and checksum validators.
//!
//! A [`Detector`] finds byte-offset [`Span`]s of one [`PiiKind`] in a string.
//! [`RegexDetector`] covers the common cases; attach a `validator` to cut the
//! false positives a bare regex can't (a 16-digit order number is not a card —
//! [`luhn_valid`] rejects it).

use crate::{Detector, PiiKind, Span};
use regex::Regex;

/// Regex-backed detector for one [`PiiKind`], with an optional post-match
/// validator. The regex finds *candidates*; the validator (if set) confirms
/// each one, so `\d{13,19}` + [`luhn_valid`] only reports digit runs that
/// actually pass the card checksum.
pub struct RegexDetector {
    kind: PiiKind,
    re: Regex,
    validator: Option<fn(&str) -> bool>,
}

impl RegexDetector {
    /// Build a detector. `pattern` is a standard `regex` crate pattern.
    pub fn new(kind: PiiKind, pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            kind,
            re: Regex::new(pattern)?,
            validator: None,
        })
    }

    /// Attach a validator run on every regex match; only matches for which it
    /// returns `true` become spans. Use to reject checksum-invalid candidates.
    pub fn with_validator(mut self, f: fn(&str) -> bool) -> Self {
        self.validator = Some(f);
        self
    }
}

impl Detector for RegexDetector {
    fn detect(&self, input: &str) -> Vec<Span> {
        self.re
            .find_iter(input)
            .filter(|m| self.validator.is_none_or(|f| f(m.as_str())))
            .map(|m| Span {
                start: m.start(),
                end: m.end(),
                kind: self.kind.clone(),
                text: m.as_str().to_string(),
            })
            .collect()
    }
}

/// Luhn (mod-10) checksum, ignoring non-digit separators. Returns `false` for
/// anything outside the 13–19 digit card range so short/long numeric ids don't
/// get flagged as cards.
pub fn luhn_valid(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        if i % 2 == 1 {
            let doubled = d * 2;
            sum += if doubled > 9 { doubled - 9 } else { doubled };
        } else {
            sum += d;
        }
    }
    sum.is_multiple_of(10)
}

/// The default detector set: Luhn-checked card numbers, emails, Chinese
/// mainland mobiles, and monetary amounts. Callers layer their own with
/// [`crate::Redactor::with_detector`].
pub fn default_detectors() -> Vec<Box<dyn Detector>> {
    // Unwraps are on compile-time-constant patterns tested below.
    vec![
        Box::new(
            RegexDetector::new(PiiKind::CreditCard, r"\b\d{13,19}\b")
                .unwrap()
                .with_validator(luhn_valid),
        ),
        Box::new(
            RegexDetector::new(
                PiiKind::Email,
                r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
            )
            .unwrap(),
        ),
        Box::new(RegexDetector::new(PiiKind::Phone, r"\b1[3-9]\d{9}\b").unwrap()),
        Box::new(RegexDetector::new(PiiKind::Money, r"[¥$€£₹]\s?\d+(?:[.,]\d+)?").unwrap()),
        Box::new(
            RegexDetector::new(
                PiiKind::Money,
                r"\b(?:USD|CNY|EUR|RMB|HKD|JPY)\s?\d+(?:[.,]\d+)?\b",
            )
            .unwrap(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_known_valid_cards() {
        // Visa/Mastercard test PANs (all pass Luhn).
        assert!(luhn_valid("4111111111111111"));
        assert!(luhn_valid("5500005555555559"));
        assert!(luhn_valid("4111 1111 1111 1111")); // separators ignored
    }

    #[test]
    fn luhn_rejects_random_16_digit_runs() {
        // The whole point: a 16-digit order/tracking number is not a card.
        assert!(!luhn_valid("1234567890123456"));
        assert!(!luhn_valid("4111111111111112")); // one digit off
    }

    #[test]
    fn luhn_rejects_out_of_range_lengths() {
        assert!(!luhn_valid("4111")); // too short
        assert!(!luhn_valid("41111111111111111111")); // 20 digits
    }

    #[test]
    fn card_detector_only_reports_luhn_valid() {
        let d = RegexDetector::new(PiiKind::CreditCard, r"\b\d{13,19}\b")
            .unwrap()
            .with_validator(luhn_valid);
        let spans = d.detect("valid 4111111111111111 invalid 1234567890123456");
        assert_eq!(spans.len(), 1, "only the Luhn-valid run should match");
        assert_eq!(spans[0].text, "4111111111111111");
    }

    #[test]
    fn default_patterns_all_compile() {
        // Constructing exercises every Regex::new — panics here if a pattern
        // regresses.
        assert_eq!(default_detectors().len(), 5);
    }
}
