import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Download, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { ModelPicker } from '@/components/profile/model-picker';
import { noteApi, getToken, type Memory } from '@/lib/api';

function MemoryCard() {
  const { t } = useTranslation();
  const [mems, setMems] = useState<Memory[] | null>(null);
  const load = useCallback(() => {
    noteApi.memories().then((j) => setMems(j.memories)).catch(() => setMems([]));
  }, []);
  useEffect(load, [load]);
  async function forget(id: string) {
    try { await noteApi.forgetMemory(id); setMems((c) => c?.filter((m) => m.id !== id) ?? null); toast.success(t('profile.memory.deleted')); }
    catch (e) { toast.error((e as Error).message); }
  }
  async function clearAll() {
    if (!confirm(t('profile.memory.clearConfirm'))) return;
    try { await noteApi.clearMemories(); setMems([]); toast.success(t('profile.memory.deleted')); }
    catch (e) { toast.error((e as Error).message); }
  }
  return (
    <Card className="space-y-2 p-4">
      <div className="flex items-center justify-between">
        <div className="text-sm font-medium">{t('profile.memory.title')}</div>
        {mems && mems.length > 0 && (
          <Button variant="ghost" size="sm" onClick={clearAll}>{t('profile.memory.clear')}</Button>
        )}
      </div>
      {mems === null ? (
        <Skeleton className="h-12 w-full" />
      ) : mems.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t('profile.memory.empty')}</p>
      ) : (
        <ul className="space-y-1.5">
          {mems.map((m) => (
            <li key={m.id} className="flex items-start justify-between gap-2 text-sm">
              <div className="min-w-0 flex-1">
                <div className="break-words">{m.content}</div>
                <div className="text-muted-foreground text-[11px]">
                  {new Date(m.created_ms).toLocaleDateString()}
                </div>
              </div>
              <Button variant="ghost" size="icon-sm" aria-label="forget" onClick={() => forget(m.id)}>
                <Trash2 className="size-4" />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

export function Profile() {
  const { t } = useTranslation();
  const [user, setUser] = useState<any>(null);
  useEffect(() => { noteApi.me().then((j) => setUser(j.user)).catch(() => {}); }, []);

  async function exportZip() {
    const resp = await fetch('/api/notes/export.zip', { headers: { Authorization: `Bearer ${getToken() ?? ''}` } });
    const blob = await resp.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob); a.download = 'notes.zip'; a.click();
    URL.revokeObjectURL(a.href);
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('nav.profile')}</h1>
      <Card className="space-y-1 p-4">
        <div className="text-sm">{user?.email}</div>
        <div className="text-muted-foreground text-xs">{user?.tier}</div>
      </Card>
      <Card className="p-4">
        <ModelPicker tier={user?.tier ?? 'trial'} current={user?.preferred_model} />
      </Card>
      <MemoryCard />
      <Button variant="outline" onClick={exportZip}>
        <Download className="size-4" /> {t('profile.export')}
      </Button>
    </div>
  );
}
