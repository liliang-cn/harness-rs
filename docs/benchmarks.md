# Benchmarks

**The suite measures the harness, not the model.** `bench-suite` runs 10 tasks,
each with a machine verifier (a shell assertion the harness runs *outside* the
agent), so "resolved" is objective — not the model grading itself. Three
disciplines on top:

- **`pass^k`** — every task run `k` times, *all* must resolve. A reliability
  floor, not the `pass@k` capability ceiling. CI gates on it.
- **Leave-one-out guard ablation** — each row is the shipped default minus
  exactly one guard (`-stuck`, `-accept`, `-cap`, `-compact`, `-spill`,
  `+dedupe`), so a delta is attributable to one thing.
- **Trigger coverage** — every run counts how often each guard actually fired;
  a row whose guard never fired is flagged as noise instead of posing as a
  measurement. Four tasks exist purely to make guards fire.

Measured 2026-08-14, `gemini-3.6-flash-high` via an OpenAI-compat gateway, k=1
(so treat cost columns as indicative; pass columns are the stable signal):

| guard | fired | Δpass^k (on−off) | note |
|---|--:|--:|---|
| stuck detector | 1 | **+2** | without it, the retry-trap task burns 16 rounds and dies |
| compactor | 3 | **+1** | halved the trap task's input (93k → 48.5k tokens) |
| result ceiling | 1 | +1 | "no ceiling" is cheapest per pass — and loses a task |
| spill (vs truncate) | 1 | 0 | same pass rate at **−17% cost/pass** (k=2: 26.4k vs 31.9k eff. tokens) |
| dedupe | 5 | 0 | −12% cost on gemini; *lost a task* on qwen — guard verdicts are model-relative |
| acceptance | 0 on gemini | — | fired ×3 on local qwen3.5: it rescues weaker models |

Reproduce:

```sh
HARNESS_API_KEY=… HARNESS_BASE_URL=… HARNESS_MODEL=… \
BENCH_K=3 BENCH_LEVELS=H2,-stuck,-accept,-cap,-compact,-spill,+dedupe,H0 \
cargo run -p eval-bench --bin bench-suite     # exits non-zero unless H2 pass^k is clean
# local/slow models: BENCH_TIMEOUT_SECS=300
```

## Cost

Measured token cost on a fixed task set — `deepseek-v4-flash` via Aliyun MaaS,
2026-07-04. Every task finished (`Done`) with side effects verified (`sum.txt`
= 42, etc.). Reproduce any row with `harness run "<task>" --json`:

| task | iters | tool calls | in tok | out tok |
|---|--:|--:|--:|--:|
| list a directory | 2 | 1 | 975 | 103 |
| read a file, then answer | 2 | 1 | 992 | 130 |
| create a file | 2 | 1 | 1350 | 107 |
| read → sum numbers → write result | 3 | 2 | 2336 | 260 |

File writes go through the `write_file` **tool** (small structured args), not the
model re-emitting whole file bodies each turn — "don't burn tokens on what code
can do", measured rather than asserted. `cargo run -p eval-bench` emits the same
per-task cost fields for cross-framework comparison.

See also [per-turn-cost.md](per-turn-cost.md).
