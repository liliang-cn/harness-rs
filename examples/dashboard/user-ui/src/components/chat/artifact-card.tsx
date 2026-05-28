import { useState, lazy, Suspense } from 'react';
import { useTranslation } from 'react-i18next';
import { LayoutDashboard, Maximize2 } from 'lucide-react';
import type { ArtifactSpec } from '@/lib/artifact';

const ArtifactView = lazy(() =>
  import('@/components/chat/artifact-view').then((m) => ({ default: m.ArtifactView })),
);

/** Compact card shown in an assistant bubble; opens the full-screen preview. */
export function ArtifactCard({ spec }: { spec: ArtifactSpec }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="border-border hover:bg-accent mt-2 flex w-full items-center gap-2 rounded-lg border p-2.5 text-left"
      >
        <LayoutDashboard className="text-muted-foreground size-4 shrink-0" />
        <span className="min-w-0 flex-1 truncate text-sm font-medium">{spec.title}</span>
        <span className="text-muted-foreground flex items-center gap-1 text-xs">
          <Maximize2 className="size-3.5" /> {t('artifact.open')}
        </span>
      </button>
      {open && (
        <Suspense fallback={null}>
          <ArtifactView spec={spec} open={open} onOpenChange={setOpen} />
        </Suspense>
      )}
    </>
  );
}
