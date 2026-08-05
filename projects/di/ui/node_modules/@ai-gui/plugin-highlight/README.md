# @ai-gui/plugin-highlight

Syntax-highlighting plugin for [AIGUI](../../README.md), powered by [Shiki](https://shiki.style). Overrides fenced code blocks with highlighted output. Async.

## Install

```sh
pnpm add @ai-gui/plugin-highlight
```

## Usage

```tsx
import { highlight } from "@ai-gui/plugin-highlight"
import { AIRenderer } from "@ai-gui/react"

<AIRenderer plugins={[highlight({ themes: ["github-dark"], langs: ["ts", "python"] })]} />
```

## Options

- `themes?: string[]` — themes to load; the first is the default when `theme` is omitted.
- `langs?: string[]` — grammars to load; a code block whose language is not listed falls back to plain text.
- `theme?: string` — theme used for rendering; defaults to the first entry of `themes`.

See the [root README](../../README.md) for the full plugin list.
