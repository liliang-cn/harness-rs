import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RotateCw, X, Loader2 } from 'lucide-react';
import { Sheet, SheetContent, SheetTitle, SheetDescription } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { fetchArtifactData, type ArtifactSpec } from '@/lib/artifact';
import { buildSrcdoc } from '@/lib/artifact-sandbox';

interface Props {
  spec: ArtifactSpec;
  open: boolean;
  onOpenChange: (v: boolean) => void;
}

/** Full-screen sandboxed preview. Fetches data host-side (with the token),
 *  renders the AI code in an opaque-origin iframe, and injects the data via
 *  postMessage once the sandbox signals ready. */
export function ArtifactView({ spec, open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  const [srcdoc, setSrcdoc] = useState<string | null>(null);
  const [data, setData] = useState<unknown>(null);
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading');
  const [errorMsg, setErrorMsg] = useState('');

  const load = useCallback(async () => {
    setStatus('loading');
    setErrorMsg('');
    try {
      const d = await fetchArtifactData(spec);
      setData(d);
      // buildSrcdoc transpiles; it can throw on bad code → caught here.
      const doc = buildSrcdoc(spec.code);
      setSrcdoc(doc);
      setStatus('ready');
    } catch (e) {
      setErrorMsg((e as Error).message || 'failed to render');
      setStatus('error');
    }
  }, [spec]);

  useEffect(() => {
    if (open) load();
  }, [open, load]);

  // Receive ready/error signals from the sandbox; inject data on ready.
  useEffect(() => {
    function onMsg(e: MessageEvent) {
      if (e.source !== iframeRef.current?.contentWindow) return;
      const m = e.data;
      if (m?.type === 'artifact-ready') {
        iframeRef.current?.contentWindow?.postMessage({ type: 'artifact-data', data }, '*');
      } else if (m?.type === 'artifact-error') {
        setErrorMsg(String(m.message));
        setStatus('error');
      }
    }
    window.addEventListener('message', onMsg);
    return () => window.removeEventListener('message', onMsg);
  }, [data]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="right" showCloseButton={false} className="flex w-full flex-col gap-0 p-0 sm:max-w-full">
        <SheetTitle className="sr-only">{spec.title}</SheetTitle>
        <SheetDescription className="sr-only">{spec.title}</SheetDescription>
        <div className="border-border flex h-12 items-center gap-2 border-b px-3">
          <span className="min-w-0 flex-1 truncate text-sm font-medium">{spec.title}</span>
          <Button variant="ghost" size="icon-sm" aria-label={t('artifact.refresh')} onClick={load}>
            <RotateCw className="size-4" />
          </Button>
          <Button variant="ghost" size="icon-sm" aria-label={t('chat.close', { defaultValue: 'close' })} onClick={() => onOpenChange(false)}>
            <X className="size-4" />
          </Button>
        </div>
        <div className="relative flex-1 overflow-hidden">
          {status === 'loading' && (
            <div className="text-muted-foreground absolute inset-0 flex items-center justify-center gap-2 text-sm">
              <Loader2 className="size-4 animate-spin" /> {t('common.loading')}
            </div>
          )}
          {status === 'error' && (
            <div className="absolute inset-0 overflow-auto p-4">
              <p className="text-destructive mb-2 text-sm font-medium">{t('artifact.error')}</p>
              <pre className="text-muted-foreground text-xs whitespace-pre-wrap">{errorMsg}</pre>
            </div>
          )}
          {status === 'ready' && srcdoc && (
            <iframe
              ref={iframeRef}
              title={spec.title}
              sandbox="allow-scripts"
              srcDoc={srcdoc}
              className="h-full w-full border-0 bg-white"
            />
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
