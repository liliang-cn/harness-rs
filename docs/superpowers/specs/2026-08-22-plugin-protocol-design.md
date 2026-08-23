# Plugin protocol v0 — tools and hooks from an external process

**Date:** 2026-08-22
**Status:** design approved, not implemented
**Scope:** `harness-rs` only. SuperLeo's side is named where it matters and specified elsewhere.

## Why

A harness application can only be extended by recompiling it. Tools, hooks, guides
and sensors are all compile-time registrations (`inventory`), so a third party
cannot add anything to somebody else's agent, and the framework's own consumers
(SuperLeo, TagIt, dataintelligence) each rebuild the same wiring.

MCP already closes half of this: `harness-mcp-client` turns an external server's
tools into `Arc<dyn Tool>`. What it cannot do is let that external process
participate in the loop — deny a dangerous call, inject context, react to a
compaction. Those are `Hook`s, and hooks are the reason plugins are interesting:
a tool adds an ability, a hook adds a *policy*.

**This lives in harness-rs, not SuperLeo, for one decisive reason:** the protocol
has to be readable by the people who write plugins. harness-rs is public and on
crates.io; SuperLeo is a private repository. Mechanism (process supervision,
protocol, trait adaptation) belongs to the framework; governance (who may install
what, where secrets live, who approves) belongs to the product.

## Non-goals

Named because each is a plausible thing to assume is included:

- **Channels.** A chat channel is an inbound message source with a reply path.
  harness has no such concept — `Sensor` observes actions inside the loop, it is
  not an ingress. Channel plugins are a SuperLeo concern and wait for a later
  slice.
- **Model providers, memory backends.** BYOK plus any OpenAI-compatible endpoint
  already covers the first; CortexDB is deliberately the second.
- **WASM sandboxing.** A subprocess is the unit of isolation in v0. WASM is the
  better long-term answer for untrusted code and is a separate decision.
- **A registry or install command.** Plugins are paths and URLs in configuration
  until there is someone to distribute them to.
- **Guides and sensors as extension points.** Both are hot-path, per-turn, and
  `Guide` shapes the prompt — remote latency there is a different design problem.

## Protocol

**v0 is MCP plus two optional methods.** An unmodified MCP server is a valid
tool-only plugin: that is not a compatibility accident, it is the point. Anything
a plugin author already knows about MCP transports, framing, and errors carries
over, and every MCP server in the wild is already a plugin.

Method discovery is by the MCP rule that already exists: a server that does not
implement a method answers `-32601 Method not found`, and the host degrades.

### `plugin/hello` (optional)

```jsonc
// → request
{"method": "plugin/hello", "params": {"host": "harness-rs", "protocol": 0}}
// ← response
{
  "protocol": 0,                       // highest version the plugin speaks
  "plugin": {"id": "acme-guard", "version": "1.2.0"},
  "capabilities": ["tools", "hooks"],  // what the host should ask for next
  "requires": {                        // permissions, granted by the host or refused
    "network": ["api.acme.example"],
    "fs_read": ["${workspace}"],
    "fs_write": []
  }
}
```

No `plugin/hello` means "tools only, unknown version, requires nothing" — the
plain-MCP case.

`requires` is a request, never a grant. The host answers with what it actually
granted, and the plugin may refuse to run on less.

Enforcement follows the framework's existing principle — permissions are decided
when the process is *spawned*, not re-prompted per call. A stdio plugin is
launched through `harness-sandbox` (`SeatbeltSandbox` on macOS, `BubblewrapSandbox`
on Linux: kernel-enforced, network denied by default), so `fs_read` / `fs_write` /
`network` are real confinement rather than a promise. Where no OS backend is
available the host says so — `Isolation` already exists to make that honest — and
an ungranted plugin is a plugin the operator chose to trust.

`harness-permissions` is a different axis and stays where it is: it decides which
*tool names* an agent may call, and a plugin's tools go through it like any other.

### `hooks/list` (optional)

```jsonc
{
  "hooks": [{
    "name": "acme-guard/deny-rm-rf",
    "events": ["PreToolUse"],           // Event variant names, exact
    "match": {"tool_name": "shell*"},   // optional narrowing, glob
    "timeout_ms": 500,
    "on_timeout": "allow"               // "allow" | "deny"
  }]
}
```

Matching is declared, not asked. `Hook::matches` runs on every event in the loop;
a round trip per event would put a process boundary in the hot path. The host
answers `matches()` locally from `events` + `match`, and only fires across the
wire when both agree.

### `hooks/fire`

```jsonc
// →
{"method": "hooks/fire", "params": {"hook": "acme-guard/deny-rm-rf", "event": {"type": "PreToolUse", "tool": "shell", "args": {...}}}}
// ←
{"outcome": "deny", "reason": "rm -rf outside the workspace"}
// or {"outcome": "allow"} | {"outcome": "inject", "text": "..."} | {"outcome": "mutate", "value": {...}}
```

## The one hard problem: a sync trait over an async wire

