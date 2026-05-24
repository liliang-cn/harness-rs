import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Mic, MicOff, Send, Square } from 'lucide-react';
import { Button } from '@/components/ui/button';
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
      <div className="flex items-end gap-2">
        {SpeechCtor && (
          <Button
            type="button"
            size="icon"
            variant={listening ? 'destructive' : 'outline'}
            aria-label={listening ? t('chat.micListening') : t('chat.mic')}
            onClick={toggleMic}
            disabled={busy && !listening}
            className={cn(listening && 'animate-pulse')}
          >
            {listening ? <MicOff className="size-4" /> : <Mic className="size-4" />}
          </Button>
        )}
        <Textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKey}
          placeholder={t('chat.placeholder')}
          rows={1}
          className="max-h-40 min-h-10 flex-1 resize-none"
          disabled={busy}
        />
        {busy && onStop ? (
          <Button
            type="button"
            size="icon"
            variant="outline"
            aria-label={t('chat.stop')}
            onClick={onStop}
          >
            <Square className="size-4" />
          </Button>
        ) : (
          <Button
            type="button"
            size="icon"
            aria-label={t('chat.send')}
            onClick={trySend}
            disabled={busy || !text.trim()}
          >
            <Send className="size-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
