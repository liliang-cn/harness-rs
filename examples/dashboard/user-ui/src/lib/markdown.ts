// Markdown rendering for assistant messages.
//
// `marked` does the parsing (GFM + line breaks so `\n` survives), DOMPurify
// strips anything `<script>`/inline-handler-shaped before we set innerHTML.
// We keep `target` + `rel` attrs because the renderer below opens links in
// a new tab — safer than nav-ing away from a streaming chat.
import { marked } from 'marked';
import DOMPurify from 'dompurify';

marked.setOptions({ gfm: true, breaks: true });

// DOMPurify hook: every link gets target="_blank" rel="noopener noreferrer".
// This runs on the parsed DOM right before serialization — cleaner than
// overriding marked's link renderer (whose token signature drifts between
// versions).
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName === 'A') {
    node.setAttribute('target', '_blank');
    node.setAttribute('rel', 'noopener noreferrer');
  }
});

export function renderMarkdown(md: string): string {
  const raw = marked.parse(md, { async: false }) as string;
  return DOMPurify.sanitize(raw, {
    ADD_ATTR: ['target', 'rel'],
    USE_PROFILES: { html: true },
  });
}
