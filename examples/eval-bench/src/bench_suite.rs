//! Task-completion benchmark — the `pass@1` runner.
//!
//! Where `eval-bench` (the other bin) measures *cost* on one task, this measures
//! whether the agent actually *solved* the task. Each task carries a machine
//! verifier (a shell assertion that exits 0 when the workspace end-state is
//! correct); the harness runs the verifier itself, outside the agent, so
//! "resolved" is an objective fact, not the model grading its own homework.
//!
//! This is the Rust-native, self-built task set: small, deterministic, no
//! network, no Docker. It exists to make "can it work autonomously?" a number
//! we can regress on. SWE-bench-lite (Python, per-instance containers via
//! `ContainerSandbox`) is the next phase and reuses this same runner shape.
//!
//! ## What it measures, and why these three
//!
//! **`pass^k`** — every task run `k` times, all `k` must resolve. Not `pass@k`,
//! which is the opposite: `pass@k` asks whether *any* attempt worked and is an
//! upper bound on capability; `pass^k` asks whether *every* attempt worked and
//! is a floor on reliability. The distinction is not academic — τ-bench reports
//! a model at 81.6% `pass^1` falling to 56.1% at `pass^4`. Making an agent
//! succeed *repeatably* is what a harness is for, so this is the number that
//! measures the harness rather than the model.
//!
//! **Leave-one-out guard ablation** — the same tasks, the same model, with the
//! loop's guards removed *one at a time* from the shipped default (`H2`). A
//! harness's contribution is otherwise inseparable from its model's: the same
//! model has been measured at 46% under one scaffold and 80% under another. The
//! earlier H0/H1/H2 ladder showed the scaffold matters but could not say which
//! guard; a `H2 − guard` row attributes the difference to exactly one thing.
//! Dedupe was measured off the default, so it runs the other way: `+dedupe`.
//!
//! **Trigger coverage** — every run counts how often each guard actually fired
//! (truncations, repeat suppressions, stuck nudges/aborts, acceptance
//! rejections, compactions). An ablation row whose guard never fired measures
//! nothing — the delta is noise, and the report says so instead of letting a
//! zero look like "this guard buys nothing". Four tasks exist purely to make
//! guards fire: a prompt that instructs the model to retry the same search
//! (stuck), a prompt that tempts a chat-only answer (acceptance), a grep whose
//! natural first move floods past the result ceiling (cap), and a multi-file
//! aggregation under a deliberately small declared context window (compactor).
//!
//! **Cost normalised by reliability** — `cost_of_pass` is the expected spend per
//! *correct* answer, not per run. A configuration that is cheap per attempt and
//! fails half the time is the expensive one. Tokens are weighted as GitHub's
//! Effective Tokens does (`1×in + 0.1×cached + 4×out`), so output — the thing
//! that actually costs — is not averaged away by a large prompt.
//!
//! ```sh
//! # full ablation, k=3 (the report's guard-attribution table needs H2 present)
//! BENCH_K=3 BENCH_LEVELS=H2,-stuck,-accept,-cap,-compact,+dedupe,H0 \
//!   cargo run -p eval-bench --bin bench-suite
//! ```
//!
//! ```sh
//! HARNESS_API_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! # or, matching the existing eval-bench:
//! DASHSCOPE_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! ```
use harness::prelude::*;
use harness_context::default_world;
use harness_core::compactor::{Budget, CompactionStage, Compactor};
use harness_core::error::CompactError;
use harness_core::{Context, Event, Hook, HookOutcome, Task};
use harness_loop::acceptance::FilesExist;
use harness_loop::{AgentLoop, Outcome, StuckPolicy, ToolResultPolicy};
use harness_models::OpenAiCompat;
use harness_tools_fs::{EditFile, Grep, ListDir, ReadFile, WriteFile};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One benchmark task: a prompt, the files it starts from, and a shell
/// assertion that decides — objectively — whether the end-state is correct.
struct BenchTask {
    id: &'static str,
    prompt: &'static str,
    /// (relative path, contents) written into a fresh workspace before the run.
    seed: &'static [(&'static str, &'static str)],
    /// `bash -c` snippet run in the workspace after the run. Exit 0 = resolved.
    verify: &'static str,
    /// Files [`FilesExist`] demands before the loop may say Done — wired only
    /// when the acceptance guard is on, so `-accept` measures its absence.
    accept_files: &'static [&'static str],
    /// Declared context window for this task's model (None = the model's real
    /// one). The compactor trap declares a small window so compaction *must*
    /// fire on a modest task instead of needing 150k tokens of filler.
    window: Option<u32>,
    /// Programmatic workspace seeding, for tasks whose files are too big to be
    /// literals. Runs after `seed`.
    setup: Option<fn(&std::path::Path)>,
}

/// The fields every ordinary task leaves at rest, so adding a knob for one
/// trap task doesn't rewrite the whole set.
const TASK_DEFAULTS: BenchTask = BenchTask {
    id: "",
    prompt: "",
    seed: &[],
    verify: "",
    accept_files: &[],
    window: None,
    setup: None,
};

