import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Mic, MicOff, Send, Square } from 'lucide-react';
import { Textarea } from '@/components/ui/textarea';
import { cn } from '@/lib/utils';

interface ComposerProps {
  onSend: (text: string) => void;
  onStop?: () => void;
  busy: boolean;
}

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
    if (!v || busy) return;
    onSend(v);
    setText('');
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

  return (
    <div className="border-border bg-background sticky bottom-0 border-t p-3">
      {/* Pill container — mic / textarea / send sit on the same baseline
          inside one rounded border. No mismatched standalone buttons. */}
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
            disabled={busy || !text.trim()}
            className={cn(
              'bg-foreground text-background hover:bg-foreground/90 flex size-9 shrink-0 items-center justify-center rounded-full transition-colors',
              (busy || !text.trim()) && 'bg-muted text-muted-foreground hover:bg-muted cursor-not-allowed',
            )}
          >
            <Send className="size-4" />
          </button>
        )}
      </div>
    </div>
  );
}
