import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { format, parseISO } from 'date-fns';
import { Check, Trash2, Sparkles } from 'lucide-react';
import { toast } from 'sonner';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { renderMarkdown } from '@/lib/markdown';
import { openChatWith } from '@/lib/chat-prefill';
import { noteApi, type Goal, type GoalReview } from '@/lib/api';

export function GoalDetail({
  id, open, onOpenChange, onChanged,
}: {
  id: string; open: boolean; onOpenChange: (v: boolean) => void; onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [data, setData] = useState<{ goal: Goal; subgoals: Goal[]; reviews: GoalReview[] } | null>(null);

  const load = useCallback(() => {
    setData(null);
    noteApi.goal(id).then(setData).catch(() => setData(null));
  }, [id]);
  useEffect(() => { if (open) load(); }, [open, load]);

  async function toggleSub(sg: Goal) {
    const next = sg.status === 'done' ? 'active' : 'done';
    await noteApi.updateGoal(sg.id, { status: next });
    load();
  }
  async function markDone() {
    await noteApi.updateGoal(id, { status: 'done' });
    toast.success(t('plans.done'));
    onOpenChange(false); onChanged();
  }
  async function del() {
    if (!confirm(t('plans.deleteConfirm'))) return;
    await noteApi.deleteGoal(id);
    onOpenChange(false); onChanged();
  }

  const goal = data?.goal;

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="bottom" className="flex h-[90svh] flex-col">
        <SheetHeader>
          <SheetTitle>{goal?.title ?? '…'}</SheetTitle>
        </SheetHeader>
        <div className="flex-1 space-y-5 overflow-y-auto px-4 pb-4">
          {!data ? (
            <Skeleton className="h-24 w-full" />
          ) : (
            <>
              {goal!.target_date && (
                <div className="text-muted-foreground text-xs">
                  {t('plans.targetDate')}: {format(parseISO(goal!.target_date), 'yyyy-MM-dd')}
                </div>
              )}
              {goal!.detail.trim() && (
                <div className="markdown-body text-sm"
                     dangerouslySetInnerHTML={{ __html: renderMarkdown(goal!.detail) }} />
              )}

              <section className="space-y-2">
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-medium">{t('plans.subgoals')}</h3>
                  <Button variant="ghost" size="sm" onClick={() => openChatWith(`把「${goal!.title}」拆解一下`)}>
                    <Sparkles className="size-3.5" /> {t('plans.addSubgoal')}
                  </Button>
                </div>
                {data.subgoals.length === 0 ? (
                  <p className="text-muted-foreground text-xs">—</p>
                ) : data.subgoals.map((sg) => (
                  <button key={sg.id} onClick={() => toggleSub(sg)}
                    className="hover:bg-accent flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm">
                    <span className={`flex size-4 items-center justify-center rounded border ${sg.status === 'done' ? 'bg-primary text-primary-foreground' : ''}`}>
                      {sg.status === 'done' && <Check className="size-3" />}
                    </span>
                    <span className={sg.status === 'done' ? 'text-muted-foreground line-through' : ''}>{sg.title}</span>
                  </button>
                ))}
              </section>

              <section className="space-y-2">
                <h3 className="text-sm font-medium">{t('plans.reviews')}</h3>
                {data.reviews.length === 0 ? (
                  <p className="text-muted-foreground text-xs">—</p>
                ) : data.reviews.map((rv) => (
                  <div key={rv.id} className="border-border rounded-md border p-2 text-sm">
                    <div className="text-muted-foreground mb-1 text-[11px]">
                      {format(parseISO(rv.created_at), 'yyyy-MM-dd HH:mm')}
                    </div>
                    <div className="whitespace-pre-wrap">{rv.progress}</div>
                    {rv.next_steps.trim() && (
                      <div className="text-muted-foreground mt-1 whitespace-pre-wrap">→ {rv.next_steps}</div>
                    )}
                  </div>
                ))}
              </section>

              <div className="flex gap-2 pt-2">
                <Button onClick={() => openChatWith(`复盘：${goal!.title}`)}>{t('plans.review')}</Button>
                <Button variant="outline" onClick={markDone}><Check className="size-4" /> {t('plans.markDone')}</Button>
                <Button variant="ghost" size="icon" onClick={del} aria-label={t('plans.delete')}>
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