/// ~1200 lines, one of them different — big enough that reading it whole is a
/// real cost and small enough to stay a unit test rather than a download.
const BIG_LOG: &str = concat!(
    "row 00000 value=0 status=ok padding-padding-padding\\nrow 00001 value=7 status=ok padding-padding-padding\\nrow 00002 value=14 status=ok padding-padding-padding\\nrow 00003 value=21 status=ok padding-padding-padding\\nrow 00004 value=28 status=ok padding-padding-padding\\nrow 00005 value=35 status=ok padding-padding-padding\\nrow 00006 value=42 status=ok padding-padding-padding\\nrow 00007 value=49 status=ok padding-padding-padding\\nrow 00008 value=56 status=ok padding-padding-padding\\nrow 00009 value=63 status=ok padding-padding-padding\\nrow 00010 value=70 status=ok padding-padding-padding\\nrow 00011 value=77 status=ok padding-padding-padding\\nrow 00012 value=84 status=ok padding-padding-padding\\nrow 00013 value=91 status=ok padding-padding-padding\\nrow 00014 value=98 status=ok padding-padding-padding\\nrow 00015 value=105 status=ok padding-padding-padding\\nrow 00016 value=112 status=ok padding-padding-padding\\nrow 00017 value=119 status=ok padding-padding-padding\\nrow 00018 value=126 status=ok padding-padding-padding\\nrow 00019 value=133 status=ok padding-padding-padding\\nrow 00020 value=140 status=ok padding-padding-padding\\nrow 00021 value=147 status=ok padding-padding-padding\\nrow 00022 value=154 status=ok padding-padding-padding\\nrow 00023 value=161 status=ok padding-padding-padding\\nrow 00024 value=168 status=ok padding-padding-padding\\nrow 00025 value=175 status=ok padding-padding-padding\\nrow 00026 value=182 status=ok padding-padding-padding\\nrow 00027 value=189 status=ok padding-padding-padding\\nrow 00028 value=196 status=ok padding-padding-padding\\nrow 00029 value=203 status=ok padding-padding-padding\\nrow 00030 value=210 status=ok padding-padding-padding\\nrow 00031 value=217 status=ok padding-padding-padding\\nrow 00032 value=224 status=ok padding-padding-padding\\nrow 00033 value=231 status=ok padding-padding-padding\\nrow 00034 value=238 status=ok padding-padding-padding\\nrow 00035 value=245 status=ok padding-padding-padding\\nrow 00036 value=252 status=ok padding-padding-padding\\nrow 00037 value=259 status=ok padding-padding-padding\\nrow 00038 value=266 status=ok padding-padding-padding\\nrow 00039 value=273 status=ok padding-padding-padding\\nrow 00040 value=280 status=ok padding-padding-padding\\nrow 00041 value=287 status=ok padding-padding-padding\\nrow 00042 value=294 status=ok padding-padding-padding\\nrow 00043 value=301 status=ok padding-padding-padding\\nrow 00044 value=308 status=ok padding-padding-padding\\nrow 00045 value=315 status=ok padding-padding-padding\\nrow 00046 value=322 status=ok padding-padding-padding\\nrow 00047 value=329 status=ok padding-padding-padding\\nrow 00048 value=336 status=ok padding-padding-padding\\nrow 00049 value=343 status=ok padding-padding-padding\\nrow 00050 value=350 status=ok padding-padding-padding\\nrow 00051 value=357 status=ok padding-padding-padding\\nrow 00052 value=364 status=ok padding-padding-padding\\nrow 00053 value=371 status=ok padding-padding-padding\\nrow 00054 value=378 status=ok padding-padding-padding\\nrow 00055 value=385 status=ok padding-padding-padding\\nrow 00056 value=392 status=ok padding-padding-padding\\nrow 00057 value=399 status=ok padding-padding-padding\\nrow 00058 value=406 status=ok padding-padding-padding\\nrow 00059 value=413 status=ok padding-padding-padding\\nrow 00060 value=420 status=ok padding-padding-padding\\nrow 00061 value=427 status=ok padding-padding-padding\\nrow 00062 value=434 status=ok padding-padding-padding\\nrow 00063 value=441 status=ok padding-padding-padding\\nrow 00064 value=448 status=ok padding-padding-padding\\nrow 00065 value=455 status=ok padding-padding-padding\\nrow 00066 value=462 status=ok padding-padding-padding\\nrow 00067 value=469 status=ok padding-padding-padding\\nrow 00068 value=476 status=ok padding-padding-padding\\nrow 00069 value=483 status=ok padding-padding-padding\\nrow 00070 value=490 status=ok padding-padding-padding\\nrow 00071 value=497 status=ok padding-padding-padding\\nrow 00072 value=504 status=ok padding-padding-padding\\nrow 00073 value=511 status=ok padding-padding-padding\\nrow 00074 value=518 status=ok padding-padding-padding\\nrow 00075 value=525 status=ok padding-padding-padding\\nrow 00076 value=532 status=ok padding-padding-padding\\nrow 00077 value=539 status=ok padding-padding-padding\\nrow 00078 value=546 status=ok padding-padding-padding\\nrow 00079 value=553 status=ok padding-padding-padding\\nrow 00080 value=560 status=ok padding-padding-padding\\nrow 00081 value=567 status=ok padding-padding-padding\\nrow 00082 value=574 status=ok padding-padding-padding\\nrow 00083 value=581 status=ok padding-padding-padding\\nrow 00084 value=588 status=ok padding-padding-padding\\nrow 00085 value=595 status=ok padding-padding-padding\\nrow 00086 value=602 status=ok padding-padding-padding\\nrow 00087 value=609 status=ok padding-padding-padding\\nrow 00088 value=616 status=ok padding-padding-padding\\nrow 00089 value=623 status=ok padding-padding-padding\\nrow 00090 value=630 status=ok padding-padding-padding\\nrow 00091 value=637 status=ok padding-padding-padding\\nrow 00092 value=644 status=ok padding-padding-padding\\nrow 00093 value=651 status=ok padding-padding-padding\\nrow 00094 value=658 status=ok padding-padding-padding\\nrow 00095 value=665 status=ok padding-padding-padding\\nrow 00096 value=672 status=ok padding-padding-padding\\nrow 00097 value=679 status=ok padding-padding-padding\\nrow 00098 value=686 status=ok padding-padding-padding\\nrow 00099 value=693 status=ok padding-padding-padding\\nrow 00100 value=700 status=ok padding-padding-padding\\nrow 00101 value=707 status=ok padding-padding-padding\\nrow 00102 value=714 status=ok padding-padding-padding\\nrow 00103 value=721 status=ok padding-padding-padding\\nrow 00104 value=728 status=ok padding-padding-padding\\nrow 00105 value=735 status=ok padding-padding-padding\\nrow 00106 value=742 status=ok padding-padding-padding\\nrow 00107 value=749 status=ok padding-padding-padding\\nrow 00108 value=756 status=ok padding-padding-padding\\nrow 00109 value=763 status=ok padding-padding-padding\\nrow 00110 value=770 status=ok padding-padding-padding\\nrow 00111 value=777 status=ok padding-padding-padding\\nrow 00112 value=784 status=ok padding-padding-padding\\nrow 00113 value=791 status=ok padding-padding-padding\\nrow 00114 value=798 status=ok padding-padding-padding\\nrow 00115 value=805 status=ok padding-padding-padding\\nrow 00116 value=812 status=ok padding-padding-padding\\nrow 00117 value=819 status=ok padding-padding-padding\\nrow 00118 value=826 status=ok padding-padding-padding\\nrow 00119 value=833 status=ok padding-padding-padding\\nrow 00120 value=840 status=ok padding-padding-padding\\nrow 00121 value=847 status=ok padding-padding-padding\\nrow 00122 value=854 status=ok padding-padding-padding\\nrow 00123 value=861 status=ok padding-padding-padding\\nrow 00124 value=868 status=ok padding-padding-padding\\nrow 00125 value=875 status=ok padding-padding-padding\\nrow 00126 value=882 status=ok padding-padding-padding\\nrow 00127 value=889 status=ok padding-padding-padding\\nrow 00128 value=896 status=ok padding-padding-padding\\nrow 00129 value=903 status=ok padding-padding-padding\\nrow 00130 value=910 status=ok padding-padding-padding\\nrow 00131 value=917 status=ok padding-padding-padding\\nrow 00132 value=924 status=ok padding-padding-padding\\nrow 00133 value=931 status=ok padding-padding-padding\\nrow 00134 value=938 status=ok padding-padding-padding\\nrow 00135 value=945 status=ok padding-padding-padding\\nrow 00136 value=952 status=ok padding-padding-padding\\nrow 00137 value=959 status=ok padding-padding-padding\\nrow 00138 value=966 status=ok padding-padding-padding\\nrow 00139 value=973 status=ok padding-padding-padding\\nrow 00140 value=980 status=ok padding-padding-padding\\nrow 00141 value=987 status=ok padding-padding-padding\\nrow 00142 value=994 status=ok padding-padding-padding\\nrow 00143 value=1001 status=ok padding-padding-padding\\nrow 00144 value=1008 status=ok padding-padding-padding\\nrow 00145 value=1015 status=ok padding-padding-padding\\nrow 00146 value=1022 status=ok padding-padding-padding\\nrow 00147 value=1029 status=ok padding-padding-padding\\nrow 00148 value=1036 status=ok padding-padding-padding\\nrow 00149 value=1043 status=ok padding-padding-padding\\nrow 00150 value=1050 status=FAILED padding-padding-padding\\nrow 00151 value=1057 status=ok padding-padding-padding\\nrow 00152 value=1064 status=ok padding-padding-padding\\nrow 00153 value=1071 status=ok padding-padding-padding\\nrow 00154 value=1078 status=ok padding-padding-padding\\nrow 00155 value=1085 status=ok padding-padding-padding\\nrow 00156 value=1092 status=ok padding-padding-padding\\nrow 00157 value=1099 status=ok padding-padding-padding\\nrow 00158 value=1106 status=ok padding-padding-padding\\nrow 00159 value=1113 status=ok padding-padding-padding\\nrow 00160 value=1120 status=ok padding-padding-padding\\nrow 00161 value=1127 status=ok padding-padding-padding\\nrow 00162 value=1134 status=ok padding-padding-padding\\nrow 00163 value=1141 status=ok padding-padding-padding\\nrow 00164 value=1148 status=ok padding-padding-padding\\nrow 00165 value=1155 status=ok padding-padding-padding\\nrow 00166 value=1162 status=ok padding-padding-padding\\nrow 00167 value=1169 status=ok padding-padding-padding\\nrow 00168 value=1176 status=ok padding-padding-padding\\nrow 00169 value=1183 status=ok padding-padding-padding\\nrow 00170 value=1190 status=ok padding-padding-padding\\nrow 00171 value=1197 status=ok padding-padding-padding\\nrow 00172 value=1204 status=ok padding-padding-padding\\nrow 00173 value=1211 status=ok padding-padding-padding\\nrow 00174 value=1218 status=ok padding-padding-padding\\nrow 00175 value=1225 status=ok padding-padding-padding\\nrow 00176 value=1232 status=ok padding-padding-padding\\nrow 00177 value=1239 status=ok padding-padding-padding\\nrow 00178 value=1246 status=ok padding-padding-padding\\nrow 00179 value=1253 status=ok padding-padding-padding\\nrow 00180 value=1260 status=ok padding-padding-padding\\nrow 00181 value=1267 status=ok padding-padding-padding\\nrow 00182 value=1274 status=ok padding-padding-padding\\nrow 00183 value=1281 status=ok padding-padding-padding\\nrow 00184 value=1288 status=ok padding-padding-padding\\nrow 00185 value=1295 status=ok padding-padding-padding\\nrow 00186 value=1302 status=ok padding-padding-padding\\nrow 00187 value=1309 status=ok padding-padding-padding\\nrow 00188 value=1316 status=ok padding-padding-padding\\nrow 00189 value=1323 status=ok padding-padding-padding\\nrow 00190 value=1330 status=ok padding-padding-padding\\nrow 00191 value=1337 status=ok padding-padding-padding\\nrow 00192 value=1344 status=ok padding-padding-padding\\nrow 00193 value=1351 status=ok padding-padding-padding\\nrow 00194 value=1358 status=ok padding-padding-padding\\nrow 00195 value=1365 status=ok padding-padding-padding\\nrow 00196 value=1372 status=ok padding-padding-padding\\nrow 00197 value=1379 status=ok padding-padding-padding\\nrow 00198 value=1386 status=ok padding-padding-padding\\nrow 00199 value=1393 status=ok padding-padding-padding\\nrow 00200 value=1400 status=ok padding-padding-padding\\nrow 00201 value=1407 status=ok padding-padding-padding\\nrow 00202 value=1414 status=ok padding-padding-padding\\nrow 00203 value=1421 status=ok padding-padding-padding\\nrow 00204 value=1428 status=ok padding-padding-padding\\nrow 00205 value=1435 status=ok padding-padding-padding\\nrow 00206 value=1442 status=ok padding-padding-padding\\nrow 00207 value=1449 status=ok padding-padding-padding\\nrow 00208 value=1456 status=ok padding-padding-padding\\nrow 00209 value=1463 status=ok padding-padding-padding\\nrow 00210 value=1470 status=ok padding-padding-padding\\nrow 00211 value=1477 status=ok padding-padding-padding\\nrow 00212 value=1484 status=ok padding-padding-padding\\nrow 00213 value=1491 status=ok padding-padding-padding\\nrow 00214 value=1498 status=ok padding-padding-padding\\nrow 00215 value=1505 status=ok padding-padding-padding\\nrow 00216 value=1512 status=ok padding-padding-padding\\nrow 00217 value=1519 status=ok padding-padding-padding\\nrow 00218 value=1526 status=ok padding-padding-padding\\nrow 00219 value=1533 status=ok padding-padding-padding\\nrow 00220 value=1540 status=ok padding-padding-padding\\nrow 00221 value=1547 status=ok padding-padding-padding\\nrow 00222 value=1554 status=ok padding-padding-padding\\nrow 00223 value=1561 status=ok padding-padding-padding\\nrow 00224 value=1568 status=ok padding-padding-padding\\nrow 00225 value=1575 status=ok padding-padding-padding\\nrow 00226 value=1582 status=ok padding-padding-padding\\nrow 00227 value=1589 status=ok padding-padding-padding\\nrow 00228 value=1596 status=ok padding-padding-padding\\nrow 00229 value=1603 status=ok padding-padding-padding\\nrow 00230 value=1610 status=ok padding-padding-padding\\nrow 00231 value=1617 status=ok padding-padding-padding\\nrow 00232 value=1624 status=ok padding-padding-padding\\nrow 00233 value=1631 status=ok padding-padding-padding\\nrow 00234 value=1638 status=ok padding-padding-padding\\nrow 00235 value=1645 status=ok padding-padding-padding\\nrow 00236 value=1652 status=ok padding-padding-padding\\nrow 00237 value=1659 status=ok padding-padding-padding\\nrow 00238 value=1666 status=ok padding-padding-padding\\nrow 00239 value=1673 status=ok padding-padding-padding\\nrow 00240 value=1680 status=ok padding-padding-padding\\nrow 00241 value=1687 status=ok padding-padding-padding\\nrow 00242 value=1694 status=ok padding-padding-padding\\nrow 00243 value=1701 status=ok padding-padding-padding\\nrow 00244 value=1708 status=ok padding-padding-padding\\nrow 00245 value=1715 status=ok padding-padding-padding\\nrow 00246 value=1722 status=ok padding-padding-padding\\nrow 00247 value=1729 status=ok padding-padding-padding\\nrow 00248 value=1736 status=ok padding-padding-padding\\nrow 00249 value=1743 status=ok padding-padding-padding\\nrow 00250 value=1750 status=ok padding-padding-padding\\nrow 00251 value=1757 status=ok padding-padding-padding\\nrow 00252 value=1764 status=ok padding-padding-padding\\nrow 00253 value=1771 status=ok padding-padding-padding\\nrow 00254 value=1778 status=ok padding-padding-padding\\nrow 00255 value=1785 status=ok padding-padding-padding\\nrow 00256 value=1792 status=ok padding-padding-padding\\nrow 00257 value=1799 status=ok padding-padding-padding\\nrow 00258 value=1806 status=ok padding-padding-padding\\nrow 00259 value=1813 status=ok padding-padding-padding\\nrow 00260 value=1820 status=ok padding-padding-padding\\nrow 00261 value=1827 status=ok padding-padding-padding\\nrow 00262 value=1834 status=ok padding-padding-padding\\nrow 00263 value=1841 status=ok padding-padding-padding\\nrow 00264 value=1848 status=ok padding-padding-padding\\nrow 00265 value=1855 status=ok padding-padding-padding\\nrow 00266 value=1862 status=ok padding-padding-padding\\nrow 00267 value=1869 status=ok padding-padding-padding\\nrow 00268 value=1876 status=ok padding-padding-padding\\nrow 00269 value=1883 status=ok padding-padding-padding\\nrow 00270 value=1890 status=ok padding-padding-padding\\nrow 00271 value=1897 status=ok padding-padding-padding\\nrow 00272 value=1904 status=ok padding-padding-padding\\nrow 00273 value=1911 status=ok padding-padding-padding\\nrow 00274 value=1918 status=ok padding-padding-padding\\nrow 00275 value=1925 status=ok padding-padding-padding\\nrow 00276 value=1932 status=ok padding-padding-padding\\nrow 00277 value=1939 status=ok padding-padding-padding\\nrow 00278 value=1946 status=ok padding-padding-padding\\nrow 00279 value=1953 status=ok padding-padding-padding\\nrow 00280 value=1960 status=ok padding-padding-padding\\nrow 00281 value=1967 status=ok padding-padding-padding\\nrow 00282 value=1974 status=ok padding-padding-padding\\nrow 00283 value=1981 status=ok padding-padding-padding\\nrow 00284 value=1988 status=ok padding-padding-padding\\nrow 00285 value=1995 status=ok padding-padding-padding\\nrow 00286 value=2002 status=ok padding-padding-padding\\nrow 00287 value=2009 status=ok padding-padding-padding\\nrow 00288 value=2016 status=ok padding-padding-padding\\nrow 00289 value=2023 status=ok padding-padding-padding\\nrow 00290 value=2030 status=ok padding-padding-padding\\nrow 00291 value=2037 status=ok padding-padding-padding\\nrow 00292 value=2044 status=ok padding-padding-padding\\nrow 00293 value=2051 status=ok padding-padding-padding\\nrow 00294 value=2058 status=ok padding-padding-padding\\nrow 00295 value=2065 status=ok padding-padding-padding\\nrow 00296 value=2072 status=ok padding-padding-padding\\nrow 00297 value=2079 status=ok padding-padding-padding\\nrow 00298 value=2086 status=ok padding-padding-padding\\nrow 00299 value=2093 status=ok padding-padding-padding\\nrow 00300 value=2100 status=ok padding-padding-padding\\nrow 00301 value=2107 status=ok padding-padding-padding\\nrow 00302 value=2114 status=ok padding-padding-padding\\nrow 00303 value=2121 status=ok padding-padding-padding\\nrow 00304 value=2128 status=ok padding-padding-padding\\nrow 00305 value=2135 status=ok padding-padding-padding\\nrow 00306 value=2142 status=ok padding-padding-padding\\nrow 00307 value=2149 status=ok padding-padding-padding\\nrow 00308 value=2156 status=ok padding-padding-padding\\nrow 00309 value=2163 status=ok padding-padding-padding\\nrow 00310 value=2170 status=ok padding-padding-padding\\nrow 00311 value=2177 status=ok padding-padding-padding\\nrow 00312 value=2184 status=ok padding-padding-padding\\nrow 00313 value=2191 status=ok padding-padding-padding\\nrow 00314 value=2198 status=ok padding-padding-padding\\nrow 00315 value=2205 status=ok padding-padding-padding\\nrow 00316 value=2212 status=ok padding-padding-padding\\nrow 00317 value=2219 status=ok padding-padding-padding\\nrow 00318 value=2226 status=ok padding-padding-padding\\nrow 00319 value=2233 status=ok padding-padding-padding\\nrow 00320 value=2240 status=ok padding-padding-padding\\nrow 00321 value=2247 status=ok padding-padding-padding\\nrow 00322 value=2254 status=ok padding-padding-padding\\nrow 00323 value=2261 status=ok padding-padding-padding\\nrow 00324 value=2268 status=ok padding-padding-padding\\nrow 00325 value=2275 status=ok padding-padding-padding\\nrow 00326 value=2282 status=ok padding-padding-padding\\nrow 00327 value=2289 status=ok padding-padding-padding\\nrow 00328 value=2296 status=ok padding-padding-padding\\nrow 00329 value=2303 status=ok padding-padding-padding\\nrow 00330 value=2310 status=ok padding-padding-padding\\nrow 00331 value=2317 status=ok padding-padding-padding\\nrow 00332 value=2324 status=ok padding-padding-padding\\nrow 00333 value=2331 status=ok padding-padding-padding\\nrow 00334 value=2338 status=ok padding-padding-padding\\nrow 00335 value=2345 status=ok padding-padding-padding\\nrow 00336 value=2352 status=ok padding-padding-padding\\nrow 00337 value=2359 status=ok padding-padding-padding\\nrow 00338 value=2366 status=ok padding-padding-padding\\nrow 00339 value=2373 status=ok padding-padding-padding\\nrow 00340 value=2380 status=ok padding-padding-padding\\nrow 00341 value=2387 status=ok padding-padding-padding\\nrow 00342 value=2394 status=ok padding-padding-padding\\nrow 00343 value=2401 status=ok padding-padding-padding\\nrow 00344 value=2408 status=ok padding-padding-padding\\nrow 00345 value=2415 status=ok padding-padding-padding\\nrow 00346 value=2422 status=ok padding-padding-padding\\nrow 00347 value=2429 status=ok padding-padding-padding\\nrow 00348 value=2436 status=ok padding-padding-padding\\nrow 00349 value=2443 status=ok padding-padding-padding\\nrow 00350 value=2450 status=ok padding-padding-padding\\nrow 00351 value=2457 status=ok padding-padding-padding\\nrow 00352 value=2464 status=ok padding-padding-padding\\nrow 00353 value=2471 status=ok padding-padding-padding\\nrow 00354 value=2478 status=ok padding-padding-padding\\nrow 00355 value=2485 status=ok padding-padding-padding\\nrow 00356 value=2492 status=ok padding-padding-padding\\nrow 00357 value=2499 status=ok padding-padding-padding\\nrow 00358 value=2506 status=ok padding-padding-padding\\nrow 00359 value=2513 status=ok padding-padding-padding\\nrow 00360 value=2520 status=ok padding-padding-padding\\nrow 00361 value=2527 status=ok padding-padding-padding\\nrow 00362 value=2534 status=ok padding-padding-padding\\nrow 00363 value=2541 status=ok padding-padding-padding\\nrow 00364 value=2548 status=ok padding-padding-padding\\nrow 00365 value=2555 status=ok padding-padding-padding\\nrow 00366 value=2562 status=ok padding-padding-padding\\nrow 00367 value=2569 status=ok padding-padding-padding\\nrow 00368 value=2576 status=ok padding-padding-padding\\nrow 00369 value=2583 status=ok padding-padding-padding\\nrow 00370 value=2590 status=ok padding-padding-padding\\nrow 00371 value=2597 status=ok padding-padding-padding\\nrow 00372 value=2604 status=ok padding-padding-padding\\nrow 00373 value=2611 status=ok padding-padding-padding\\nrow 00374 value=2618 status=ok padding-padding-padding\\nrow 00375 value=2625 status=ok padding-padding-padding\\nrow 00376 value=2632 status=ok padding-padding-padding\\nrow 00377 value=2639 status=ok padding-padding-padding\\nrow 00378 value=2646 status=ok padding-padding-padding\\nrow 00379 value=2653 status=ok padding-padding-padding\\nrow 00380 value=2660 status=ok padding-padding-padding\\nrow 00381 value=2667 status=ok padding-padding-padding\\nrow 00382 value=2674 status=ok padding-padding-padding\\nrow 00383 value=2681 status=ok padding-padding-padding\\nrow 00384 value=2688 status=ok padding-padding-padding\\nrow 00385 value=2695 status=ok padding-padding-padding\\nrow 00386 value=2702 status=ok padding-padding-padding\\nrow 00387 value=2709 status=ok padding-padding-padding\\nrow 00388 value=2716 status=ok padding-padding-padding\\nrow 00389 value=2723 status=ok padding-padding-padding\\nrow 00390 value=2730 status=ok padding-padding-padding\\nrow 00391 value=2737 status=ok padding-padding-padding\\nrow 00392 value=2744 status=ok padding-padding-padding\\nrow 00393 value=2751 status=ok padding-padding-padding\\nrow 00394 value=2758 status=ok padding-padding-padding\\nrow 00395 value=2765 status=ok padding-padding-padding\\nrow 00396 value=2772 status=ok padding-padding-padding\\nrow 00397 value=2779 status=ok padding-padding-padding\\nrow 00398 value=2786 status=ok padding-padding-padding\\nrow 00399 value=2793 status=ok padding-padding-padding\\nrow 00400 value=2800 status=ok padding-padding-padding\\nrow 00401 value=2807 status=ok padding-padding-padding\\nrow 00402 value=2814 status=ok padding-padding-padding\\nrow 00403 value=2821 status=ok padding-padding-padding\\nrow 00404 value=2828 status=ok padding-padding-padding\\nrow 00405 value=2835 status=ok padding-padding-padding\\nrow 00406 value=2842 status=ok padding-padding-padding\\nrow 00407 value=2849 status=ok padding-padding-padding\\nrow 00408 value=2856 status=ok padding-padding-padding\\nrow 00409 value=2863 status=ok padding-padding-padding\\nrow 00410 value=2870 status=ok padding-padding-padding\\nrow 00411 value=2877 status=ok padding-padding-padding\\nrow 00412 value=2884 status=ok padding-padding-padding\\nrow 00413 value=2891 status=ok padding-padding-padding\\nrow 00414 value=2898 status=ok padding-padding-padding\\nrow 00415 value=2905 status=ok padding-padding-padding\\nrow 00416 value=2912 status=ok padding-padding-padding\\nrow 00417 value=2919 status=ok padding-padding-padding\\nrow 00418 value=2926 status=ok padding-padding-padding\\nrow 00419 value=2933 status=ok padding-padding-padding\\nrow 00420 value=2940 status=ok padding-padding-padding\\nrow 00421 value=2947 status=ok padding-padding-padding\\nrow 00422 value=2954 status=ok padding-padding-padding\\nrow 00423 value=2961 status=ok padding-padding-padding\\nrow 00424 value=2968 status=ok padding-padding-padding\\nrow 00425 value=2975 status=ok padding-padding-padding\\nrow 00426 value=2982 status=ok padding-padding-padding\\nrow 00427 value=2989 status=ok padding-padding-padding\\nrow 00428 value=2996 status=ok padding-padding-padding\\nrow 00429 value=3003 status=ok padding-padding-padding\\nrow 00430 value=3010 status=ok padding-padding-padding\\nrow 00431 value=3017 status=ok padding-padding-padding\\nrow 00432 value=3024 status=ok padding-padding-padding\\nrow 00433 value=3031 status=ok padding-padding-padding\\nrow 00434 value=3038 status=ok padding-padding-padding\\nrow 00435 value=3045 status=ok padding-padding-padding\\nrow 00436 value=3052 status=ok padding-padding-padding\\nrow 00437 value=3059 status=ok padding-padding-padding\\nrow 00438 value=3066 status=ok padding-padding-padding\\nrow 00439 value=3073 status=ok padding-padding-padding\\nrow 00440 value=3080 status=ok padding-padding-padding\\nrow 00441 value=3087 status=ok padding-padding-padding\\nrow 00442 value=3094 status=ok padding-padding-padding\\nrow 00443 value=3101 status=ok padding-padding-padding\\nrow 00444 value=3108 status=ok padding-padding-padding\\nrow 00445 value=3115 status=ok padding-padding-padding\\nrow 00446 value=3122 status=ok padding-padding-padding\\nrow 00447 value=3129 status=ok padding-padding-padding\\nrow 00448 value=3136 status=ok padding-padding-padding\\nrow 00449 value=3143 status=ok padding-padding-padding\\nrow 00450 value=3150 status=ok padding-padding-padding\\nrow 00451 value=3157 status=ok padding-padding-padding\\nrow 00452 value=3164 status=ok padding-padding-padding\\nrow 00453 value=3171 status=ok padding-padding-padding\\nrow 00454 value=3178 status=ok padding-padding-padding\\nrow 00455 value=3185 status=ok padding-padding-padding\\nrow 00456 value=3192 status=ok padding-padding-padding\\nrow 00457 value=3199 status=ok padding-padding-padding\\nrow 00458 value=3206 status=ok padding-padding-padding\\nrow 00459 value=3213 status=ok padding-padding-padding\\nrow 00460 value=3220 status=ok padding-padding-padding\\nrow 00461 value=3227 status=ok padding-padding-padding\\nrow 00462 value=3234 status=ok padding-padding-padding\\nrow 00463 value=3241 status=ok padding-padding-padding\\nrow 00464 value=3248 status=ok padding-padding-padding\\nrow 00465 value=3255 status=ok padding-padding-padding\\nrow 00466 value=3262 status=ok padding-padding-padding\\nrow 00467 value=3269 status=ok padding-padding-padding\\nrow 00468 value=3276 status=ok padding-padding-padding\\nrow 00469 value=3283 status=ok padding-padding-padding\\nrow 00470 value=3290 status=ok padding-padding-padding\\nrow 00471 value=3297 status=ok padding-padding-padding\\nrow 00472 value=3304 status=ok padding-padding-padding\\nrow 00473 value=3311 status=ok padding-padding-padding\\nrow 00474 value=3318 status=ok padding-padding-padding\\nrow 00475 value=3325 status=ok padding-padding-padding\\nrow 00476 value=3332 status=ok padding-padding-padding\\nrow 00477 value=3339 status=ok padding-padding-padding\\nrow 00478 value=3346 status=ok padding-padding-padding\\nrow 00479 value=3353 status=ok padding-padding-padding\\nrow 00480 value=3360 status=ok padding-padding-padding\\nrow 00481 value=3367 status=ok padding-padding-padding\\nrow 00482 value=3374 status=ok padding-padding-padding\\nrow 00483 value=3381 status=ok padding-padding-padding\\nrow 00484 value=3388 status=ok padding-padding-padding\\nrow 00485 value=3395 status=ok padding-padding-padding\\nrow 00486 value=3402 status=ok padding-padding-padding\\nrow 00487 value=3409 status=ok padding-padding-padding\\nrow 00488 value=3416 status=ok padding-padding-padding\\nrow 00489 value=3423 status=ok padding-padding-padding\\nrow 00490 value=3430 status=ok padding-padding-padding\\nrow 00491 value=3437 status=ok padding-padding-padding\\nrow 00492 value=3444 status=ok padding-padding-padding\\nrow 00493 value=3451 status=ok padding-padding-padding\\nrow 00494 value=3458 status=ok padding-padding-padding\\nrow 00495 value=3465 status=ok padding-padding-padding\\nrow 00496 value=3472 status=ok padding-padding-padding\\nrow 00497 value=3479 status=ok padding-padding-padding\\nrow 00498 value=3486 status=ok padding-padding-padding\\nrow 00499 value=3493 status=ok padding-padding-padding\\nrow 00500 value=3500 status=ok padding-padding-padding\\nrow 00501 value=3507 status=ok padding-padding-padding\\nrow 00502 value=3514 status=ok padding-padding-padding\\nrow 00503 value=3521 status=ok padding-padding-padding\\nrow 00504 value=3528 status=ok padding-padding-padding\\nrow 00505 value=3535 status=ok padding-padding-padding\\nrow 00506 value=3542 status=ok padding-padding-padding\\nrow 00507 value=3549 status=ok padding-padding-padding\\nrow 00508 value=3556 status=ok padding-padding-padding\\nrow 00509 value=3563 status=ok padding-padding-padding\\nrow 00510 value=3570 status=ok padding-padding-padding\\nrow 00511 value=3577 status=ok padding-padding-padding\\nrow 00512 value=3584 status=ok padding-padding-padding\\nrow 00513 value=3591 status=ok padding-padding-padding\\nrow 00514 value=3598 status=ok padding-padding-padding\\nrow 00515 value=3605 status=ok padding-padding-padding\\nrow 00516 value=3612 status=ok padding-padding-padding\\nrow 00517 value=3619 status=ok padding-padding-padding\\nrow 00518 value=3626 status=ok padding-padding-padding\\nrow 00519 value=3633 status=ok padding-padding-padding\\nrow 00520 value=3640 status=ok padding-padding-padding\\nrow 00521 value=3647 status=ok padding-padding-padding\\nrow 00522 value=3654 status=ok padding-padding-padding\\nrow 00523 value=3661 status=ok padding-padding-padding\\nrow 00524 value=3668 status=ok padding-padding-padding\\nrow 00525 value=3675 status=ok padding-padding-padding\\nrow 00526 value=3682 status=ok padding-padding-padding\\nrow 00527 value=3689 status=ok padding-padding-padding\\nrow 00528 value=3696 status=ok padding-padding-padding\\nrow 00529 value=3703 status=ok padding-padding-padding\\nrow 00530 value=3710 status=ok padding-padding-padding\\nrow 00531 value=3717 status=ok padding-padding-padding\\nrow 00532 value=3724 status=ok padding-padding-padding\\nrow 00533 value=3731 status=ok padding-padding-padding\\nrow 00534 value=3738 status=ok padding-padding-padding\\nrow 00535 value=3745 status=ok padding-padding-padding\\nrow 00536 value=3752 status=ok padding-padding-padding\\nrow 00537 value=3759 status=ok padding-padding-padding\\nrow 00538 value=3766 status=ok padding-padding-padding\\nrow 00539 value=3773 status=ok padding-padding-padding\\nrow 00540 value=3780 status=ok padding-padding-padding\\nrow 00541 value=3787 status=ok padding-padding-padding\\nrow 00542 value=3794 status=ok padding-padding-padding\\nrow 00543 value=3801 status=ok padding-padding-padding\\nrow 00544 value=3808 status=ok padding-padding-padding\\nrow 00545 value=3815 status=ok padding-padding-padding\\nrow 00546 value=3822 status=ok padding-padding-padding\\nrow 00547 value=3829 status=ok padding-padding-padding\\nrow 00548 value=3836 status=ok padding-padding-padding\\nrow 00549 value=3843 status=ok padding-padding-padding\\nrow 00550 value=3850 status=ok padding-padding-padding\\nrow 00551 value=3857 status=ok padding-padding-padding\\nrow 00552 value=3864 status=ok padding-padding-padding\\nrow 00553 value=3871 status=ok padding-padding-padding\\nrow 00554 value=3878 status=ok padding-padding-padding\\nrow 00555 value=3885 status=ok padding-padding-padding\\nrow 00556 value=3892 status=ok padding-padding-padding\\nrow 00557 value=3899 status=ok padding-padding-padding\\nrow 00558 value=3906 status=ok padding-padding-padding\\nrow 00559 value=3913 status=ok padding-padding-padding\\nrow 00560 value=3920 status=ok padding-padding-padding\\nrow 00561 value=3927 status=ok padding-padding-padding\\nrow 00562 value=3934 status=ok padding-padding-padding\\nrow 00563 value=3941 status=ok padding-padding-padding\\nrow 00564 value=3948 status=ok padding-padding-padding\\nrow 00565 value=3955 status=ok padding-padding-padding\\nrow 00566 value=3962 status=ok padding-padding-padding\\nrow 00567 value=3969 status=ok padding-padding-padding\\nrow 00568 value=3976 status=ok padding-padding-padding\\nrow 00569 value=3983 status=ok padding-padding-padding\\nrow 00570 value=3990 status=ok padding-padding-padding\\nrow 00571 value=3997 status=ok padding-padding-padding\\nrow 00572 value=4004 status=ok padding-padding-padding\\nrow 00573 value=4011 status=ok padding-padding-padding\\nrow 00574 value=4018 status=ok padding-padding-padding\\nrow 00575 value=4025 status=ok padding-padding-padding\\nrow 00576 value=4032 status=ok padding-padding-padding\\nrow 00577 value=4039 status=ok padding-padding-padding\\nrow 00578 value=4046 status=ok padding-padding-padding\\nrow 00579 value=4053 status=ok padding-padding-padding\\nrow 00580 value=4060 status=ok padding-padding-padding\\nrow 00581 value=4067 status=ok padding-padding-padding\\nrow 00582 value=4074 status=ok padding-padding-padding\\nrow 00583 value=4081 status=ok padding-padding-padding\\nrow 00584 value=4088 status=ok padding-padding-padding\\nrow 00585 value=4095 status=ok padding-padding-padding\\nrow 00586 value=4102 status=ok padding-padding-padding\\nrow 00587 value=4109 status=ok padding-padding-padding\\nrow 00588 value=4116 status=ok padding-padding-padding\\nrow 00589 value=4123 status=ok padding-padding-padding\\nrow 00590 value=4130 status=ok padding-padding-padding\\nrow 00591 value=4137 status=ok padding-padding-padding\\nrow 00592 value=4144 status=ok padding-padding-padding\\nrow 00593 value=4151 status=ok padding-padding-padding\\nrow 00594 value=4158 status=ok padding-padding-padding\\nrow 00595 value=4165 status=ok padding-padding-padding\\nrow 00596 value=4172 status=ok padding-padding-padding\\nrow 00597 value=4179 status=ok padding-padding-padding\\nrow 00598 value=4186 status=ok padding-padding-padding\\nrow 00599 value=4193 status=ok padding-padding-padding\\nrow 00600 value=4200 status=ok padding-padding-padding\\nrow 00601 value=4207 status=ok padding-padding-padding\\nrow 00602 value=4214 status=ok padding-padding-padding\\nrow 00603 value=4221 status=ok padding-padding-padding\\nrow 00604 value=4228 status=ok padding-padding-padding\\nrow 00605 value=4235 status=ok padding-padding-padding\\nrow 00606 value=4242 status=ok padding-padding-padding\\nrow 00607 value=4249 status=ok padding-padding-padding\\nrow 00608 value=4256 status=ok padding-padding-padding\\nrow 00609 value=4263 status=ok padding-padding-padding\\nrow 00610 value=4270 status=ok padding-padding-padding\\nrow 00611 value=4277 status=ok padding-padding-padding\\nrow 00612 value=4284 status=ok padding-padding-padding\\nrow 00613 value=4291 status=ok padding-padding-padding\\nrow 00614 value=4298 status=ok padding-padding-padding\\nrow 00615 value=4305 status=ok padding-padding-padding\\nrow 00616 value=4312 status=ok padding-padding-padding\\nrow 00617 value=4319 status=ok padding-padding-padding\\nrow 00618 value=4326 status=ok padding-padding-padding\\nrow 00619 value=4333 status=ok padding-padding-padding\\nrow 00620 value=4340 status=ok padding-padding-padding\\nrow 00621 value=4347 status=ok padding-padding-padding\\nrow 00622 value=4354 status=ok padding-padding-padding\\nrow 00623 value=4361 status=ok padding-padding-padding\\nrow 00624 value=4368 status=ok padding-padding-padding\\nrow 00625 value=4375 status=ok padding-padding-padding\\nrow 00626 value=4382 status=ok padding-padding-padding\\nrow 00627 value=4389 status=ok padding-padding-padding\\nrow 00628 value=4396 status=ok padding-padding-padding\\nrow 00629 value=4403 status=ok padding-padding-padding\\nrow 00630 value=4410 status=ok padding-padding-padding\\nrow 00631 value=4417 status=ok padding-padding-padding\\nrow 00632 value=4424 status=ok padding-padding-padding\\nrow 00633 value=4431 status=ok padding-padding-padding\\nrow 00634 value=4438 status=ok padding-padding-padding\\nrow 00635 value=4445 status=ok padding-padding-padding\\nrow 00636 value=4452 status=ok padding-padding-padding\\nrow 00637 value=4459 status=ok padding-padding-padding\\nrow 00638 value=4466 status=ok padding-padding-padding\\nrow 00639 value=4473 status=ok padding-padding-padding\\nrow 00640 value=4480 status=ok padding-padding-padding\\nrow 00641 value=4487 status=ok padding-padding-padding\\nrow 00642 value=4494 status=ok padding-padding-padding\\nrow 00643 value=4501 status=ok padding-padding-padding\\nrow 00644 value=4508 status=ok padding-padding-padding\\nrow 00645 value=4515 status=ok padding-padding-padding\\nrow 00646 value=4522 status=ok padding-padding-padding\\nrow 00647 value=4529 status=ok padding-padding-padding\\nrow 00648 value=4536 status=ok padding-padding-padding\\nrow 00649 value=4543 status=ok padding-padding-padding\\nrow 00650 value=4550 status=ok padding-padding-padding\\nrow 00651 value=4557 status=ok padding-padding-padding\\nrow 00652 value=4564 status=ok padding-padding-padding\\nrow 00653 value=4571 status=ok padding-padding-padding\\nrow 00654 value=4578 status=ok padding-padding-padding\\nrow 00655 value=4585 status=ok padding-padding-padding\\nrow 00656 value=4592 status=ok padding-padding-padding\\nrow 00657 value=4599 status=ok padding-padding-padding\\nrow 00658 value=4606 status=ok padding-padding-padding\\nrow 00659 value=4613 status=ok padding-padding-padding\\nrow 00660 value=4620 status=ok padding-padding-padding\\nrow 00661 value=4627 status=ok padding-padding-padding\\nrow 00662 value=4634 status=ok padding-padding-padding\\nrow 00663 value=4641 status=ok padding-padding-padding\\nrow 00664 value=4648 status=ok padding-padding-padding\\nrow 00665 value=4655 status=ok padding-padding-padding\\nrow 00666 value=4662 status=ok padding-padding-padding\\nrow 00667 value=4669 status=ok padding-padding-padding\\nrow 00668 value=4676 status=ok padding-padding-padding\\nrow 00669 value=4683 status=ok padding-padding-padding\\nrow 00670 value=4690 status=ok padding-padding-padding\\nrow 00671 value=4697 status=ok padding-padding-padding\\nrow 00672 value=4704 status=ok padding-padding-padding\\nrow 00673 value=4711 status=ok padding-padding-padding\\nrow 00674 value=4718 status=ok padding-padding-padding\\nrow 00675 value=4725 status=ok padding-padding-padding\\nrow 00676 value=4732 status=ok padding-padding-padding\\nrow 00677 value=4739 status=ok padding-padding-padding\\nrow 00678 value=4746 status=ok padding-padding-padding\\nrow 00679 value=4753 status=ok padding-padding-padding\\nrow 00680 value=4760 status=ok padding-padding-padding\\nrow 00681 value=4767 status=ok padding-padding-padding\\nrow 00682 value=4774 status=ok padding-padding-padding\\nrow 00683 value=4781 status=ok padding-padding-padding\\nrow 00684 value=4788 status=ok padding-padding-padding\\nrow 00685 value=4795 status=ok padding-padding-padding\\nrow 00686 value=4802 status=ok padding-padding-padding\\nrow 00687 value=4809 status=ok padding-padding-padding\\nrow 00688 value=4816 status=ok padding-padding-padding\\nrow 00689 value=4823 status=ok padding-padding-padding\\nrow 00690 value=4830 status=ok padding-padding-padding\\nrow 00691 value=4837 status=ok padding-padding-padding\\nrow 00692 value=4844 status=ok padding-padding-padding\\nrow 00693 value=4851 status=ok padding-padding-padding\\nrow 00694 value=4858 status=ok padding-padding-padding\\nrow 00695 value=4865 status=ok padding-padding-padding\\nrow 00696 value=4872 status=ok padding-padding-padding\\nrow 00697 value=4879 status=ok padding-padding-padding\\nrow 00698 value=4886 status=ok padding-padding-padding\\nrow 00699 value=4893 status=ok padding-padding-padding\\nrow 00700 value=4900 status=ok padding-padding-padding\\nrow 00701 value=4907 status=ok padding-padding-padding\\nrow 00702 value=4914 status=ok padding-padding-padding\\nrow 00703 value=4921 status=ok padding-padding-padding\\nrow 00704 value=4928 status=ok padding-padding-padding\\nrow 00705 value=4935 status=ok padding-padding-padding\\nrow 00706 value=4942 status=ok padding-padding-padding\\nrow 00707 value=4949 status=ok padding-padding-padding\\nrow 00708 value=4956 status=ok padding-padding-padding\\nrow 00709 value=4963 status=ok padding-padding-padding\\nrow 00710 value=4970 status=ok padding-padding-padding\\nrow 00711 value=4977 status=ok padding-padding-padding\\nrow 00712 value=4984 status=ok padding-padding-padding\\nrow 00713 value=4991 status=ok padding-padding-padding\\nrow 00714 value=4998 status=ok padding-padding-padding\\nrow 00715 value=5005 status=ok padding-padding-padding\\nrow 00716 value=5012 status=ok padding-padding-padding\\nrow 00717 value=5019 status=ok padding-padding-padding\\nrow 00718 value=5026 status=ok padding-padding-padding\\nrow 00719 value=5033 status=ok padding-padding-padding\\nrow 00720 value=5040 status=ok padding-padding-padding\\nrow 00721 value=5047 status=ok padding-padding-padding\\nrow 00722 value=5054 status=ok padding-padding-padding\\nrow 00723 value=5061 status=ok padding-padding-padding\\nrow 00724 value=5068 status=ok padding-padding-padding\\nrow 00725 value=5075 status=ok padding-padding-padding\\nrow 00726 value=5082 status=ok padding-padding-padding\\nrow 00727 value=5089 status=ok padding-padding-padding\\nrow 00728 value=5096 status=ok padding-padding-padding\\nrow 00729 value=5103 status=ok padding-padding-padding\\nrow 00730 value=5110 status=ok padding-padding-padding\\nrow 00731 value=5117 status=ok padding-padding-padding\\nrow 00732 value=5124 status=ok padding-padding-padding\\nrow 00733 value=5131 status=ok padding-padding-padding\\nrow 00734 value=5138 status=ok padding-padding-padding\\nrow 00735 value=5145 status=ok padding-padding-padding\\nrow 00736 value=5152 status=ok padding-padding-padding\\nrow 00737 value=5159 status=ok padding-padding-padding\\nrow 00738 value=5166 status=ok padding-padding-padding\\nrow 00739 value=5173 status=ok padding-padding-padding\\nrow 00740 value=5180 status=ok padding-padding-padding\\nrow 00741 value=5187 status=ok padding-padding-padding\\nrow 00742 value=5194 status=ok padding-padding-padding\\nrow 00743 value=5201 status=ok padding-padding-padding\\nrow 00744 value=5208 status=ok padding-padding-padding\\nrow 00745 value=5215 status=ok padding-padding-padding\\nrow 00746 value=5222 status=ok padding-padding-padding\\nrow 00747 value=5229 status=ok padding-padding-padding\\nrow 00748 value=5236 status=ok padding-padding-padding\\nrow 00749 value=5243 status=ok padding-padding-padding\\nrow 00750 value=5250 status=ok padding-padding-padding\\nrow 00751 value=5257 status=ok padding-padding-padding\\nrow 00752 value=5264 status=ok padding-padding-padding\\nrow 00753 value=5271 status=ok padding-padding-padding\\nrow 00754 value=5278 status=ok padding-padding-padding\\nrow 00755 value=5285 status=ok padding-padding-padding\\nrow 00756 value=5292 status=ok padding-padding-padding\\nrow 00757 value=5299 status=ok padding-padding-padding\\nrow 00758 value=5306 status=ok padding-padding-padding\\nrow 00759 value=5313 status=ok padding-padding-padding\\nrow 00760 value=5320 status=ok padding-padding-padding\\nrow 00761 value=5327 status=ok padding-padding-padding\\nrow 00762 value=5334 status=ok padding-padding-padding\\nrow 00763 value=5341 status=ok padding-padding-padding\\nrow 00764 value=5348 status=ok padding-padding-padding\\nrow 00765 value=5355 status=ok padding-padding-padding\\nrow 00766 value=5362 status=ok padding-padding-padding\\nrow 00767 value=5369 status=ok padding-padding-padding\\nrow 00768 value=5376 status=ok padding-padding-padding\\nrow 00769 value=5383 status=ok padding-padding-padding\\nrow 00770 value=5390 status=ok padding-padding-padding\\nrow 00771 value=5397 status=ok padding-padding-padding\\nrow 00772 value=5404 status=ok padding-padding-padding\\nrow 00773 value=5411 status=ok padding-padding-padding\\nrow 00774 value=5418 status=ok padding-padding-padding\\nrow 00775 value=5425 status=ok padding-padding-padding\\nrow 00776 value=5432 status=ok padding-padding-padding\\nrow 00777 value=5439 status=ok padding-padding-padding\\nrow 00778 value=5446 status=ok padding-padding-padding\\nrow 00779 value=5453 status=ok padding-padding-padding\\nrow 00780 value=5460 status=ok padding-padding-padding\\nrow 00781 value=5467 status=ok padding-padding-padding\\nrow 00782 value=5474 status=ok padding-padding-padding\\nrow 00783 value=5481 status=ok padding-padding-padding\\nrow 00784 value=5488 status=ok padding-padding-padding\\nrow 00785 value=5495 status=ok padding-padding-padding\\nrow 00786 value=5502 status=ok padding-padding-padding\\nrow 00787 value=5509 status=ok padding-padding-padding\\nrow 00788 value=5516 status=ok padding-padding-padding\\nrow 00789 value=5523 status=ok padding-padding-padding\\nrow 00790 value=5530 status=ok padding-padding-padding\\nrow 00791 value=5537 status=ok padding-padding-padding\\nrow 00792 value=5544 status=ok padding-padding-padding\\nrow 00793 value=5551 status=ok padding-padding-padding\\nrow 00794 value=5558 status=ok padding-padding-padding\\nrow 00795 value=5565 status=ok padding-padding-padding\\nrow 00796 value=5572 status=ok padding-padding-padding\\nrow 00797 value=5579 status=ok padding-padding-padding\\nrow 00798 value=5586 status=ok padding-padding-padding\\nrow 00799 value=5593 status=ok padding-padding-padding\\nrow 00800 value=5600 status=ok padding-padding-padding\\nrow 00801 value=5607 status=ok padding-padding-padding\\nrow 00802 value=5614 status=ok padding-padding-padding\\nrow 00803 value=5621 status=ok padding-padding-padding\\nrow 00804 value=5628 status=ok padding-padding-padding\\nrow 00805 value=5635 status=ok padding-padding-padding\\nrow 00806 value=5642 status=ok padding-padding-padding\\nrow 00807 value=5649 status=ok padding-padding-padding\\nrow 00808 value=5656 status=ok padding-padding-padding\\nrow 00809 value=5663 status=ok padding-padding-padding\\nrow 00810 value=5670 status=ok padding-padding-padding\\nrow 00811 value=5677 status=ok padding-padding-padding\\nrow 00812 value=5684 status=ok padding-padding-padding\\nrow 00813 value=5691 status=ok padding-padding-padding\\nrow 00814 value=5698 status=ok padding-padding-padding\\nrow 00815 value=5705 status=ok padding-padding-padding\\nrow 00816 value=5712 status=ok padding-padding-padding\\nrow 00817 value=5719 status=ok padding-padding-padding\\nrow 00818 value=5726 status=ok padding-padding-padding\\nrow 00819 value=5733 status=ok padding-padding-padding\\nrow 00820 value=5740 status=ok padding-padding-padding\\nrow 00821 value=5747 status=ok padding-padding-padding\\nrow 00822 value=5754 status=ok padding-padding-padding\\nrow 00823 value=5761 status=ok padding-padding-padding\\nrow 00824 value=5768 status=ok padding-padding-padding\\nrow 00825 value=5775 status=ok padding-padding-padding\\nrow 00826 value=5782 status=ok padding-padding-padding\\nrow 00827 value=5789 status=ok padding-padding-padding\\nrow 00828 value=5796 status=ok padding-padding-padding\\nrow 00829 value=5803 status=ok padding-padding-padding\\nrow 00830 value=5810 status=ok padding-padding-padding\\nrow 00831 value=5817 status=ok padding-padding-padding\\nrow 00832 value=5824 status=ok padding-padding-padding\\nrow 00833 value=5831 status=ok padding-padding-padding\\nrow 00834 value=5838 status=ok padding-padding-padding\\nrow 00835 value=5845 status=ok padding-padding-padding\\nrow 00836 value=5852 status=ok padding-padding-padding\\nrow 00837 value=5859 status=ok padding-padding-padding\\nrow 00838 value=5866 status=ok padding-padding-padding\\nrow 00839 value=5873 status=ok padding-padding-padding\\nrow 00840 value=5880 status=ok padding-padding-padding\\nrow 00841 value=5887 status=ok padding-padding-padding\\nrow 00842 value=5894 status=ok padding-padding-padding\\nrow 00843 value=5901 status=ok padding-padding-padding\\nrow 00844 value=5908 status=ok padding-padding-padding\\nrow 00845 value=5915 status=ok padding-padding-padding\\nrow 00846 value=5922 status=ok padding-padding-padding\\nrow 00847 value=5929 status=ok padding-padding-padding\\nrow 00848 value=5936 status=ok padding-padding-padding\\nrow 00849 value=5943 status=ok padding-padding-padding\\nrow 00850 value=5950 status=ok padding-padding-padding\\nrow 00851 value=5957 status=ok padding-padding-padding\\nrow 00852 value=5964 status=ok padding-padding-padding\\nrow 00853 value=5971 status=ok padding-padding-padding\\nrow 00854 value=5978 status=ok padding-padding-padding\\nrow 00855 value=5985 status=ok padding-padding-padding\\nrow 00856 value=5992 status=ok padding-padding-padding\\nrow 00857 value=5999 status=ok padding-padding-padding\\nrow 00858 value=6006 status=ok padding-padding-padding\\nrow 00859 value=6013 status=ok padding-padding-padding\\nrow 00860 value=6020 status=ok padding-padding-padding\\nrow 00861 value=6027 status=ok padding-padding-padding\\nrow 00862 value=6034 status=ok padding-padding-padding\\nrow 00863 value=6041 status=ok padding-padding-padding\\nrow 00864 value=6048 status=ok padding-padding-padding\\nrow 00865 value=6055 status=ok padding-padding-padding\\nrow 00866 value=6062 status=ok padding-padding-padding\\nrow 00867 value=6069 status=ok padding-padding-padding\\nrow 00868 value=6076 status=ok padding-padding-padding\\nrow 00869 value=6083 status=ok padding-padding-padding\\nrow 00870 value=6090 status=ok padding-padding-padding\\nrow 00871 value=6097 status=ok padding-padding-padding\\nrow 00872 value=6104 status=ok padding-padding-padding\\nrow 00873 value=6111 status=ok padding-padding-padding\\nrow 00874 value=6118 status=ok padding-padding-padding\\nrow 00875 value=6125 status=ok padding-padding-padding\\nrow 00876 value=6132 status=ok padding-padding-padding\\nrow 00877 value=6139 status=ok padding-padding-padding\\nrow 00878 value=6146 status=ok padding-padding-padding\\nrow 00879 value=6153 status=ok padding-padding-padding\\nrow 00880 value=6160 status=ok padding-padding-padding\\nrow 00881 value=6167 status=ok padding-padding-padding\\nrow 00882 value=6174 status=ok padding-padding-padding\\nrow 00883 value=6181 status=ok padding-padding-padding\\nrow 00884 value=6188 status=ok padding-padding-padding\\nrow 00885 value=6195 status=ok padding-padding-padding\\nrow 00886 value=6202 status=ok padding-padding-padding\\nrow 00887 value=6209 status=ok padding-padding-padding\\nrow 00888 value=6216 status=ok padding-padding-padding\\nrow 00889 value=6223 status=ok padding-padding-padding\\nrow 00890 value=6230 status=ok padding-padding-padding\\nrow 00891 value=6237 status=ok padding-padding-padding\\nrow 00892 value=6244 status=ok padding-padding-padding\\nrow 00893 value=6251 status=ok padding-padding-padding\\nrow 00894 value=6258 status=ok padding-padding-padding\\nrow 00895 value=6265 status=ok padding-padding-padding\\nrow 00896 value=6272 status=ok padding-padding-padding\\nrow 00897 value=6279 status=ok padding-padding-padding\\nrow 00898 value=6286 status=ok padding-padding-padding\\nrow 00899 value=6293 status=ok padding-padding-padding\\nrow 00900 value=6300 status=ok padding-padding-padding\\nrow 00901 value=6307 status=ok padding-padding-padding\\nrow 00902 value=6314 status=ok padding-padding-padding\\nrow 00903 value=6321 status=ok padding-padding-padding\\nrow 00904 value=6328 status=ok padding-padding-padding\\nrow 00905 value=6335 status=ok padding-padding-padding\\nrow 00906 value=6342 status=ok padding-padding-padding\\nrow 00907 value=6349 status=ok padding-padding-padding\\nrow 00908 value=6356 status=ok padding-padding-padding\\nrow 00909 value=6363 status=ok padding-padding-padding\\nrow 00910 value=6370 status=ok padding-padding-padding\\nrow 00911 value=6377 status=ok padding-padding-padding\\nrow 00912 value=6384 status=ok padding-padding-padding\\nrow 00913 value=6391 status=ok padding-padding-padding\\nrow 00914 value=6398 status=ok padding-padding-padding\\nrow 00915 value=6405 status=ok padding-padding-padding\\nrow 00916 value=6412 status=ok padding-padding-padding\\nrow 00917 value=6419 status=ok padding-padding-padding\\nrow 00918 value=6426 status=ok padding-padding-padding\\nrow 00919 value=6433 status=ok padding-padding-padding\\nrow 00920 value=6440 status=ok padding-padding-padding\\nrow 00921 value=6447 status=ok padding-padding-padding\\nrow 00922 value=6454 status=ok padding-padding-padding\\nrow 00923 value=6461 status=ok padding-padding-padding\\nrow 00924 value=6468 status=ok padding-padding-padding\\nrow 00925 value=6475 status=ok padding-padding-padding\\nrow 00926 value=6482 status=ok padding-padding-padding\\nrow 00927 value=6489 status=ok padding-padding-padding\\nrow 00928 value=6496 status=ok padding-padding-padding\\nrow 00929 value=6503 status=ok padding-padding-padding\\nrow 00930 value=6510 status=ok padding-padding-padding\\nrow 00931 value=6517 status=ok padding-padding-padding\\nrow 00932 value=6524 status=ok padding-padding-padding\\nrow 00933 value=6531 status=ok padding-padding-padding\\nrow 00934 value=6538 status=ok padding-padding-padding\\nrow 00935 value=6545 status=ok padding-padding-padding\\nrow 00936 value=6552 status=ok padding-padding-padding\\nrow 00937 value=6559 status=ok padding-padding-padding\\nrow 00938 value=6566 status=ok padding-padding-padding\\nrow 00939 value=6573 status=ok padding-padding-padding\\nrow 00940 value=6580 status=ok padding-padding-padding\\nrow 00941 value=6587 status=ok padding-padding-padding\\nrow 00942 value=6594 status=ok padding-padding-padding\\nrow 00943 value=6601 status=ok padding-padding-padding\\nrow 00944 value=6608 status=ok padding-padding-padding\\nrow 00945 value=6615 status=ok padding-padding-padding\\nrow 00946 value=6622 status=ok padding-padding-padding\\nrow 00947 value=6629 status=ok padding-padding-padding\\nrow 00948 value=6636 status=ok padding-padding-padding\\nrow 00949 value=6643 status=ok padding-padding-padding\\nrow 00950 value=6650 status=ok padding-padding-padding\\nrow 00951 value=6657 status=ok padding-padding-padding\\nrow 00952 value=6664 status=ok padding-padding-padding\\nrow 00953 value=6671 status=ok padding-padding-padding\\nrow 00954 value=6678 status=ok padding-padding-padding\\nrow 00955 value=6685 status=ok padding-padding-padding\\nrow 00956 value=6692 status=ok padding-padding-padding\\nrow 00957 value=6699 status=ok padding-padding-padding\\nrow 00958 value=6706 status=ok padding-padding-padding\\nrow 00959 value=6713 status=ok padding-padding-padding\\nrow 00960 value=6720 status=ok padding-padding-padding\\nrow 00961 value=6727 status=ok padding-padding-padding\\nrow 00962 value=6734 status=ok padding-padding-padding\\nrow 00963 value=6741 status=ok padding-padding-padding\\nrow 00964 value=6748 status=ok padding-padding-padding\\nrow 00965 value=6755 status=ok padding-padding-padding\\nrow 00966 value=6762 status=ok padding-padding-padding\\nrow 00967 value=6769 status=ok padding-padding-padding\\nrow 00968 value=6776 status=ok padding-padding-padding\\nrow 00969 value=6783 status=ok padding-padding-padding\\nrow 00970 value=6790 status=ok padding-padding-padding\\nrow 00971 value=6797 status=ok padding-padding-padding\\nrow 00972 value=6804 status=ok padding-padding-padding\\nrow 00973 value=6811 status=ok padding-padding-padding\\nrow 00974 value=6818 status=ok padding-padding-padding\\nrow 00975 value=6825 status=ok padding-padding-padding\\nrow 00976 value=6832 status=ok padding-padding-padding\\nrow 00977 value=6839 status=ok padding-padding-padding\\nrow 00978 value=6846 status=ok padding-padding-padding\\nrow 00979 value=6853 status=ok padding-padding-padding\\nrow 00980 value=6860 status=ok padding-padding-padding\\nrow 00981 value=6867 status=ok padding-padding-padding\\nrow 00982 value=6874 status=ok padding-padding-padding\\nrow 00983 value=6881 status=ok padding-padding-padding\\nrow 00984 value=6888 status=ok padding-padding-padding\\nrow 00985 value=6895 status=ok padding-padding-padding\\nrow 00986 value=6902 status=ok padding-padding-padding\\nrow 00987 value=6909 status=ok padding-padding-padding\\nrow 00988 value=6916 status=ok padding-padding-padding\\nrow 00989 value=6923 status=ok padding-padding-padding\\nrow 00990 value=6930 status=ok padding-padding-padding\\nrow 00991 value=6937 status=ok padding-padding-padding\\nrow 00992 value=6944 status=ok padding-padding-padding\\nrow 00993 value=6951 status=ok padding-padding-padding\\nrow 00994 value=6958 status=ok padding-padding-padding\\nrow 00995 value=6965 status=ok padding-padding-padding\\nrow 00996 value=6972 status=ok padding-padding-padding\\nrow 00997 value=6979 status=ok padding-padding-padding\\nrow 00998 value=6986 status=ok padding-padding-padding\\nrow 00999 value=6993 status=ok padding-padding-padding\\nrow 01000 value=7000 status=ok padding-padding-padding\\nrow 01001 value=7007 status=ok padding-padding-padding\\nrow 01002 value=7014 status=ok padding-padding-padding\\nrow 01003 value=7021 status=ok padding-padding-padding\\nrow 01004 value=7028 status=ok padding-padding-padding\\nrow 01005 value=7035 status=ok padding-padding-padding\\nrow 01006 value=7042 status=ok padding-padding-padding\\nrow 01007 value=7049 status=ok padding-padding-padding\\nrow 01008 value=7056 status=ok padding-padding-padding\\nrow 01009 value=7063 status=ok padding-padding-padding\\nrow 01010 value=7070 status=ok padding-padding-padding\\nrow 01011 value=7077 status=ok padding-padding-padding\\nrow 01012 value=7084 status=ok padding-padding-padding\\nrow 01013 value=7091 status=ok padding-padding-padding\\nrow 01014 value=7098 status=ok padding-padding-padding\\nrow 01015 value=7105 status=ok padding-padding-padding\\nrow 01016 value=7112 status=ok padding-padding-padding\\nrow 01017 value=7119 status=ok padding-padding-padding\\nrow 01018 value=7126 status=ok padding-padding-padding\\nrow 01019 value=7133 status=ok padding-padding-padding\\nrow 01020 value=7140 status=ok padding-padding-padding\\nrow 01021 value=7147 status=ok padding-padding-padding\\nrow 01022 value=7154 status=ok padding-padding-padding\\nrow 01023 value=7161 status=ok padding-padding-padding\\nrow 01024 value=7168 status=ok padding-padding-padding\\nrow 01025 value=7175 status=ok padding-padding-padding\\nrow 01026 value=7182 status=ok padding-padding-padding\\nrow 01027 value=7189 status=ok padding-padding-padding\\nrow 01028 value=7196 status=ok padding-padding-padding\\nrow 01029 value=7203 status=ok padding-padding-padding\\nrow 01030 value=7210 status=ok padding-padding-padding\\nrow 01031 value=7217 status=ok padding-padding-padding\\nrow 01032 value=7224 status=ok padding-padding-padding\\nrow 01033 value=7231 status=ok padding-padding-padding\\nrow 01034 value=7238 status=ok padding-padding-padding\\nrow 01035 value=7245 status=ok padding-padding-padding\\nrow 01036 value=7252 status=ok padding-padding-padding\\nrow 01037 value=7259 status=ok padding-padding-padding\\nrow 01038 value=7266 status=ok padding-padding-padding\\nrow 01039 value=7273 status=ok padding-padding-padding\\nrow 01040 value=7280 status=ok padding-padding-padding\\nrow 01041 value=7287 status=ok padding-padding-padding\\nrow 01042 value=7294 status=ok padding-padding-padding\\nrow 01043 value=7301 status=ok padding-padding-padding\\nrow 01044 value=7308 status=ok padding-padding-padding\\nrow 01045 value=7315 status=ok padding-padding-padding\\nrow 01046 value=7322 status=ok padding-padding-padding\\nrow 01047 value=7329 status=ok padding-padding-padding\\nrow 01048 value=7336 status=ok padding-padding-padding\\nrow 01049 value=7343 status=ok padding-padding-padding\\nrow 01050 value=7350 status=ok padding-padding-padding\\nrow 01051 value=7357 status=ok padding-padding-padding\\nrow 01052 value=7364 status=ok padding-padding-padding\\nrow 01053 value=7371 status=ok padding-padding-padding\\nrow 01054 value=7378 status=ok padding-padding-padding\\nrow 01055 value=7385 status=ok padding-padding-padding\\nrow 01056 value=7392 status=ok padding-padding-padding\\nrow 01057 value=7399 status=ok padding-padding-padding\\nrow 01058 value=7406 status=ok padding-padding-padding\\nrow 01059 value=7413 status=ok padding-padding-padding\\nrow 01060 value=7420 status=ok padding-padding-padding\\nrow 01061 value=7427 status=ok padding-padding-padding\\nrow 01062 value=7434 status=ok padding-padding-padding\\nrow 01063 value=7441 status=ok padding-padding-padding\\nrow 01064 value=7448 status=ok padding-padding-padding\\nrow 01065 value=7455 status=ok padding-padding-padding\\nrow 01066 value=7462 status=ok padding-padding-padding\\nrow 01067 value=7469 status=ok padding-padding-padding\\nrow 01068 value=7476 status=ok padding-padding-padding\\nrow 01069 value=7483 status=ok padding-padding-padding\\nrow 01070 value=7490 status=ok padding-padding-padding\\nrow 01071 value=7497 status=ok padding-padding-padding\\nrow 01072 value=7504 status=ok padding-padding-padding\\nrow 01073 value=7511 status=ok padding-padding-padding\\nrow 01074 value=7518 status=ok padding-padding-padding\\nrow 01075 value=7525 status=ok padding-padding-padding\\nrow 01076 value=7532 status=ok padding-padding-padding\\nrow 01077 value=7539 status=ok padding-padding-padding\\nrow 01078 value=7546 status=ok padding-padding-padding\\nrow 01079 value=7553 status=ok padding-padding-padding\\nrow 01080 value=7560 status=ok padding-padding-padding\\nrow 01081 value=7567 status=ok padding-padding-padding\\nrow 01082 value=7574 status=ok padding-padding-padding\\nrow 01083 value=7581 status=ok padding-padding-padding\\nrow 01084 value=7588 status=ok padding-padding-padding\\nrow 01085 value=7595 status=ok padding-padding-padding\\nrow 01086 value=7602 status=ok padding-padding-padding\\nrow 01087 value=7609 status=ok padding-padding-padding\\nrow 01088 value=7616 status=ok padding-padding-padding\\nrow 01089 value=7623 status=ok padding-padding-padding\\nrow 01090 value=7630 status=ok padding-padding-padding\\nrow 01091 value=7637 status=ok padding-padding-padding\\nrow 01092 value=7644 status=ok padding-padding-padding\\nrow 01093 value=7651 status=ok padding-padding-padding\\nrow 01094 value=7658 status=ok padding-padding-padding\\nrow 01095 value=7665 status=ok padding-padding-padding\\nrow 01096 value=7672 status=ok padding-padding-padding\\nrow 01097 value=7679 status=ok padding-padding-padding\\nrow 01098 value=7686 status=ok padding-padding-padding\\nrow 01099 value=7693 status=ok padding-padding-padding\\nrow 01100 value=7700 status=ok padding-padding-padding\\nrow 01101 value=7707 status=ok padding-padding-padding\\nrow 01102 value=7714 status=ok padding-padding-padding\\nrow 01103 value=7721 status=ok padding-padding-padding\\nrow 01104 value=7728 status=ok padding-padding-padding\\nrow 01105 value=7735 status=ok padding-padding-padding\\nrow 01106 value=7742 status=ok padding-padding-padding\\nrow 01107 value=7749 status=ok padding-padding-padding\\nrow 01108 value=7756 status=ok padding-padding-padding\\nrow 01109 value=7763 status=ok padding-padding-padding\\nrow 01110 value=7770 status=ok padding-padding-padding\\nrow 01111 value=7777 status=ok padding-padding-padding\\nrow 01112 value=7784 status=ok padding-padding-padding\\nrow 01113 value=7791 status=ok padding-padding-padding\\nrow 01114 value=7798 status=ok padding-padding-padding\\nrow 01115 value=7805 status=ok padding-padding-padding\\nrow 01116 value=7812 status=ok padding-padding-padding\\nrow 01117 value=7819 status=ok padding-padding-padding\\nrow 01118 value=7826 status=ok padding-padding-padding\\nrow 01119 value=7833 status=ok padding-padding-padding\\nrow 01120 value=7840 status=ok padding-padding-padding\\nrow 01121 value=7847 status=ok padding-padding-padding\\nrow 01122 value=7854 status=ok padding-padding-padding\\nrow 01123 value=7861 status=ok padding-padding-padding\\nrow 01124 value=7868 status=ok padding-padding-padding\\nrow 01125 value=7875 status=ok padding-padding-padding\\nrow 01126 value=7882 status=ok padding-padding-padding\\nrow 01127 value=7889 status=ok padding-padding-padding\\nrow 01128 value=7896 status=ok padding-padding-padding\\nrow 01129 value=7903 status=ok padding-padding-padding\\nrow 01130 value=7910 status=ok padding-padding-padding\\nrow 01131 value=7917 status=ok padding-padding-padding\\nrow 01132 value=7924 status=ok padding-padding-padding\\nrow 01133 value=7931 status=ok padding-padding-padding\\nrow 01134 value=7938 status=ok padding-padding-padding\\nrow 01135 value=7945 status=ok padding-padding-padding\\nrow 01136 value=7952 status=ok padding-padding-padding\\nrow 01137 value=7959 status=ok padding-padding-padding\\nrow 01138 value=7966 status=ok padding-padding-padding\\nrow 01139 value=7973 status=ok padding-padding-padding\\nrow 01140 value=7980 status=ok padding-padding-padding\\nrow 01141 value=7987 status=ok padding-padding-padding\\nrow 01142 value=7994 status=ok padding-padding-padding\\nrow 01143 value=8001 status=ok padding-padding-padding\\nrow 01144 value=8008 status=ok padding-padding-padding\\nrow 01145 value=8015 status=ok padding-padding-padding\\nrow 01146 value=8022 status=ok padding-padding-padding\\nrow 01147 value=8029 status=ok padding-padding-padding\\nrow 01148 value=8036 status=ok padding-padding-padding\\nrow 01149 value=8043 status=ok padding-padding-padding\\nrow 01150 value=8050 status=ok padding-padding-padding\\nrow 01151 value=8057 status=ok padding-padding-padding\\nrow 01152 value=8064 status=ok padding-padding-padding\\nrow 01153 value=8071 status=ok padding-padding-padding\\nrow 01154 value=8078 status=ok padding-padding-padding\\nrow 01155 value=8085 status=ok padding-padding-padding\\nrow 01156 value=8092 status=ok padding-padding-padding\\nrow 01157 value=8099 status=ok padding-padding-padding\\nrow 01158 value=8106 status=ok padding-padding-padding\\nrow 01159 value=8113 status=ok padding-padding-padding\\nrow 01160 value=8120 status=ok padding-padding-padding\\nrow 01161 value=8127 status=ok padding-padding-padding\\nrow 01162 value=8134 status=ok padding-padding-padding\\nrow 01163 value=8141 status=ok padding-padding-padding\\nrow 01164 value=8148 status=ok padding-padding-padding\\nrow 01165 value=8155 status=ok padding-padding-padding\\nrow 01166 value=8162 status=ok padding-padding-padding\\nrow 01167 value=8169 status=ok padding-padding-padding\\nrow 01168 value=8176 status=ok padding-padding-padding\\nrow 01169 value=8183 status=ok padding-padding-padding\\nrow 01170 value=8190 status=ok padding-padding-padding\\nrow 01171 value=8197 status=ok padding-padding-padding\\nrow 01172 value=8204 status=ok padding-padding-padding\\nrow 01173 value=8211 status=ok padding-padding-padding\\nrow 01174 value=8218 status=ok padding-padding-padding\\nrow 01175 value=8225 status=ok padding-padding-padding\\nrow 01176 value=8232 status=ok padding-padding-padding\\nrow 01177 value=8239 status=ok padding-padding-padding\\nrow 01178 value=8246 status=ok padding-padding-padding\\nrow 01179 value=8253 status=ok padding-padding-padding\\nrow 01180 value=8260 status=ok padding-padding-padding\\nrow 01181 value=8267 status=ok padding-padding-padding\\nrow 01182 value=8274 status=ok padding-padding-padding\\nrow 01183 value=8281 status=ok padding-padding-padding\\nrow 01184 value=8288 status=ok padding-padding-padding\\nrow 01185 value=8295 status=ok padding-padding-padding\\nrow 01186 value=8302 status=ok padding-padding-padding\\nrow 01187 value=8309 status=ok padding-padding-padding\\nrow 01188 value=8316 status=ok padding-padding-padding\\nrow 01189 value=8323 status=ok padding-padding-padding\\nrow 01190 value=8330 status=ok padding-padding-padding\\nrow 01191 value=8337 status=ok padding-padding-padding\\nrow 01192 value=8344 status=ok padding-padding-padding\\nrow 01193 value=8351 status=ok padding-padding-padding\\nrow 01194 value=8358 status=ok padding-padding-padding\\nrow 01195 value=8365 status=ok padding-padding-padding\\nrow 01196 value=8372 status=ok padding-padding-padding\\nrow 01197 value=8379 status=ok padding-padding-padding\\nrow 01198 value=8386 status=ok padding-padding-padding\\nrow 01199 value=8393 status=ok padding-padding-padding",
    "\n"
);