```rust
pub trait Hook: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn matches(&self, ev: &Event<'_>) -> bool;
    fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome;   // sync!
}
```

`fire` returns a value, not a future, and it is called from inside the loop. A
remote hook has to do IO there. Three consequences, all deliberate:

1. **Blocking is bounded and explicit.** `fire` hands the serialized event to the
   plugin's supervisor task over a channel and waits on the reply with the
   hook's declared `timeout_ms` (default 500, host cap 5000). No unbounded wait
   exists anywhere in this design.
2. **Timeout is a policy the plugin declares and the host may override.**
   `on_timeout: "allow"` for observers, `"deny"` for gates. The default is
   `allow`: a plugin that dies must not brick the agent. A security gate that
   wants fail-closed says so, and then a dead plugin blocks the tool — which is
   the correct behaviour for a gate and the wrong one for a logger.
3. **Remote hooks do not get `World`.** They receive a serialized event and
   return an outcome; they cannot read or mutate agent state. Anything a plugin
   needs from the world has to arrive in the event payload or be fetched through
   a tool it also exposes. This keeps the wire surface small and the security
   story honest — a plugin cannot reach into memory it was never granted.

`Event<'a>` borrows; the serialized form is a separate owned type (`WireEvent`)
with a `From<&Event<'_>>`. `WireEvent` carries a per-variant **allowlist** of
fields rather than mirroring the enum: a third-party process gets the fields a
hook needs to decide, and adding a field to `Event` does not silently widen what
plugins can see. Free text that survives that filter passes through
`harness-redact` first, so a plugin that logs everything does not become the place
the user's PII leaks.

## Components

| Crate | Responsibility |
|---|---|
| `harness-plugin-host` (new) | Spawn/connect, supervise, negotiate, adapt |
| `harness-mcp-client` (exists) | Transport + `tools()` — reused, not reimplemented |
| `harness-core` (exists) | `Tool`, `Hook`, `Event` — unchanged |
| `harness-sandbox` (exists) | Spawn-time confinement for `requires` |
| `harness-redact` (exists) | PII scrubbing on the wire |

Public surface, deliberately four calls:

```rust
let plugin = Plugin::spawn(PluginSpec::stdio("acme-guard", ["--flag"])).await?;
// or Plugin::connect(PluginSpec::http(url).with_header(...)).await?;
let tools: Vec<Arc<dyn Tool>> = plugin.tools();
let hooks: Vec<Arc<dyn Hook>> = plugin.hooks();
plugin.shutdown().await;
```

`Plugin` owns a supervisor task. Restart on exit with exponential backoff
(100ms → 5s, five attempts), then **quarantine**: the plugin stays down, its
tools and hooks answer with a stable error, and one `Event::Error` is emitted.
A crash loop must be visible and finite, not a busy loop that looks like slowness.

## Failure semantics

| Situation | Behaviour |
|---|---|
| Plugin exits mid tool call | `ToolError`, call fails, no panic, restart begins |
| Tool call exceeds its timeout | `ToolError`; the plugin is not restarted (slow ≠ dead) |
| Hook exceeds `timeout_ms` | Declared `on_timeout`; one warning per hook per run, not per event |
| `plugin/hello` absent | Tools-only plugin, no hooks requested |
| `hooks/list` absent | Same |
| Protocol version above host's | Refuse to load, name both versions in the error |
| `requires` cannot be granted | Refuse to load, name the missing grant |

## Testing

- **Deterministic integration tests, no network.** The repo already ships
  `harness-mcp-client/src/bin/mcp-echo-server.rs`; a sibling `plugin-echo` binary
  adds `plugin/hello`, `hooks/list` and `hooks/fire` with scriptable outcomes.
- Tests to write: a plain MCP server loads as tools-only; a plugin hook denies a
  tool call and the loop honours it; a hook that sleeps past its timeout yields
  its declared outcome; a plugin killed mid-call surfaces `ToolError` and comes
  back; five crashes quarantine it; a `requires` the host refuses fails the load
  with both names in the message.
- **Timeout tests must not sleep in real time** where avoidable — the timeout is
  injected, not hardcoded, so tests set it to milliseconds.

## Acceptance

1. An external plugin exposing one tool and one `PreToolUse` hook is loaded by a
   harness application; the hook observably denies a call the agent tried to make.
2. An unmodified MCP server (the existing `mcp-echo-server`) loads as a tools-only
   plugin with no changes to it.
3. Killing the plugin process mid-run produces an error and a recovery, not a
   hung or panicking agent.
4. **Dogfood:** one tool group currently compiled into SuperLeo runs as an
   external plugin instead, with behaviour unchanged. If an external plugin
   cannot reach parity with a built-in, the protocol is wrong and this is where
   that gets discovered.

## What this is worth if nobody writes a plugin

Then harness applications gain a supported way to load capability from a separate
process, SuperLeo can move tool groups out of its binary, and the tools SuperLeo
already has become reachable by other agents. The ecosystem is upside, not the
premise.
