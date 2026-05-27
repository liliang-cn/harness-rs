import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Mic, MicOff, Send, Square, X, FileText } from 'lucide-react';
import { Textarea } from '@/components/ui/textarea';
import { cn } from '@/lib/utils';
import { fetchAttachmentBlob, type Attachment } from '@/lib/api';
import { AttachmentButton } from './attachment-button';

interface ComposerProps {
  onSend: (text: string, attachment_ids: string[]) => void;
  onStop?: () => void;
  busy: boolean;
}

const MAX_ATTACHMENTS = 3;

// Minimal type surface for Web Speech API — no @types package needed.
interface SpeechRecognitionEvent extends Event {
  results: ArrayLike<ArrayLike<{ transcript: string; isFinal: boolean }>>;
  resultIndex: number;
}
interface SpeechRecognition extends EventTarget {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  onresult: ((ev: SpeechRecognitionEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onend: (() => void) | null;
  start(): void;
  stop(): void;
}
type SpeechRecognitionCtor = new () => SpeechRecognition;

function getSpeechRecognitionCtor(): SpeechRecognitionCtor | null {
  if (typeof window === 'undefined') return null;
  const w = window as unknown as {
    SpeechRecognition?: SpeechRecognitionCtor;
    webkitSpeechRecognition?: SpeechRecognitionCtor;
  };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

export function Composer({ onSend, onStop, busy }: ComposerProps) {
  const { t, i18n } = useTranslation();
  const [text, setText] = useState('');
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [listening, setListening] = useState(false);
  const recogRef = useRef<SpeechRecognition | null>(null);
  const SpeechCtor = getSpeechRecognitionCtor();

  // Cleanup the recognizer on unmount (e.g. Sheet close mid-listen).
  useEffect(() => {
    return () => {
      try {
        recogRef.current?.stop();
      } catch {
        /* ignore */
      }
    };
  }, []);

  function trySend() {
    const v = text.trim();
    if ((!v && attachments.length === 0) || busy) return;
    onSend(v, attachments.map((a) => a.id));
    setText('');
    setAttachments([]);
  }

  function removeAttachment(id: string) {
    setAttachments((cur) => cur.filter((a) => a.id !== id));
  }

  function onKey(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    // Cmd/Ctrl+Enter always sends. Plain Enter sends unless Shift held.
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      trySend();
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      trySend();
    }
  }

  function toggleMic() {
    if (!SpeechCtor) return;
    if (listening) {
      try {
        recogRef.current?.stop();
      } catch {
        /* ignore */
      }
      setListening(false);
      return;
    }
    const r = new SpeechCtor();
    r.lang = i18n.language.startsWith('zh') ? 'zh-CN' : 'en-US';
    r.continuous = false;
    r.interimResults = true;
    r.onresult = (ev: SpeechRecognitionEvent) => {
      let interim = '';
      let finalText = '';
      for (let i = ev.resultIndex; i < ev.results.length; i++) {
        const result = ev.results[i];
        const alt = result[0];
        if (alt.isFinal) finalText += alt.transcript;
        else interim += alt.transcript;
      }
      // Fill input — user reviews + clicks send (per spec).
      setText((prev) => (finalText ? `${prev}${finalText}`.trim() : prev || interim));
    };
    r.onerror = () => setListening(false);
    r.onend = () => setListening(false);
    try {
      r.start();
      recogRef.current = r;
      setListening(true);
    } catch {
      setListening(false);
    }
  }

  const canSend = !busy && (text.trim().length > 0 || attachments.length > 0);

  return (
    <div className="border-border bg-background sticky bottom-0 border-t p-3">
      {attachments.length > 0 && (
        <div className="mb-2 flex flex-wrap gap-1.5">
          {attachments.map((a) => (
            <AttachmentPreview key={a.id} attachment={a} onRemove={() => removeAttachment(a.id)} />
          ))}
        </div>
      )}
      {/* Pill container — mic / attach / textarea / send sit on the same
          baseline inside one rounded border. */}
      <div
        className={cn(
          'border-input bg-background flex items-center gap-1 rounded-2xl border pr-1 pl-1 transition-shadow',
          'focus-within:ring-ring/30 focus-within:ring-2',
        )}
      >
        {SpeechCtor && (
          <button
            type="button"
            aria-label={listening ? t('chat.micListening') : t('chat.mic')}
            onClick={toggleMic}
            disabled={busy && !listening}
            className={cn(
              'flex size-9 shrink-0 items-center justify-center rounded-full transition-colors',
              'text-muted-foreground hover:text-foreground hover:bg-muted',
              listening && 'text-destructive animate-pulse',
              busy && !listening && 'cursor-not-allowed opacity-50',
            )}
          >
            {listening ? <MicOff className="size-4" /> : <Mic className="size-4" />}
          </button>
        )}
        <AttachmentButton
          onAttached={(a) => setAttachments((cur) => [...cur, a])}
          disabled={busy || attachments.length >= MAX_ATTACHMENTS}
        />
        <Textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKey}
          placeholder={t('chat.placeholder')}
          rows={1}
          className={cn(
            'max-h-40 min-h-9 flex-1 resize-none border-0 bg-transparent px-1 py-2 text-sm shadow-none',
            'focus-visible:ring-0',
          )}
          disabled={busy}
        />
        {busy && onStop ? (
          <button
            type="button"
            aria-label={t('chat.stop')}
            onClick={onStop}
            className="bg-foreground text-background hover:bg-foreground/90 flex size-9 shrink-0 items-center justify-center rounded-full"
          >
            <Square className="size-4" />
          </button>
        ) : (
          <button
            type="button"
            aria-label={t('chat.send')}
            onClick={trySend}
            disabled={!canSend}
            className={cn(
              'bg-foreground text-background hover:bg-foreground/90 flex size-9 shrink-0 items-center justify-center rounded-full transition-colors',
              !canSend && 'bg-muted text-muted-foreground hover:bg-muted cursor-not-allowed',
            )}
          >
            <Send className="size-4" />
          </button>
        )}
      </div>
    </div>
  );
}

function AttachmentPreview({
  attachment,
  onRemove,
}: {
  attachment: Attachment;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    if (attachment.kind !== 'image') return;
    let blobUrl: string | null = null;
    let cancelled = false;
    fetchAttachmentBlob(attachment.id)
      .then((u) => {
        if (cancelled) {
          URL.revokeObjectURL(u);
          return;
        }
        blobUrl = u;
        setUrl(u);
      })
      .catch(() => {
        /* leave placeholder */
      });
    return () => {
      cancelled = true;
      if (blobUrl) URL.revokeObjectURL(blobUrl);
    };
  }, [attachment.id, attachment.kind]);

  return (
    <div className="relative">
      {attachment.kind === 'image' ? (
        <img
          src={url ?? undefined}
          alt=""
          className="bg-muted size-12 rounded-md object-cover"
        />
      ) : (
        <div className="bg-muted text-muted-foreground flex size-12 items-center justify-center rounded-md">
          <FileText className="size-5" />
        </div>
      )}
      <button
        type="button"
        onClick={onRemove}
        aria-label={t('chat.attachRemove')}
        className="bg-foreground text-background absolute -top-1 -right-1 flex size-4 items-center justify-center rounded-full"
      >
        <X className="size-3" />
      </button>
    </div>
  );
}