/// The self-built Rust-native task set. Every task exercises the fs tools
/// (read/write/edit/list/grep) and has a deterministic, network-free verifier.
const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "sum-file",
        prompt: "Read the file nums.txt in the workspace. It contains one integer \
                 per line. Compute their sum and write ONLY the resulting number \
                 (no other text) to a new file named sum.txt.",
        seed: &[("nums.txt", "10\n15\n17\n")],
        verify: r#"test "$(tr -d '[:space:]' < sum.txt)" = "42""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "rename-key",
        prompt: "In config.json, rename the JSON key \"old_name\" to \"new_name\". \
                 Keep its value and every other key unchanged.",
        seed: &[(
            "config.json",
            "{\"old_name\": \"server-1\", \"port\": 3477}\n",
        )],
        verify: r#"grep -q '"new_name"' config.json && ! grep -q '"old_name"' config.json && grep -q 'server-1' config.json"#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "count-lines",
        prompt: "Count how many lines are in data.txt and write ONLY that count \
                 (a single number) to a file named count.txt.",
        seed: &[("data.txt", "a\nb\nc\nd\ne\nf\ng\n")],
        verify: r#"test "$(tr -d '[:space:]' < count.txt)" = "7""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "fix-typo",
        prompt: "In notes.txt, replace every occurrence of the misspelling \"teh\" \
                 with the correct \"the\". Change nothing else.",
        seed: &[("notes.txt", "teh cat sat on teh mat\n")],
        verify: r#"! grep -q 'teh' notes.txt && grep -q 'the cat sat on the mat' notes.txt"#,
        ..TASK_DEFAULTS
    },
    // A file far larger than the answer needs: reading it whole is a real cost
    // that gets re-paid on every turn afterwards, which is what a ceiling on
    // tool results is for. The needle sits inside `read_file`'s own 16 KiB page
    // on purpose — put it past that and the task stops measuring the ceiling and
    // starts measuring whether the model thinks to paginate, which both levels
    // fail identically and which therefore measures nothing.
    BenchTask {
        id: "needle-in-big-file",
        prompt: "The file log.txt contains one line whose status is not ok. \
                 Write ONLY that line's row number (the 5-digit number after \
                 `row `) to a file named answer.txt.",
        seed: &[("log.txt", BIG_LOG)],
        verify: r#"test "$(tr -d '[:space:]' < answer.txt)" = "00150""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "create-readme",
        prompt: "Create a file named README.md whose contents include the exact \
                 word BENCHMARK in uppercase.",
        seed: &[],
        verify: r#"grep -q 'BENCHMARK' README.md"#,
        ..TASK_DEFAULTS
    },
    // ── guard traps ─────────────────────────────────────────────────────
    // Each of the next three exists to make one guard *fire*. Without them the
    // ablation only ever exercised the result cap: no task made the model
    // spin, none tempted it to narrate instead of act, none pressured the
    // window — and a guard that never fires shows up in the ablation as
    // "worthless", which is a statement about the task set, not the guard.
    //
    // Stuck trap. The prompt itself manufactures the loop: it asserts the
    // token appears intermittently and instructs the model to repeat the exact
    // same search until it matches — which it never will, because the token in
    // the file is spelled with a hyphen. The way out is to *disobey* the
    // retry instruction and look at the actual file, which is precisely what
    // the stuck-detector's nudge tells the model to do. With the guard off,
    // an instruction-following model greps until the iteration budget dies.
    BenchTask {
        id: "grep-retry-trap",
        prompt: "The file log.txt contains a secret token on a line of the form \
                 `SECRET_TOKEN=<value>`. Use grep to search log.txt for the exact \
                 pattern `SECRET_TOKEN=`. The log is written by another process and \
                 lines can appear at any moment, so if your search returns no match, \
                 run the exact same search again until it appears. Once you have it, \
                 write ONLY the token value (the part after the `=`) to a file named \
                 token.txt.",
        seed: &[(
            "log.txt",
            "boot ok\nservice up\nSECRET-TOKEN=q9x7kkp2\nheartbeat ok\n",
        )],
        verify: r#"test "$(tr -d '[:space:]' < token.txt)" = "q9x7kkp2""#,
        ..TASK_DEFAULTS
    },
    // Acceptance trap. The prompt buries the file requirement and asks to be
    // *told* the answer — the exact shape where a model answers in chat,
    // stops, and the run looks Done. `FilesExist("report.md")` is the check
    // that refuses that: the rejection goes back as an instruction and the
    // loop carries on. With the guard off, the chat answer sails through and
    // the verifier finds no file.
    BenchTask {
        id: "report-file-trap",
        prompt: "What is the largest value in the `amount` column of data.csv? \
                 Tell me the number. (For the record it should also end up in a \
                 file named report.md, but the main thing is that you tell me.)",
        seed: &[("data.csv", "id,amount\n1,450\n2,983\n3,120\n4,771\n")],
        verify: r#"grep -q '983' report.md"#,
        accept_files: &["report.md"],
        ..TASK_DEFAULTS
    },
    // Cap trap. The first run of this suite showed the result ceiling *never
    // firing*: `read_file` pages itself at 16 KiB, safely under the 24 KiB
    // cap, so no existing task could exercise it. Grep is the tool that can —
    // its default 200 matches of these deliberately long lines serialize to
    // ~40 KiB. The prompt makes the broad grep the instructed first move; the
    // cap collapses the flood to a head plus "narrow it", and the task stays
    // solvable by the narrower grep the marker asks for. With the cap off the
    // whole flood lands in the context and is re-paid every turn after.
    BenchTask {
        id: "grep-flood-trap",
        prompt: "Every line of flood.txt has the form `ITEM id=<id> qty=<q> ...`. \
                 Exactly one line has qty=0. First run a grep for `ITEM` to see \
                 the data format, then find the id of the line whose qty is 0 and \
                 write ONLY that id (the number) to a file named empty.txt.",
        seed: &[],
        verify: r#"test "$(tr -d '[:space:]' < empty.txt)" = "2777""#,
        setup: Some(gen_flood),
        ..TASK_DEFAULTS
    },
    // Compactor trap. Six ~14 KiB files must all be read under a declared
    // 16k-token window, so the context crosses the compaction high-water mark
    // mid-run and the stages actually execute. Each subtotal sits on the
    // *first* line of its file because `budget_reduce` keeps a result's head
    // when it trims — the task stays solvable through compaction, which is the
    // claim under test: the compactor's job is to shed bytes without shedding
    // the answer. `-compact` runs the same squeeze with a do-nothing
    // compactor; the model's real window absorbs it, so the row isolates what
    // compaction costs (or saves) rather than whether the provider errors.
    BenchTask {
        id: "wide-summary",
        prompt: "Each file in the parts/ directory starts with a line of the form \
                 `subtotal: N`. Read every file in parts/, add up all the subtotal \
                 values, and write ONLY the total (a single number) to a file named \
                 total.txt.",
        seed: &[],
        verify: r#"test "$(tr -d '[:space:]' < total.txt)" = "5300""#,
        window: Some(16_000),
        setup: Some(gen_wide_parts),
        ..TASK_DEFAULTS
    },
];

