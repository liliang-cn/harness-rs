import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Trash } from 'lucide-react';
import { toast } from 'sonner';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetTitle,
} from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { ledgerApi, type MemoryEntry } from '@/lib/api';
import { MemoryList } from './memory-list';

interface MemorySheetProps {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  /** Called after entries change so the parent badge can refetch the count. */
  onChanged?: () => void;
}

/**
 * Right-side sheet (same width pattern as ChatSheet) that lists every
 * memory entry for the signed-in user, with per-row delete and a
 * "Clear all" button gated by window.confirm. Lazy-loads on open so we
 * don't fetch memory on every Profile render.
 */
export function MemorySheet({ open, onOpenChange, onChanged }: MemorySheetProps) {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<MemoryEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [clearing, setClearing] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const r = await ledgerApi.memories();
      setEntries(r.memories);
      onChanged?.();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }, [t, onChanged]);

  useEffect(() => {
    if (open) {
      reload();
    } else {
      // Drop stale state when the sheet closes so the next open starts
      // with a fresh skeleton instead of flashing previous entries.
      setEntries(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  async function handleClearAll() {
    if (!window.confirm(t('profile.memoryClearConfirm'))) return;
    setClearing(true);
    try {
      const r = await ledgerApi.clearMemories();
      toast.success(t('profile.memoryCleared', { count: r.deleted }));
      await reload();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setClearing(false);
    }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="right"
        showCloseButton={false}
        className="flex w-full flex-col gap-0 p-0 sm:max-w-md"
      >
        <SheetTitle className="sr-only">{t('profile.memory')}</SheetTitle>
        <SheetDescription className="sr-only">
          {t('profile.memory')}
        </SheetDescription>
        <div className="border-border flex h-12 items-center gap-2 border-b px-3">
          <div className="min-w-0 flex-1 text-sm font-medium">
            {t('profile.memory')}
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={handleClearAll}
            disabled={
              clearing || loading || !entries || entries.length === 0
            }
          >
            <Trash className="size-4" />
            {t('profile.memoryClear')}
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label="close"
            onClick={() => onOpenChange(false)}
          >
            <span aria-hidden>×</span>
          </Button>
        </div>
        <div className="flex-1 overflow-y-auto px-4 py-3">
          {loading || entries === null ? (
            <div className="space-y-3">
              <Skeleton className="h-14 w-full" />
              <Skeleton className="h-14 w-full" />
              <Skeleton className="h-14 w-full" />
            </div>
          ) : (
            <MemoryList entries={entries} onChanged={reload} />
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
