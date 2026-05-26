// Tiny pub/sub so non-chat pages (e.g. Plans) can open the chat sheet with the
// composer pre-filled. The ChatFab subscribes; callers invoke openChatWith().
type Listener = (text: string) => void;
const listeners = new Set<Listener>();

export function openChatWith(text: string) {
  for (const l of listeners) l(text);
}

export function subscribeChatPrefill(l: Listener): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}