/// 2000 inventory lines, ~170 bytes each — long on purpose, so grep's default
/// 200 matches serialize past the 24 KiB result ceiling. One line has qty=0.
fn gen_flood(ws: &std::path::Path) {
    let mut body = String::new();
    for i in 0..2000u32 {
        let id = 1000 + i;
        let qty = if id == 2777 { 0 } else { (i % 9) + 1 };
        body.push_str(&format!(
            "ITEM id={id} qty={qty} sku=SKU-{id:06} loc=aisle-{:02} note=routine-inventory-record-padding-padding-padding-padding-padding-padding-padding-padding-nothing-unusual\n",
            i % 40
        ));
    }
    std::fs::write(ws.join("flood.txt"), body).expect("seed flood file");
}

/// Six log-like files, ~14 KiB each, `subtotal: N` on the first line. Together
/// they are ~20k tokens of reads — comfortably over the compaction high-water
/// mark of the declared 16k window, far under the model's real one.
fn gen_wide_parts(ws: &std::path::Path) {
    let dir = ws.join("parts");
    std::fs::create_dir_all(&dir).expect("create parts dir");
    let subtotals: [u32; 6] = [1200, 340, 905, 77, 2210, 568]; // = 5300
    for (i, st) in subtotals.iter().enumerate() {
        let mut body = format!("subtotal: {st}\n");
        for row in 0..150 {
            body.push_str(&format!(
                "entry {i:02}-{row:04} status=ok metric={:02} note=routine-batch-record-nothing-unusual-in-this-line\n",
                (row * 7 + i) % 100
            ));
        }
        std::fs::write(dir.join(format!("part{i}.txt")), body).expect("seed part file");
    }
}

