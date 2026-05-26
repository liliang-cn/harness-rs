import { useCallback, useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { format, parseISO } from 'date-fns';
import { ArrowLeft, Check, Trash2, Sparkles } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { renderMarkdown } from '@/lib/markdown';
import { openChatWith } from '@/lib/chat-prefill';
import { useConfirm } from '@/components/confirm-dialog';
import { noteApi, type Goal, type GoalReview } from '@/lib/api';

/** Goal detail as a full page. Route: `/app/plans/:id`. Replaces the old
 *  bottom-sheet modal. Authoring (decompose / review) stays NL-first via chat. */
export function GoalView() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams();
  const confirm = useConfirm();
  const [data, setData] = useState<{ goal: Goal; subgoals: Goal[]; reviews: GoalReview[] } | null>(null);

  const load = useCallback(() => {
    if (!id) return;
    noteApi
      .goal(id)
      .then(setData)
      .catch(() => { toast.error(t('plans.empty')); navigate('/app/plans'); });
  }, [id, navigate, t]);
  useEffect(() => { load(); }, [load]);

  async function toggleSub(sg: Goal) {
    const next = sg.status === 'done' ? 'active' : 'done';
    await noteApi.updateGoal(sg.id, { status: next });
    load();
  }
  async function markDone() {
    if (!id) return;
    await noteApi.updateGoal(id, { status: 'done' });
    toast.success(t('plans.done'));
    navigate('/app/plans');
  }
  async function del() {
    if (!id) return;
    if (!(await confirm({ title: t('plans.deleteConfirm'), destructive: true }))) return;
    await noteApi.deleteGoal(id);
    navigate('/app/plans');
  }

  if (!data) return <Skeleton className="h-64 w-full" />;
  const { goal, subgoals, reviews } = data;

  return (
    <div className="space-y-5">
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="icon-sm" onClick={() => navigate('/app/plans')} aria-label={t('notes.back')}>
          <ArrowLeft className="size-4" />
        </Button>
        <h1 className="flex-1 truncate text-xl font-semibold">{goal.title}</h1>
      </div>

      {goal.target_date && (
        <div className="text-muted-foreground text-xs">
          {t('plans.targetDate')}: {format(parseISO(goal.target_date), 'yyyy-MM-dd')}
        </div>
      )}
      {goal.detail.trim() && (
        <div
          className="markdown-body text-sm"
          dangerouslySetInnerHTML={{ __html: renderMarkdown(goal.detail) }}
        />
      )}

      <section className="space-y-2">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-medium">{t('plans.subgoals')}</h3>
          <Button variant="ghost" size="sm" onClick={() => openChatWith(`把「${goal.title}」拆解一下`)}>
            <Sparkles className="size-3.5" /> {t('plans.addSubgoal')}
          </Button>
        </div>
        {subgoals.length === 0 ? (
          <p className="text-muted-foreground text-xs">—</p>
        ) : subgoals.map((sg) => (
          <button
            key={sg.id}
            onClick={() => toggleSub(sg)}
            className="hover:bg-accent flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm"
          >
            <span className={`flex size-4 items-center justify-center rounded border ${sg.status === 'done' ? 'bg-primary text-primary-foreground' : ''}`}>
              {sg.status === 'done' && <Check className="size-3" />}
            </span>
            <span className={sg.status === 'done' ? 'text-muted-foreground line-through' : ''}>{sg.title}</span>
          </button>
        ))}
      </section>

      <section className="space-y-2">
        <h3 className="text-sm font-medium">{t('plans.reviews')}</h3>
        {reviews.length === 0 ? (
          <p className="text-muted-foreground text-xs">—</p>
        ) : reviews.map((rv) => (
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
        <Button onClick={() => openChatWith(`复盘：${goal.title}`)}>{t('plans.review')}</Button>
        <Button variant="outline" onClick={markDone}><Check className="size-4" /> {t('plans.markDone')}</Button>
        <Button variant="ghost" size="icon" onClick={del} aria-label={t('plans.delete')}>
          <Trash2 className="size-4" />
        </Button>
      </div>
    </div>
  );
}
