# harness-redact

PII detection + redaction for [harness-rs](https://github.com/liliang-cn/harness-rs).
Published as `harness-rs-redact`.

An agent's memory, transcripts, tool results, and logs accumulate text that may
contain personally identifiable information — card numbers, emails, phones.
Dropping the whole record on a match is lossy and blunt; this crate **redacts in
place** and keeps the surrounding, still-useful text.

Three orthogonal axes (mirroring Microsoft Presidio / cloud DLP):

1. **Detect** — `Detector`s find `Span`s. `RegexDetector` with a checksum
   validator (e.g. `luhn_valid`) keeps false positives down — a 16-digit order
   number is not a card.
2. **Act** — a `Policy` maps each `PiiKind` to an `Action`: `Label` (`<EMAIL>`),
   `Mask` (`************1111`), `Hash` (`<EMAIL:ab12cd34>`, a stable pseudonym),
   `Block` (reject the whole record), or `Keep`.
3. **Apply** — `Redactor::scrub` runs detectors, resolves overlaps, and returns a
   `Redaction { text, spans, blocked }`.

```toml
[dependencies]
harness-rs-redact = "0.0.25"
```

```rust
use harness_redact::Redactor;

let r = Redactor::new();
let out = r.scrub("mail me at a@b.com, card 4111111111111111");
assert_eq!(out.text, "mail me at <EMAIL>, card ************1111");
assert!(!out.blocked);
```

## Custom detectors & policy

```rust
use harness_redact::{Redactor, Policy, Action, PiiKind, RegexDetector, luhn_valid};

let r = Redactor::empty()
    .with_detector(Box::new(
        // IBAN → label
        RegexDetector::new(PiiKind::Custom("iban".into()),
                           r"\b[A-Z]{2}\d{2}[A-Z0-9]{10,30}\b").unwrap(),
    ))
    .with_detector(Box::new(
        // card, only if it passes the Luhn checksum
        RegexDetector::new(PiiKind::CreditCard, r"\b\d{13,19}\b").unwrap()
            .with_validator(luhn_valid),
    ))
    .with_policy(
        Policy::new(Action::Label)                 // default: label everything
            .on(PiiKind::CreditCard, Action::Mask), // …but mask cards
    );
```

## Built-in policies

- `Policy::default()` — mask cards, label email/phone/custom, **keep money**.
  Good for transcripts (a conversation legitimately discusses prices).
- `Policy::memory_hygiene()` — same, but **block monetary amounts** (transaction
  figures belong in a ledger, not long-term memory).

## In harness-rs

`harness-context` layers this onto the `Memory` trait:

- **`GuardedMemory`** — redact-on-write for an agent's long-term memory; blocks
  drop the entry (default `memory_hygiene`).
- **`RedactingMemory`** — redact-only, never drops; wrap it around the `Memory`
  your transcript writer / experience store persists to, and the biggest PII
  leak (full transcripts → a CortexDB knowledge graph) closes in one place.

## License

MIT OR Apache-2.0
