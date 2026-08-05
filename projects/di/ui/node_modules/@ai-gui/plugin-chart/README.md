# @ai-gui/plugin-chart

Chart plugin for [AIGUI](../../README.md), powered by [ECharts](https://echarts.apache.org). The model emits a ` ```chart ` fenced block containing an ECharts option JSON. Charts are complete-gated: a skeleton shows while the option streams, then the full chart renders (never partial-drawn).

## Install

```sh
pnpm add @ai-gui/plugin-chart
```

Install `echarts-gl` only when using `chart({ gl: true })`:

```sh
pnpm add echarts-gl
```

## Usage

```tsx
import { chart } from "@ai-gui/plugin-chart"
import { AIRenderer } from "@ai-gui/react"

<AIRenderer plugins={[chart({ interactive: true })]} />
```

The model emits, e.g.:

    ```chart {"xAxis":{"type":"category","data":["A","B"]},"yAxis":{"type":"value"},"series":[{"type":"bar","data":[1,2]}]}```

## Options

- `interactive?: boolean` — when true, complete options render a **live** ECharts instance (tooltip / dataZoom / click) via a `mount` output. When false/omitted, they render a static SSR SVG.
- `gl?: boolean` — when true, render 3D charts via the optional `echarts-gl` peer dependency (WebGL, live-only; implies interactive). Enables 3D series like `bar3D`, `scatter3D`, `surface`, `line3D`, `globe`, `map3D`.
- `width?: number` / `height?: number` — chart dimensions (default 600 × 400).

## Exports

- `chart(options)` — the plugin.
- `chartPromptSpec()` — the prompt-spec string (also folded in automatically by `buildSystemPrompt` when the plugin is passed).

See the [root README](../../README.md) for the full plugin list.
