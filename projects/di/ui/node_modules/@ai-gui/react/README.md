# @ai-gui/react

React adapter for [AIGUI](../../README.md) — renders a streaming LLM response into React, with app-defined cards and plugins.

## Install

```sh
pnpm add @ai-gui/core @ai-gui/react
```

## Usage

```tsx
import { ActionRegistry, CardRegistry, CardStore, createActionRuntime } from "@ai-gui/core"
import { AIRenderer, useActionState } from "@ai-gui/react"
import { useRef } from "react"

const registry = new CardRegistry()
registry.register({
  type: "weather",
  description: "Weather summary",
  // card render = a React component receiving { data, onAction }
  render: ({ data, onAction }: { data: any; onAction: (a: any) => void }) => (
    <div>
      {data.city} — {data.tempC}°C
      <button onClick={() => onAction({ type: "refresh", params: { city: data.city } })}>Refresh</button>
    </div>
  ),
})

const actions = new ActionRegistry()
actions.register({
  type: "refresh",
  run: async (params, { signal }) => fetch("/api/weather", { signal }).then((r) => r.json()),
})
const cardStore = new CardStore({ registry })
const actionRuntime = createActionRuntime({ registry: actions, cardStore })

function Chat() {
  const ref = useRef<React.ComponentRef<typeof AIRenderer>>(null)
  // ref.current?.push(chunk) / feed(source) / reset()
  return (
    <AIRenderer
      ref={ref}
      registry={registry}
      cardStore={cardStore}
      actionRuntime={actionRuntime}
      onCardAction={(action) => console.log("observed", action)}
    />
  )
}
```

## Exports

- `<AIRenderer ref registry cardStore plugins sanitize actionRuntime onCardAction />` — imperative `ref.current.push/feed/reset`.
- `useAIRenderer(options)` → `{ nodes, push, feed, reset }` — the hook form when you want to render `nodes` yourself.
- `useActionState(runtime, key)` — subscribe to one action's `idle | pending | success | error | cancelled` state.

Cards with a top-level `id` use the supplied `CardStore`. Their React component receives `{ data, state, onAction }`; patches update props while preserving component and DOM state.

See the [root README](../../README.md) for cards, plugins, and `buildSystemPrompt`.
