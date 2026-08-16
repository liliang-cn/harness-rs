# Per-turn cost of the prompt-injection and memory machinery

Four guides now run on every model turn, plus a gate that runs after every
episode. Nobody had measured them. This is the measurement.

**Short version.** Three of the four guides are free. The one that is not free
is not free because of the *store backend*, not the guide: `FileMemory` reads
and re-parses the entire JSONL file on every recall, so `MemoryGuide` and
`ExperienceGuide` cost 41 ms and 58 ms **per model iteration** at 50 000
entries. `UserModelGuide` is the cheapest thing in the system after the first
turn (167 ns). `SkillDistiller::gate` really does make zero model calls and
zero I/O — but its accept path allocates 26 000 times at 500 skills. Two things
the benchmark was not looking for fell out of it anyway, one of them a
correctness bug; see [Surprises](#surprises).

---

## Methodology

There is **no `criterion` anywhere in this workspace** (`grep -rn criterion
--include=Cargo.toml .` returns nothing), and the brief said not to add it. So
these are **plain wall-clock timings, not statistically-rigorous criterion
runs**: no outlier rejection, no bootstrapped confidence intervals, no
regression against a saved baseline.

What they do have:

- a warmup phase whose samples are discarded;
- 10–2 000 timed iterations per case, sized so the slow cases still finish;
- **min / median / mean** reported together, so a single scheduler hiccup shows
  up as a mean/median gap instead of hiding inside one number;
- sub-microsecond operations timed in batches of 20–50 and divided, so we are
  not measuring `Instant::now()` resolution;
- `std::hint::black_box` around inputs and results.

Read the **min** as "what this costs", and the median/mean gap as noise. Where
they disagree by more than ~10 % it is called out.

Everything runs offline. No network, no API keys. The two model-backed paths
(`UserModelUpdater`'s LLM call, `SkillDistiller::distill`'s LLM call) are
explicitly **not** measured — only the deterministic per-turn work. The
distiller bench wires a tripwire `Model` that increments a counter and returns
an error; the counter is asserted to be `0` at the end of the run.

### Machine

| | |
|---|---|
| CPU | `Apple M2 Pro` (`sysctl -n machdep.cpu.brand_string`), 12 cores |
| RAM | 32 GB |
| OS | macOS 27.0 (Darwin 27.0.0) |
| rustc | 1.92.0 (ded5c06cf 2025-12-08) |
| profile | `bench` (= release, opt-level 3) |

### Running them

```sh
cargo bench -p harness-rs-loop          --bench memory_guide
cargo bench -p harness-rs-experience    --bench experience_guide
cargo bench -p harness-rs-experience    --bench distill_gate
cargo bench -p harness-rs-experience    --bench distill_gate_allocs
cargo bench -p harness-rs-context       --bench user_model
cargo bench -p harness-rs-tools-recall  --bench recall_guide
```

Each bench target is declared `harness = false` and is a plain `fn main()` —
`#[bench]` would need nightly.

### What "store size" means for each component

The four components do not share a backend, so "50 000 items" is not one thing:

| Component | Backend under test | 1 item = |
|---|---|---|
| `MemoryGuide` | `harness_context::FileMemory` (shipped JSONL) | one `MemoryEntry` line |
| `ExperienceGuide` | `FileMemory` via `ExperienceStore` | one rendered `Episode` |
| `UserModelGuide` | `FileMemory` via `UserModelStore` | one unrelated `MemoryEntry` (portrait + N noise rows) |
| `RecallGuide` | `harness_recall_sqlite::SqliteRecall` (in-memory, FTS5 + trigram) | one transcript message |

`MemoryGuide` and `ExperienceGuide` are also measured against an O(1) stub
backend, so the guide's own work (filter, score, parse, format, strip-and-push)
can be separated from the backend's. Those stub figures also include building a
fresh `Context` per iteration, which is how the loop would present one — so they
are a slight over-estimate of the guide in isolation, not an under-estimate.

---

## 1. Latency and rendered size, by store size

`apply_before_iter` — the path that runs before **every model call**, not every
user message. A turn that uses three tools pays this four times.

| Component | store size | latency (min) | latency (median) | rendered chars |
|---|---:|---:|---:|---:|
| **`MemoryGuide`** (top_k=5) | O(1) stub | **4.7 µs** | 6.2 µs | 948 |
| | 10 | 49.5 µs | 51.5 µs | 948 |
| | 1 000 | 785 µs | 817 µs | 948 |
| | 50 000 | **40.9 ms** | 41.4 ms | 948 |
| `MemoryGuide` + filters (min_score, excluded_tags → 3× over-fetch) | 1 000 | 791 µs | 826 µs | 948 |
| | 50 000 | 41.0 ms | 41.6 ms | 948 |
| `MemoryGuide`, CJK corpus, **sentence** query | 1 000 | 1.41 ms | 1.42 ms | **0** |
| | 50 000 | **72.2 ms** | 73.1 ms | **0** |
| `MemoryGuide`, CJK corpus, contiguous-substring query | 1 000 | 1.07 ms | 1.10 ms | 398 |
| | 50 000 | 55.8 ms | 56.6 ms | 398 |
| **`ExperienceGuide`** (top_k=3) | O(1) stub | **9.3 µs** | 9.8 µs | 1 093 |
| | 10 | 51.9 µs | 61.0 µs | 1 093 |
| | 1 000 | 1.07 ms | 1.10 ms | 1 105 |
| | 50 000 | **57.5 ms** | 58.2 ms | 1 117 |
| **`UserModelGuide`** cold (session start) | 10 | 64.9 µs | 66.3 µs | 1 159 |
| | 1 000 | 594 µs | 613 µs | 1 159 |
| | 50 000 | **28.9 ms** | 29.3 ms | 1 159 |
| **`UserModelGuide`** warm (every later turn) | 10 | **125 ns** | 167 ns | 1 159 |
| | 1 000 | 125 ns | 167 ns | 1 159 |
| | 50 000 | **125 ns** | 167 ns | 1 159 |
| `UserModelGuide` cold, **unknown user** | 1 000 | 605 µs | 611 µs | 0 |
| | 50 000 | 28.7 ms | 29.2 ms | 0 |
| **`RecallGuide`** (top_k=2), ascii hit | 10 | 82 µs | 120 µs | 288 |
| | 1 000 | 343 µs | 346 µs | 288 |
| | 50 000 | **13.2 ms** | 13.3 ms | 288 |
| `RecallGuide`, CJK verbatim hit | 1 000 | 525 µs | 542 µs | 230 |
| | 50 000 | 19.2 ms | 19.4 ms | 230 |
| `RecallGuide`, CJK fallback hit (3 store calls) | 1 000 | 580 µs | 583 µs | 295 |
| | 50 000 | 21.1 ms | 21.1 ms | 299 |
| `RecallGuide`, **worst-case CJK miss** (13 store calls, 5 of them `LIKE`) | 1 000 | 924 µs | 927 µs | 0 |
| | 50 000 | **34.6 ms** | 35.1 ms | 0 |

Scaling is **linear everywhere**. No quadratic landmine was found. The
`MemoryGuide` slope is ~0.82 µs per JSONL entry (ASCII) and ~1.46 µs per entry
(CJK — more bytes per line to read, UTF-8-validate, and lowercase);
`ExperienceGuide`'s is ~1.16 µs per entry, the extra being `Episode::parse` on
the over-fetched hits.

The 50 000-entry numbers are all "one linear pass over the whole store", which
is exactly what `FileMemory`'s own doc comment says it does:

> Reads stat+read the whole file on each recall — fine for the kilobyte-scale
> memories these JSONL stores realistically hold.

That caveat is accurate and it is the whole story. The guides are not slow; the
default backend is not an index.

### Rendered size, and the CJK/ASCII token gap

Rendered size is essentially **flat in store size** — every guide caps what it
injects (`top_k`, a per-snippet clip, a char budget), and the tables above
confirm the caps hold. That is the good news: the recurring prompt bill does
not grow as the store grows.

Chars are reported because that is what the code budgets in. Converting to
tokens is not one number:

- **ASCII English** — roughly **4 chars per token** on BPE tokenizers.
- **Chinese (han)** — roughly **1–1.5 chars per token**; most tokenizers emit
  about one token per han character.

So the same 1 200-char budget buys ~300 tokens of English and ~1 000 tokens of
Chinese. Applying that to the totals below:

| Injection | chars | ≈ tokens (ASCII) | ≈ tokens (CJK) |
|---|---:|---:|---:|
| `MemoryGuide` (top_k=5) | 948 | ~240 | ~800 |
| `ExperienceGuide` (top_k=3) | ~1 100 | ~275 | ~900 |
| `UserModelGuide` (1 200-char budget) | 1 159 | ~290 | ~950 |
| `RecallGuide` (top_k=2) | 230–299 | ~60–75 | ~230–300 |
| **all four stacked** | **~3 500** | **~875** | **~2 950** |

**This is the number that actually matters.** ~875 prompt tokens on every
request for an English deployment, and ~2 950 for a Chinese one, before the
system prompt, the tool schemas, or the conversation. At 20 turns that is 17 k
/ 59 k tokens of pure repetition. The `PortraitPolicy` doc comment estimates
"~1200 chars is roughly 300 tokens" — true for English, off by 3× for the
zh-CN users this project actually serves.

Note also that the guides run **sequentially** in `AgentLoop`
(`for g in &all_guides { … .await }`), so their latencies add. They are not
overlapped.

---

## 2. The CJK fallback in `harness-tools-recall`

The docs describe the cost as "up to 12 extra store queries". Probe counts
below are **observed**, via a `CountingRecall` decorator that wraps
`SqliteRecall` and counts `search` calls.

| Query shape | store calls | 10 msgs | 1 000 msgs | 50 000 msgs |
|---|---:|---:|---:|---:|
| ascii hit | 1 | 82 µs | 343 µs | 13.2 ms |
| ascii miss | 1 | 31 µs | 164 µs | 6.5 ms |
| cjk verbatim hit (trigram) | 1 | 112 µs | 525 µs | 19.2 ms |
| cjk fallback hit (chunk size 4 lands) | 3 | 129 µs | 580 µs | 21.1 ms |
| cjk miss, short query (4 han chars) | 4 | 64 µs | 350 µs | 13.9 ms |
| cjk miss, long query (36 han chars) | **13** | 166 µs | 216 µs | **170 µs** |
| **cjk miss, 12 han chars** | **13** | 198 µs | 924 µs | **34.6 ms** |

**The probe count is the wrong thing to worry about.** The 36-han-char query
issues the full 13 store calls and is the *cheapest* case in the table at
50 000 messages — 170 µs, 78× faster than a single-probe ASCII hit.

What actually drives cost is which SQL path each probe takes, and there are two
independent multipliers:

1. **Hydration, not matching.** A *hit* is expensive because `SqliteRecall`
   then runs `meta_of` + `read_window(±5)` + `read_first(3)` + `read_last(3)` +
   `snippet()` for each returned session. A probe that matches nothing skips
   all of it. That is why 13 probes that all miss beat 1 probe that hits.

2. **`LIKE` full table scans.** The backend routes a query to the FTS5 trigram
   index only when `count_cjk(query) >= 3`. A **2-character chunk** therefore
   goes to plain FTS (which cannot tokenise space-free han text), misses, and
   falls through to `content LIKE '%…%'` — a full table scan. Measured cost:
   **~6.5 ms per scan at 50 000 messages** (see the ascii-miss row, which is
   exactly one scan).

The worst case is the query length that leaves the most budget for 2-char
chunks. For a 12-han-char run: 3 probes on 4-char chunks, 4 on 3-char chunks,
leaving 5 of the 12-probe budget for 2-char chunks — **five full table scans in
one turn**. Predicted 5 × 6.5 ms ≈ 33 ms; measured **35.1 ms**. The 36-char
query is cheap precisely *because* it burns its whole budget on 4- and 3-char
chunks and never reaches the `LIKE` ladder.

So: **the real ceiling is ~5 full table scans, not 12 probes**, and it is hit by
short-to-medium Chinese questions — the common case — not by long ones.

---

## 3. `SkillDistiller::gate`

Runs after every episode. The claim under test was "pure arithmetic, zero LLM
cost on rejection".

| Path | existing skills | latency (min) | allocations / call | bytes / call |
|---|---:|---:|---:|---:|
| `Skip(NotSuccessful)` | 500 | **2 ns** | 0 | 0 |
| `Skip(TooFewToolCalls)` | 500 | **4 ns** | 0 | 0 |
| `Skip(TooFewDistinctTools)` | 500 | 81 ns | 0 | 0 |
| `Skip(SituationTooShort)` | 500 | 77 ns | 1 | 80 |
| `Distill` (full duplicate scan) | 0 | 300 ns | 1 | 80 |
| `Distill` | 50 | 196 µs | **2 591** | 178 KB |
| `Distill` | 500 | **2.00 ms** | **25 991** | **1.78 MB** |
| `Skip(DuplicateSkill)` | 501 | 1.96 ms | — | — |

**Model calls made across every `gate()` invocation above: 0.** Asserted, not
assumed.

**Zero I/O: confirmed, by construction.** `gate(&self, ep, existing:
&[ExistingSkill])` is handed the skills list already in memory; it has no path
to a filesystem. The I/O is in `existing_skills_in(root)`, the *separate* helper
that builds that slice — measured here because a host that rebuilds the list per
episode pays it per episode:

| `existing_skills_in(root)` | latency (min) |
|---|---:|
| 0 skills | 1.3 µs |
| 50 skills | 1.33 ms |
| 500 skills | **13.3 ms** |

**Allocation-light: true on rejection, false on acceptance.** The early
rejections allocate literally nothing and cost single-digit nanoseconds — the
claim is exactly right for the case it was written about. But the duplicate
scan is ~52 allocations per existing skill, because `overlap_similarity(a, b)`
calls `tokens()` on *both* arguments, and the episode's situation is
re-tokenised into a fresh `HashSet<String>` once per skill:

```rust
// distill.rs — nearest()
existing.iter().map(|s| {
    let hay = format!("{} {}", s.name.replace('-', " "), s.description);
    (s.name.clone(), skillmd::overlap_similarity(text, &hay))  // re-tokenises `text` each time
})
```

At 500 skills that is 1.78 MB of churn to make one boolean decision.

---

## 4. `UserModelGuide` render, by portrait size

`UserModel::render_within` — the candidate build, the `(tier, confidence,
recency)` eviction sort, and the greedy budget fill. Budget: 1 200 chars.

| Portrait | stored items | latency (min) | rendered chars |
|---|---:|---:|---:|
| empty | 0 | **25 ns** | 0 (nothing injected) |
| small | 3 | 2.6 µs | 433 |
| at the default `max_items_stored` (24/collection) | 51 | **9.4 µs** | 1 143 |
| oversized | 403 | 61.7 µs | 1 143 |
| absurd | 2 003 | **305 µs** | 1 143 |

Linear at ~152 ns per stored item, and the budget holds exactly — 1 143 chars
out of 1 200 at every size past the small case, with the tail evicted by tier.

The oversized rows are **synthetic**. `PortraitPolicy::max_items_stored`
defaults to 24 per collection and `UserModel::merge` prunes to it on every
merge, so a portrait that went through the normal write path cannot reach 403
items, let alone 2 003 — the bench had to raise the cap to build them. The row
that describes production is the 51-item one: **9.4 µs, once per session**,
since `UserModelGuide` caches the rendered string.

---

## Ranked verdict

### 1. `ExperienceGuide` + `MemoryGuide` on `FileMemory` — the only real problem

**Hurts from ~5 000 entries; painful past 20 000.**

| entries | `MemoryGuide` | `ExperienceGuide` | both, per model call |
|---:|---:|---:|---:|
| 1 000 | 0.8 ms | 1.1 ms | ~1.9 ms |
| 10 000 (interpolated) | ~8 ms | ~12 ms | ~20 ms |
| 50 000 | 41 ms | 58 ms | **~99 ms** |

And this is per **model iteration**, not per user message. A five-iteration
tool-using turn at 50 000 entries spends **half a second** re-reading two JSONL
files, most of it re-parsing the same rows it parsed 40 ms earlier.

It is linear, not quadratic — but linear with a full file read and a full JSON
re-parse per call is the wrong constant to multiply by "every turn". A busy
single user reaches 50 000 memory rows in about a year with `MemorySynthesizer`
writing three facts per session; a multi-tenant server sharing one file gets
there far sooner.

**Recommendations**, cheapest first:

1. **Cache the parsed file in `FileMemory`.** Keep the `Vec<MemoryEntry>` plus
   the file's `mtime`/`len`; re-read only when they change. Recall becomes a
   scan over an in-memory `Vec` — the ~0.8 µs/entry drops to the ~0.1 µs/entry
   of the scoring loop alone, a ~10× win for a few dozen lines, with no format
   change and no new dependency. This is the single highest-value fix here.
2. **Document a size ceiling** on `FileMemory` and have `MemoryGuide` log a
   warning above it, so the failure mode is a log line and not a mystery
   latency. The doc comment already says "kilobyte-scale"; nothing enforces it.
3. **Point production at `harness-recall-sqlite`-style indexed storage** past a
   few thousand rows. The infrastructure exists in the workspace; `Memory` just
   has no SQLite implementation yet.
4. **Do not run `MemoryGuide` and `ExperienceGuide` over the same backend
   unless you need both** — they are two independent full scans of two files
   for one turn.

### 2. `RecallGuide`'s CJK fallback — a bounded but sharp edge

**Hurts from ~20 000 messages.** Worst case 35 ms per turn at 50 000, from five
`LIKE '%…%'` full table scans, triggered by ordinary short Chinese questions
that happen to match nothing.

**Recommendations:**

1. **Stop emitting 2-character chunks**, or make the backend route them to the
   trigram index too. `CJK_CHUNK_SIZES` includes `2` for recall, but a 2-char
   chunk is below the backend's `count_cjk >= 3` trigram threshold, so it buys a
   full table scan and, per the code's own comment, "precision drops". Dropping
   `2` from the ladder removes the entire worst case.
2. **Budget the fallback in scans, not in probes.** `MAX_CJK_PROBES = 12` limits
   the wrong quantity — the measurements show a 13-probe query costing 170 µs
   and a 13-probe query costing 35 ms. A separate, much smaller ceiling on
   probes that can reach `LIKE` would bound the real cost.
3. **Index the `LIKE` path or drop it** for owner-scoped queries at scale.

Everything else about `RecallGuide` is fine: the hit path is one store call, the
injection is the smallest of the four (230–299 chars), and the guide is opt-in
by construction with a doc comment that already argues against enabling it.

### 3. `SkillDistiller::gate` — cheap where it was claimed to be, allocation-heavy where it wasn't

**Only matters past ~200 skills.** The rejection claim is verified and holds
completely: 2–81 ns, zero allocations, zero I/O, zero model calls. That is the
common path and it is free.

The accept/duplicate path is 2 ms and 1.78 MB at 500 skills. Once per episode,
not once per turn, so 2 ms is not an emergency — but 1.78 MB of allocator churn
for one boolean is avoidable:

1. **Tokenise the episode situation once**, outside the `map`, and pass the
   `HashSet` in. That alone removes roughly half the allocations.
2. **Pre-tokenise `ExistingSkill`** at construction (store the token set beside
   `name`/`description`) and the `format!` + second tokenisation go too.
3. If you rebuild the skills list per episode, note that
   **`existing_skills_in` at 500 skills (13.3 ms) is 6× more expensive than the
   gate it feeds**. Cache it; invalidate on write.

### 4. `UserModelGuide` — free. Genuinely.

**125–167 ns per turn, at every store size**, because it caches the rendered
block and `apply_before_iter` only strips and re-pushes it. The 1 200-char
budget holds exactly (1 143 chars rendered), the eviction sort is 9.4 µs on a
realistic portrait, and that runs **once per session**, not once per turn.

The one-time session-start load is a `FileMemory` read like everybody else's
(29 ms at 50 000 entries) and inherits fix #1 above for free. Nothing to do
here. The design note in `guide.rs` — that a portrait does not depend on the
current message, so re-querying per iteration would be pure I/O for an identical
result — is the reason this one is 200 000× cheaper than its neighbours, and it
is worth copying.

---

## Surprises

Two things the benchmark found that it was not looking for.

### `MemoryGuide` silently recalls nothing for natural Chinese queries

Against a corpus of Chinese memories, the natural-sentence query
`结算服务在支付回调重试的时候应该怎么处理幂等问题` rendered **0 characters** at
every store size — while still spending the full 73 ms scan at 50 000 entries.
The same corpus with a contiguous-substring query (`支付回调重试`) renders 398
chars as expected.

Cause, in `FileMemory::tokenise`:

```rust
s.to_lowercase()
    .split(|c: char| !c.is_alphanumeric())   // han chars ARE alphanumeric
    .filter(|t| t.len() >= 3)
```

Chinese has no spaces, so a space-free han sentence does not split — it becomes
**one token**, which is then `contains`-matched against entry content and
matches nothing. Every Chinese query that is a sentence rather than a term
recalls nothing, and does so at full price. `MemoryGuide::tokenise_for_score`
has the same shape.

This is precisely the problem `harness-tools-recall` solved for its own store
with `search_with_cjk_fallback` and documented at length. `FileMemory` never got
the same treatment. Given this project's zh-CN users, it is worth treating as a
correctness bug, not a tuning issue — the minimal fix is to chunk han runs in
`tokenise` the way `cjk_chunks` does.

### More store round-trips can be dramatically *cheaper*

The 13-probe CJK miss at 50 000 messages (170 µs) is 78× faster than the
1-probe ASCII hit (13.2 ms), and 200× faster than the *other* 13-probe CJK miss
(35 ms). Probe count is nearly uncorrelated with cost; what matters is whether a
probe matches (triggering five hydration queries per session) and whether it
falls through to a `LIKE` table scan. Any budget expressed in "number of store
calls" — which is what `MAX_CJK_PROBES` is — is bounding a quantity that does
not predict the bill.

---

## What could not be isolated

Stated plainly, since a benchmark that overclaims is worse than one that finds
nothing:

- **Guide cost vs. backend cost, for `UserModelGuide` and `RecallGuide`.**
  `MemoryGuide` and `ExperienceGuide` were run against an O(1) stub backend as
  well as `FileMemory`, so their own overhead is isolated (4.7 µs and 9.3 µs
  respectively). The other two were not: `UserModelStore` is a concrete struct
  rather than a trait, and `search_with_cjk_fallback` is `pub(crate)`, so
  neither could be driven without a real backend from outside the crate. For
  `UserModelGuide` this does not matter — the warm path is 167 ns and there is
  nothing left to attribute. For `RecallGuide` it means the numbers are
  "guide + SqliteRecall", and the guide's own share is not separately known
  (it is bounded above by the 82 µs 10-message case, so it is small).

- **Absolute I/O cost.** Every `FileMemory` measurement ran against a warm page
  cache on an SSD. A cold cache, a network filesystem, or a container with
  throttled I/O would all be worse, and by an amount this benchmark cannot
  predict. The numbers are a floor.

- **Real-world store composition.** Corpora are synthetic and uniform:
  same-length entries, one query shape per case. A real store has a long tail of
  very long entries (pasted files, tool output), which changes the per-entry
  constant but not the linearity.

- **`MemorySynthesizer` / `MemoryWriter` / `UserModelUpdater` /
  `SkillDistiller::distill`.** All model-backed and out of scope by design. Note
  that `MemoryWriter` and `MemorySynthesizer` `tokio::spawn` their writes, so
  they do not block a turn, but they do contend for the same `FileMemory` write
  lock the recall path reads through.

- **Concurrency.** Everything was measured single-threaded on an idle machine.
  `FileMemory` serialises writes behind a `Mutex` and reads the file
  unsynchronised; what N concurrent tenants do to the 41 ms figure is not
  measured here.