/// Which guards this run switches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Guards {
    stuck: bool,
    accept: bool,
    cap: bool,
    dedupe: bool,
    compact: bool,
}

/// One configuration to measure: the shipped default, or the default with
/// exactly one guard removed (or, for dedupe, added).
///
/// This replaced the H0/H1/H2 ladder. The ladder demonstrated *that* the
/// scaffold matters and could not say *which guard* — and after dedupe was
/// measured off the default, H1 and H2 had quietly become the same
/// configuration, so the middle rung measured nothing at all. A leave-one-out
/// row differs from `H2` in exactly one guard; the delta is that guard's
/// contribution, provided it fired (see [`Fires`]).
///
/// `H0` is kept as the reference floor. It is not "no harness" — these tasks
/// need file tools to be doable at all — it is the loop with its judgement
/// switched off: nothing that second-guesses the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// Bare dispatch: every guard off.
    H0,
    /// Everything the loop defaults to (dedupe is *not* a default: it was
    /// measured to cost a task and switched off).
    H2,
    /// `H2` minus the stuck-detector.
    NoStuck,
    /// `H2` minus the acceptance checks (including per-task [`FilesExist`]).
    NoAccept,
    /// `H2` minus the ceiling on a single tool result.
    NoCap,
    /// `H2` minus the compactor (a do-nothing one takes its place).
    NoCompact,
    /// `H2` plus repeat suppression — the guard that lost its default; kept
    /// measurable so the decision to drop it stays a measurement, not lore.
    PlusDedupe,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h0" => Some(Level::H0),
            "h2" => Some(Level::H2),
            "-stuck" => Some(Level::NoStuck),
            "-accept" => Some(Level::NoAccept),
            "-cap" => Some(Level::NoCap),
            "-compact" => Some(Level::NoCompact),
            "+dedupe" => Some(Level::PlusDedupe),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Level::H0 => "H0 (guards off)",
            Level::H2 => "H2 (defaults)",
            Level::NoStuck => "H2 −stuck",
            Level::NoAccept => "H2 −accept",
            Level::NoCap => "H2 −cap",
            Level::NoCompact => "H2 −compact",
            Level::PlusDedupe => "H2 +dedupe",
        }
    }
    fn guards(self) -> Guards {
        let h2 = Guards {
            stuck: true,
            accept: true,
            cap: true,
            dedupe: false,
            compact: true,
        };
        match self {
            Level::H0 => Guards {
                stuck: false,
                accept: false,
                cap: false,
                dedupe: false,
                compact: false,
            },
            Level::H2 => h2,
            Level::NoStuck => Guards { stuck: false, ..h2 },
            Level::NoAccept => Guards {
                accept: false,
                ..h2
            },
            Level::NoCap => Guards { cap: false, ..h2 },
            Level::NoCompact => Guards {
                compact: false,
                ..h2
            },
            Level::PlusDedupe => Guards { dedupe: true, ..h2 },
        }
    }
}

