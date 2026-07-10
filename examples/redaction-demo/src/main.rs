//! PII redaction across harness-rs — the engine and the two `Memory` decorators.
//!
//! ```sh
//! cargo run -p redaction-demo
//! ```
//!
//! No network, no API key, no binaries: everything here is local and
//! deterministic. For scanned-PDF OCR (`read_document` + the `ocr-tesseract`
//! feature), see `crates/harness-tools-docs` — it needs `pdftoppm` + `tesseract`
//! on PATH at runtime, so it isn't exercised in this CI-safe demo.

use async_trait::async_trait;
use harness_context::{GuardedMemory, RedactingMemory};
use harness_core::{Memory, MemoryEntry, MemoryError};
use harness_redact::{Action, PiiKind, Policy, Redactor};
use std::sync::{Arc, Mutex};

/// A trivial in-memory `Memory` so the demo prints what actually got stored.
#[derive(Default)]
struct VecMemory(Mutex<Vec<MemoryEntry>>);

#[async_trait]
impl Memory for VecMemory {
    async fn recall(&self, _q: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        Ok(self.0.lock().unwrap().iter().take(k).cloned().collect())
    }
    async fn write(&self, e: MemoryEntry) -> Result<(), MemoryError> {
        self.0.lock().unwrap().push(e);
        Ok(())
    }
}

async fn dump(label: &str, mem: &Arc<dyn Memory>) {
    let all = mem.recall("", 100).await.unwrap();
    println!("  {label}: {} stored", all.len());
    for e in all {
        println!("    · {}", e.content);
    }
}

#[tokio::main]
async fn main() {
    // 1) The engine, standalone — scrub any text.
    println!("== 1. Redactor::scrub (default policy) ==");
    let r = Redactor::new();
    for line in [
        "email a@b.com and card 4111111111111111",
        "order 1234567890123456 shipped", // 16 digits but NOT Luhn-valid → kept
        "call me on 13800138000",
        "the plan costs $20 / month", // money kept by default
    ] {
        let out = r.scrub(line);
        println!("  {line}\n    -> {}", out.text);
    }

    // A stricter, hashing policy: equal values map to equal pseudonyms.
    println!("\n== 2. Custom policy: hash everything ==");
    let hasher = Redactor::new().with_policy(Policy::new(Action::Hash));
    println!("  {}", hasher.scrub("ping a@b.com, again a@b.com").text);

    // 3) GuardedMemory — redact on write into an agent's long-term memory.
    //    Cards masked, email/phone labelled, money & secrets DROPPED.
    println!("\n== 3. GuardedMemory (agent memory: redact, drop money/secrets) ==");
    let backing: Arc<dyn Memory> = Arc::new(VecMemory::default());
    let guarded: Arc<dyn Memory> =
        Arc::new(GuardedMemory::new(backing.clone()).with_blocked_substring("password"));
    for m in [
        "user's card is 4111111111111111", // → masked, kept
        "contact ll_faw@hotmail.com",      // → <EMAIL>, kept
        "user spent ¥199 on hotpot",       // → money, DROPPED
        "the password is hunter2",         // → blocked substring, DROPPED
        "user prefers dark mode",          // → clean, kept as-is
    ] {
        guarded.write(MemoryEntry::new(m)).await.unwrap();
    }
    dump("agent memory", &backing).await;

    // 4) RedactingMemory — the persistence boundary (transcripts → CortexDB).
    //    Redact-only: never drops a turn, and money is KEPT (a transcript may
    //    legitimately quote a price).
    println!("\n== 4. RedactingMemory (transcript/experience: redact, never drop) ==");
    let sink: Arc<dyn Memory> = Arc::new(VecMemory::default());
    let safe: Arc<dyn Memory> = Arc::new(
        RedactingMemory::new(sink.clone())
            // override the policy if you want, e.g. block money here too:
            .with_redactor(Redactor::new().with_policy(Policy::default())),
    );
    for turn in [
        "[tool] read file: card 4111111111111111, email a@b.com",
        "assistant: the invoice total is $20",
        "user: thanks!",
    ] {
        safe.write(MemoryEntry::new(turn)).await.unwrap();
    }
    dump("transcript sink", &sink).await;

    println!(
        "\nTakeaway: wrap agent memory in GuardedMemory, wrap the persistence \n\
         boundary in RedactingMemory, scrub arbitrary text with Redactor.\n\
         Custom kinds/detectors: RegexDetector + Policy::on(PiiKind::Custom(..), _)."
    );
    let _ = PiiKind::Custom("iban".into()); // named for discoverability
}
