# @ai-gui/core

The core also exposes provider-neutral model stream primitives:

```ts
import { contentDeltas, mockModelStream, parseSSE } from "@ai-gui/core"

const events = mockModelStream([
  { type: "content", delta: "Hello" },
  { type: "usage", data: { outputTokens: 1 } },
])
await renderer.feed(contentDeltas(events))
```

`parseSSE`, `jsonLines`/`ndjson`, and `textLines` accept fetch responses, byte streams, and async byte iterables. They preserve split UTF-8 characters, support cancellation, release readers, and let callers reject or skip malformed records. `readableBytes` and `mockModelStream` are deterministic helpers for tests and examples.

The headless streaming engine behind [AIGUI](../../README.md) — framework-agnostic. It parses a streaming LLM response into an AST + patches, runs plugin node renderers, sanitizes HTML, and builds the system prompt. Use it directly, or via an adapter (`@ai-gui/react`, `@ai-gui/vue`, `@ai-gui/vanilla`).

## Install

```sh
pnpm add @ai-gui/core
```

## Usage

```ts
import { Renderer, CardRegistry, buildSystemPrompt } from "@ai-gui/core"

const registry = new CardRegistry()
registry.register({ type: "weather", description: "Weather summary", example: { city: "Tokyo" } })

const renderer = new Renderer({
  registry,
  sanitize: true,
  onPatch: (patches, nodes) => {
    // called as the stream grows; render `nodes` however you like
  },
})

// feed an AsyncIterable<string> or a ReadableStream, or push chunks manually
await renderer.feed(response.body!)
// renderer.push("more text"); renderer.reset()

// assemble the system-prompt guidance for the model
const system = buildSystemPrompt({ registry })
```

## Actions

```ts
import { ActionRegistry, createActionRuntime } from "@ai-gui/core"

const actions = new ActionRegistry()
actions.register<{ city: string }, unknown>({
  type: "weather.refresh",
  schema: {
    type: "object",
    required: ["city"],
    properties: { city: { type: "string" } },
  },
  run: async (params, { signal, actionId, cardType }) => {
    return fetch(`/api/weather?city=${encodeURIComponent(params.city)}`, { signal }).then((r) => r.json())
  },
})

const runtime = createActionRuntime({ registry: actions, timeoutMs: 10_000 })
const result = await runtime.dispatch({
  type: "weather.refresh",
  params: { city: "Tokyo" },
  cardType: "weather",
})
```

Use `runtime.subscribe()`, `runtime.getState(key)`, `runtime.cancel(key)`, `runtime.reset()` and `runtime.destroy()` to observe and control actions. Automatic dispatch errors are observed through runtime state or adapter hooks; `onCardAction` / `card-action` only observe action events. Pending duplicate dispatches from the same owner share one Promise; adapters provide isolated owners automatically.

## Stateful cards

```ts
import { ActionRegistry, CardStore, createActionRuntime } from "@ai-gui/core"

const cardStore = new CardStore({ registry })
cardStore.register({
  id: "weather-tokyo",
  type: "weather",
  data: { id: "weather-tokyo", city: "Tokyo", tempC: 24 },
})

cardStore.apply({
  op: "merge",
  cardId: "weather-tokyo",
  data: { tempC: 25 },
})

const snapshot = cardStore.snapshot()
cardStore.restore(JSON.parse(JSON.stringify(snapshot)))

const actions = new ActionRegistry()
actions.register({
  type: "weather.refresh",
  async run(_params, { cardId }) {
    return { op: "merge", cardId: cardId!, data: { tempC: 26 } }
  },
})

const runtime = createActionRuntime({ registry: actions, cardStore })
```

`CardStore` supports initialize-if-absent registration, immutable records, recursive object merge, replace, atomic patch batches, revision checks, subscriptions, delete/clear, and snapshot/restore. Action patch results use optimistic mutation epochs, so an older Action cannot overwrite a Card changed, deleted, recreated, or restored after that Action started.

## Exports

- `Renderer` — `push(chunk)`, `feed(AsyncIterable | ReadableStream)`, `reset()`; constructor `{ registry?, plugins?, sanitize?, onPatch?(patches, nodes) }`.
- `StreamRouter` — demultiplex one stream into named channels: `.channel(name, sink)`, `.on(name, cb)`, `.feed(source)`.
- `CardRegistry` — `register(def)`, `parse(type, rawJson)`, `getRender(type)`, `toPromptSpec()`, `toJSONSchema()`.
- `CardStore` — `register`, `get`, `list`, `subscribe`, `apply`, `applyAll`, `delete`, `clear`, `snapshot`, and `restore` for Cards with stable IDs.
- `ActionRegistry`, `ActionRuntime`, `createActionRuntime`, `getActionKey`, `getIdleActionState` — validated application-owned action execution and observable lifecycle state.
- `buildSystemPrompt({ base?, registry?, plugins? })`.
- Utilities: `parsePartialJSON`, `repairMarkdown`, `sanitizeHtml`, `createParser`, `diffAst`, `collectNodeRenderers`.
- Types: `ASTNode`, `Patch`, `RenderOutput` (`html | element | card | mount`), `CardDef`, `AIGuiPlugin`, `NodeRenderer`, `RendererOptions`, `JSONSchema`.

See the [root README](../../README.md) for the full picture.