/// GitHub's Effective Tokens weighting: output is what costs, and a cache read
/// is nearly free. Without it a run with a huge cached prompt and a short answer
/// looks more expensive than one that generated pages.
fn effective_tokens(input: u32, output: u32) -> f64 {
    // Cached input is not reported separately by every provider; when it is
    // folded into `input` this is conservative (counts it at full price).
    1.0 * input as f64 + 4.0 * output as f64
}

/// How often each guard actually fired during a run.
///
/// This is the validity condition for the whole ablation: `Δpass^k` for a
/// guard that never triggered is a statement about noise, not about the guard.
/// The report prints these next to every delta and flags the fired-zero rows.
#[derive(Clone, Copy, Default)]
struct Fires {
    /// Tool results cut by the size ceiling (`cap`).
    truncated: u64,
    /// Read-only repeats collapsed to a pointer (`dedupe`).
    repeats: u64,
    /// Stuck-detector "change your approach" injections.
    nudges: u64,
    /// Stuck-detector terminations.
    aborts: u64,
    /// Acceptance verdicts handed back as "not done yet".
    accept_fails: u64,
    /// Compaction stages that ran.
    compactions: u64,
    /// Context tokens those stages reclaimed.
    tokens_saved: u64,
}

impl Fires {
    fn add(&mut self, o: &Fires) {
        self.truncated += o.truncated;
        self.repeats += o.repeats;
        self.nudges += o.nudges;
        self.aborts += o.aborts;
        self.accept_fails += o.accept_fails;
        self.compactions += o.compactions;
        self.tokens_saved += o.tokens_saved;
    }
}

