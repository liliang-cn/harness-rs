import { useTranslation } from 'react-i18next';
import { Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { ledgerApi, type MemoryEntry } from '@/lib/api';

interface MemoryListProps {
  entries: MemoryEntry[];
  /** Called after a successful per-row delete so the parent can refetch. */
  onChanged: () => void;
}

/**
 * Scrollable memory list. Each row shows `content` + an optional source
 * badge + the created_ms timestamp + a trash icon. Window.confirm gates
 * the destructive action since these are real user-owned facts.
 */
export function MemoryList({ entries, onChanged }: MemoryListProps) {
  const { t, i18n } = useTranslation();

  if (entries.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        {t('profile.memoryEmpty')}
      </p>
    );
  }

  async function handleDelete(id: string) {
    if (!window.confirm(t('profile.memoryDeleteConfirm'))) return;
    try {
      await ledgerApi.deleteMemory(id);
      onChanged();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    }
  }

  return (
    <ul className="divide-border divide-y">
      {entries.map((m) => {
        const when = m.created_ms
          ? new Date(m.created_ms).toLocaleString(i18n.language, {
              year: 'numeric',
              month: 'short',
              day: 'numeric',
              hour: '2-digit',
              minute: '2-digit',
            })
          : '';
        return (
          <li
            key={m.id}
            className="flex items-start gap-3 py-3 first:pt-0 last:pb-0"
          >
            <div className="min-w-0 flex-1">
              <div className="text-sm break-words whitespace-pre-wrap">
                {m.content}
              </div>
              <div className="text-muted-foreground mt-1 flex flex-wrap items-center gap-2 text-xs">
                {m.source ? (
                  <Badge variant="outline" className="font-normal">
                    {m.source}
                  </Badge>
                ) : null}
                {when ? <span>{when}</span> : null}
              </div>
            </div>
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={t('profile.memoryDeleteConfirm')}
              onClick={() => handleDelete(m.id)}
            >
              <Trash2 className="size-4" />
            </Button>
          </li>
        );
      })}
    </ul>
  );
}