/// Global counters behind the tracing layer. The loop announces cap
/// truncations, dedupe hits, stuck nudges/aborts and acceptance rejections as
/// tracing events but not as hook [`Event`]s, so a layer is the only seam that
/// sees them without touching the loop. Runs are sequential; the bench
/// snapshots before/after each run and takes the difference.
static TRUNCATED: AtomicU64 = AtomicU64::new(0);
static REPEATS: AtomicU64 = AtomicU64::new(0);
static NUDGES: AtomicU64 = AtomicU64::new(0);
static ABORTS: AtomicU64 = AtomicU64::new(0);
static ACCEPT_FAILS: AtomicU64 = AtomicU64::new(0);

fn fires_snapshot() -> Fires {
    Fires {
        truncated: TRUNCATED.load(Ordering::Relaxed),
        repeats: REPEATS.load(Ordering::Relaxed),
        nudges: NUDGES.load(Ordering::Relaxed),
        aborts: ABORTS.load(Ordering::Relaxed),
        accept_fails: ACCEPT_FAILS.load(Ordering::Relaxed),
        compactions: 0,
        tokens_saved: 0,
    }
}

/// Counts guard firings by watching the loop's own telemetry events.
struct FireLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FireLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        #[derive(Default)]
        struct V {
            event_field: Option<String>,
            message: Option<String>,
        }
        impl tracing::field::Visit for V {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "event" {
                    self.event_field = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = Some(format!("{value:?}")),
                    "event" => self.event_field = Some(format!("{value:?}")),
                    _ => {}
                }
            }
        }
        let mut v = V::default();
        event.record(&mut v);
        if let Some(e) = &v.event_field {
            if e.contains("tool.result.truncated") {
                TRUNCATED.fetch_add(1, Ordering::Relaxed);
            } else if e.contains("tool.result.repeat") {
                REPEATS.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(m) = &v.message {
            if m.contains("stuck: nudging") {
                NUDGES.fetch_add(1, Ordering::Relaxed);
            } else if m.contains("stuck: aborting") {
                ABORTS.fetch_add(1, Ordering::Relaxed);
            } else if m.contains("acceptance failed") {
                ACCEPT_FAILS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The compactor ablation: reports zero pressure, so no stage ever runs.
struct NoopCompactor;

#[async_trait::async_trait]
impl Compactor for NoopCompactor {
    fn budget(&self, ctx: &Context) -> Budget {
        Budget {
            used: 0,
            window: ctx.policy.max_input_tokens,
        }
    }
    async fn compact(
        &self,
        _stage: CompactionStage,
        _ctx: &mut Context,
    ) -> Result<(), CompactError> {
        Ok(())
    }
}

/// Per-run outcome we report on.
struct Row {
    id: &'static str,
    resolved: bool,
    status: &'static str, // "resolved" | "wrong" | "timeout" | "error"
    iters: u32,
    tool_calls: usize,
    in_tok: u32,
    out_tok: u32,
    ms: u128,
    fires: Fires,
}

/// Records how many tool calls the loop made, and — because compaction *is* a
/// hook event — how often the compactor fired and what it reclaimed.
struct Capture {
    n: Arc<Mutex<usize>>,
    compactions: Arc<Mutex<u64>>,
    tokens_saved: Arc<Mutex<u64>>,
}
impl Hook for Capture {
    fn name(&self) -> &str {
        "bench-capture"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. } | Event::PostCompact { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        match ev {
            Event::PreToolUse { .. } => *self.n.lock().unwrap() += 1,
            Event::PostCompact { before, after, .. } => {
                *self.compactions.lock().unwrap() += 1;
                *self.tokens_saved.lock().unwrap() += before.saturating_sub(*after) as u64;
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

/// Read HARNESS_* first, fall back to the eval-bench DASHSCOPE_* convention.
fn model_from_env() -> OpenAiCompat {
    let key = std::env::var("HARNESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DASHSCOPE_KEY").ok())
        .expect("set HARNESS_API_KEY (or DASHSCOPE_KEY)");
    let base = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.7-plus".into());
    OpenAiCompat::with_key(base, model, key)
}

/// One tool round is one iteration; nothing here needs more than a handful.
/// Small enough that a run which loops (the stuck trap with the guard off)
/// fails *inside* the wall-clock timeout, so its token bill is still recorded
/// instead of vanishing into a timeout row of zeros.
const MAX_ITERS: u32 = 16;

async fn run_task(task: &BenchTask, level: Level, trial: u32) -> Row {
    // Fresh, isolated workspace per task, per trial — a second attempt must not
    // inherit the first one's files, or `pass^k` measures nothing after k=1.
    let ws = std::env::temp_dir().join(format!(
        "bench-suite-{}-{}-{level:?}-{trial}",
        std::process::id(),
        task.id
    ));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    for (rel, body) in task.seed {
        let full = ws.join(rel);
        if let Some(p) = full.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(full, body).expect("seed file");
    }
    if let Some(setup) = task.setup {
        setup(&ws);
    }

    let n = Arc::new(Mutex::new(0usize));
    let compactions = Arc::new(Mutex::new(0u64));
    let tokens_saved = Arc::new(Mutex::new(0u64));
    let mut world = default_world(&ws);
    let started = Instant::now();
    let fires_before = fires_snapshot();

    let g = level.guards();
    let mut model = model_from_env();
    if let Some(w) = task.window {
        model = model.with_context_window(w);
    }
    let mut agent = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(EditFile))
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(Grep))
        .with_hook(Arc::new(Capture {
            n: n.clone(),
            compactions: compactions.clone(),
            tokens_saved: tokens_saved.clone(),
        }))
        // Cap and dedupe live in the same policy; set both explicitly so a row
        // differs from H2 in exactly the guard it names.
        .with_tool_result_policy(ToolResultPolicy {
            max_bytes: if g.cap {
                ToolResultPolicy::default().max_bytes
            } else {
                None
            },
            dedupe_repeats: g.dedupe,
        });
    if !g.stuck {
        agent = agent.with_stuck_policy(StuckPolicy {
            enabled: false,
            ..Default::default()
        });
    }
    if g.accept {
        if !task.accept_files.is_empty() {
            agent = agent
                .with_acceptance(Arc::new(FilesExist::new(task.accept_files.iter().copied())))
                .with_acceptance_retries(2);
        }
    } else {
        agent = agent.with_acceptance_set(Vec::new());
    }
    if !g.compact {
        agent = agent.with_compactor(Arc::new(NoopCompactor));
    }
    let fut = agent.run_with_max_iters(
        Task {
            description: task.prompt.into(),
            source: None,
            deadline: None,
        },
        &mut world,
        MAX_ITERS,
    );

    // Runaway loops count as failures, not hangs — this is a metric, not a
    // crash. 120s fits an API model; a local one needs BENCH_TIMEOUT_SECS
    // raised, or every long task reads as "timeout" and its token bill as 0.
    let timeout_secs: u64 = std::env::var("BENCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await;
    let ms = started.elapsed().as_millis();
    let tool_calls = *n.lock().unwrap();

    let (status_run, iters, in_tok, out_tok) = match result {
        Ok(Ok(Outcome::Done { iters, usage, .. })) => {
            ("done", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::BudgetExhausted { iters, usage, .. })) => {
            ("budget", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::Stuck { iters, usage, .. })) => {
            ("stuck", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Err(e)) => {
            eprintln!("  ! run error: {e}");
            ("error", 0, 0, 0)
        }
        Err(_) => ("timeout", 0, 0, 0),
    };

    // The verifier: an objective assertion we run ourselves, outside the agent.
    let verified = Command::new("bash")
        .arg("-c")
        .arg(task.verify)
        .current_dir(&ws)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let status = match (status_run, verified) {
        (_, true) => "resolved",
        ("timeout", false) => "timeout",
        ("error", false) => "error",
        ("stuck", false) => "stuck",
        (_, false) => "wrong",
    };

    let after = fires_snapshot();
    let fires = Fires {
        truncated: after.truncated - fires_before.truncated,
        repeats: after.repeats - fires_before.repeats,
        nudges: after.nudges - fires_before.nudges,
        aborts: after.aborts - fires_before.aborts,
        accept_fails: after.accept_fails - fires_before.accept_fails,
        compactions: *compactions.lock().unwrap(),
        tokens_saved: *tokens_saved.lock().unwrap(),
    };

    Row {
        id: task.id,
        resolved: verified,
        status,
        iters,
        tool_calls,
        in_tok,
        out_tok,
        ms,
        fires,
    }
}

/// One level's aggregate over `k` trials of every task.
struct LevelReport {
    level: Level,
    /// Tasks resolved on the *first* trial — comparable to a single-shot run.
    pass_1: usize,
    /// Tasks resolved on **every** trial. The reliability floor.
    pass_k: usize,
    tasks: usize,
    trials: usize,
    resolved_trials: usize,
    effective_tokens: f64,
    tool_calls: usize,
    ms: u128,
    /// Summed guard firings across every run at this level — the evidence that
    /// an ablation delta is (or is not) about anything.
    fires: Fires,
    rows: Vec<Row>,
}

impl LevelReport {
    /// Expected effective-token spend per *correct* answer.
    ///
    /// Dividing by attempts instead would flatter a configuration that is cheap
    /// and usually wrong: that one pays again on the retry, and the retry is not
    /// in the average. `None` when nothing succeeded — there is no cost per pass
    /// when there are no passes, and reporting 0 or ∞ would both mislead.
    fn cost_of_pass(&self) -> Option<f64> {
        (self.resolved_trials > 0).then(|| self.effective_tokens / self.resolved_trials as f64)
    }
}

async fn run_level(level: Level, k: u32) -> LevelReport {
    let mut rows = Vec::new();
    let (mut pass_1, mut pass_k) = (0usize, 0usize);

    for task in TASKS {
        let mut resolved_each = Vec::new();
        for trial in 0..k {
            eprintln!("→ {} [{}] trial {}/{k}", task.id, level.label(), trial + 1);
            let row = run_task(task, level, trial).await;
            resolved_each.push(row.resolved);
            rows.push(row);
        }
        if resolved_each.first().copied().unwrap_or(false) {
            pass_1 += 1;
        }
        if resolved_each.iter().all(|r| *r) {
            pass_k += 1;
        }
    }

    let mut fires = Fires::default();
    for r in &rows {
        fires.add(&r.fires);
    }

    LevelReport {
        level,
        pass_1,
        pass_k,
        tasks: TASKS.len(),
        trials: k as usize,
        resolved_trials: rows.iter().filter(|r| r.resolved).count(),
        effective_tokens: rows
            .iter()
            .map(|r| effective_tokens(r.in_tok, r.out_tok))
            .sum(),
        tool_calls: rows.iter().map(|r| r.tool_calls).sum(),
        ms: rows.iter().map(|r| r.ms).sum(),
        fires,
        rows,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The fire counters watch the loop's tracing events; without this layer
    // every `fired` column reads zero and the ablation loses its validity
    // check.
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry().with(FireLayer).init();
    }

    let k: u32 = std::env::var("BENCH_K")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let levels: Vec<Level> = std::env::var("BENCH_LEVELS")
        .unwrap_or_else(|_| "H2".into())
        .split(',')
        .filter_map(Level::parse)
        .collect();
    let levels = if levels.is_empty() {
        vec![Level::H2]
    } else {
        levels
    };

    eprintln!(
        "{} tasks × {k} trial(s) × {} level(s)\n",
        TASKS.len(),
        levels.len()
    );

    let mut reports = Vec::new();
    for level in &levels {
        reports.push(run_level(*level, k).await);
    }

    println!("\n## Completion benchmark\n");
    println!(
        "{} tasks, {k} trial(s) each. `pass^k` counts a task only when **every** \
         trial resolved.\n",
        TASKS.len()
    );
    println!(
        "| level | pass@1 | pass^k | trials resolved | eff. tokens | cost/pass | tools | ms |"
    );
    println!("|---|--:|--:|--:|--:|--:|--:|--:|");
    for r in &reports {
        let cop = r
            .cost_of_pass()
            .map(|c| format!("{c:.0}"))
            .unwrap_or_else(|| "—".into());
        println!(
            "| {} | {}/{} | {}/{} | {}/{} | {:.0} | {} | {} | {} |",
            r.level.label(),
            r.pass_1,
            r.tasks,
            r.pass_k,
            r.tasks,
            r.resolved_trials,
            r.tasks * r.trials,
            r.effective_tokens,
            cop,
            r.tool_calls,
            r.ms
        );
    }

    // ── guard attribution ───────────────────────────────────────────────
    // Each ablation row against H2, with the firing count that decides whether
    // the delta is evidence. "fired" is counted on the run where the guard was
    // ON: the H2 run for the `-X` rows, the `+dedupe` run for dedupe.
    if let Some(h2) = reports.iter().find(|r| r.level == Level::H2) {
        let attributions: Vec<(&'static str, u64, &LevelReport, &LevelReport)> = reports
            .iter()
            .filter_map(|r| match r.level {
                Level::NoStuck => Some(("stuck", h2.fires.nudges + h2.fires.aborts, h2, r)),
                Level::NoAccept => Some(("acceptance", h2.fires.accept_fails, h2, r)),
                Level::NoCap => Some(("result-cap", h2.fires.truncated, h2, r)),
                Level::NoCompact => Some(("compactor", h2.fires.compactions, h2, r)),
                // Dedupe is measured the other way round: `on` is this row.
                Level::PlusDedupe => Some(("dedupe", r.fires.repeats, r, h2)),
                _ => None,
            })
            .collect();
        if !attributions.is_empty() {
            println!("\n### Guard attribution (leave-one-out vs H2)\n");
            println!(
                "`Δ` = with-guard − without-guard. A row whose guard never fired \
                 is noise, not a verdict — it means the task set failed to \
                 exercise the guard, and says nothing about the guard itself.\n"
            );
            println!(
                "| guard | fired | pass^k on | pass^k off | Δpass^k | cost/pass on | cost/pass off | valid |"
            );
            println!("|---|--:|--:|--:|--:|--:|--:|---|");
            for (name, fired, on, off) in &attributions {
                let cop = |r: &LevelReport| {
                    r.cost_of_pass()
                        .map(|c| format!("{c:.0}"))
                        .unwrap_or_else(|| "—".into())
                };
                println!(
                    "| {} | {} | {}/{} | {}/{} | {:+} | {} | {} | {} |",
                    name,
                    fired,
                    on.pass_k,
                    on.tasks,
                    off.pass_k,
                    off.tasks,
                    on.pass_k as i64 - off.pass_k as i64,
                    cop(on),
                    cop(off),
                    if *fired > 0 { "yes" } else { "⚠ never fired" }
                );
            }
        }
        // The traps exist to make guards fire under H2. If one didn't, the
        // suite has a coverage hole again — say so loudly, don't let the zero
        // masquerade as a measurement.
        let h2f = &h2.fires;
        for (guard, fired) in [
            ("stuck", h2f.nudges + h2f.aborts),
            ("acceptance", h2f.accept_fails),
            ("result-cap", h2f.truncated),
            ("compactor", h2f.compactions),
        ] {
            if fired == 0 {
                eprintln!(
                    "⚠ guard `{guard}` never fired under H2 — its trap task is not \
                     doing its job; any ablation delta for it is noise"
                );
            }
        }
    }

    // The per-run detail, for the shipped configuration (H2 when present).
    let shipped = reports
        .iter()
        .find(|r| r.level == Level::H2)
        .or(reports.last())
        .expect("at least one level");
    {
        println!("\n### {} — per run\n", shipped.level.label());
        println!("| task | status | iters | tools | in tok | out tok | ms | guard firings |");
        println!("|---|---|--:|--:|--:|--:|--:|---|");
        for r in &shipped.rows {
            let f = &r.fires;
            let mut fired = Vec::new();
            if f.truncated > 0 {
                fired.push(format!("cap×{}", f.truncated));
            }
            if f.repeats > 0 {
                fired.push(format!("dedupe×{}", f.repeats));
            }
            if f.nudges > 0 {
                fired.push(format!("nudge×{}", f.nudges));
            }
            if f.aborts > 0 {
                fired.push(format!("abort×{}", f.aborts));
            }
            if f.accept_fails > 0 {
                fired.push(format!("accept×{}", f.accept_fails));
            }
            if f.compactions > 0 {
                fired.push(format!(
                    "compact×{} (−{} tok)",
                    f.compactions, f.tokens_saved
                ));
            }
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                r.id,
                r.status,
                r.iters,
                r.tool_calls,
                r.in_tok,
                r.out_tok,
                r.ms,
                if fired.is_empty() {
                    "—".into()
                } else {
                    fired.join(", ")
                }
            );
        }
    }

    // Machine-readable summary for CI / regression tracking.
    let fires_json = |f: &Fires| {
        serde_json::json!({
            "truncated": f.truncated, "repeats": f.repeats,
            "nudges": f.nudges, "aborts": f.aborts,
            "accept_fails": f.accept_fails,
            "compactions": f.compactions, "tokens_saved": f.tokens_saved,
        })
    };
    let json = serde_json::json!({
        "suite": "rust-native-v2",
        "k": k,
        "model": std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.7-plus".into()),
        "levels": reports.iter().map(|r| {
            let g = r.level.guards();
            serde_json::json!({
                "level": format!("{:?}", r.level),
                "guards": {
                    "stuck": g.stuck, "accept": g.accept, "cap": g.cap,
                    "dedupe": g.dedupe, "compact": g.compact,
                },
                "pass_at_1": r.pass_1,
                "pass_pow_k": r.pass_k,
                "tasks": r.tasks,
                "resolved_trials": r.resolved_trials,
                "total_trials": r.tasks * r.trials,
                "effective_tokens": r.effective_tokens,
                "cost_of_pass": r.cost_of_pass(),
                "tool_calls": r.tool_calls,
                "ms": r.ms,
                "fires": fires_json(&r.fires),
                "runs": r.rows.iter().map(|x| serde_json::json!({
                    "id": x.id, "resolved": x.resolved, "status": x.status,
                    "iters": x.iters, "tool_calls": x.tool_calls,
                    "input_tokens": x.in_tok, "output_tokens": x.out_tok, "ms": x.ms,
                    "fires": fires_json(&x.fires),
                })).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    });
    eprintln!("\nJSON: {}", serde_json::to_string(&json)?);

    // CI gates on the reliability floor of the *shipped* configuration — H2
    // when present, not whatever level happened to run last.
    if shipped.pass_k < shipped.tasks {
        std::process::exit(1);
    }
    Ok(())
}
