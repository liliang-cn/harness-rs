import { useCallback, useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Target, ChevronRight, Sparkles } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useSpace } from '@/components/space-context';
import { openChatWith } from '@/lib/chat-prefill';
import { noteApi, type Goal } from '@/lib/api';

export function Plans() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const navigate = useNavigate();
  const [goals, setGoals] = useState<Goal[] | null>(null);

  const load = useCallback(() => {
    setGoals(null);
    noteApi.goals(space, 'all').then((j) => setGoals(j.goals)).catch(() => setGoals([]));
  }, [space]);
  useEffect(load, [load]);

  const now = Date.now();
  const active = (goals ?? []).filter((g) => g.status === 'active' && !g.parent_id);
  const due = active.filter((g) => g.kind === 'goal' && g.next_review_at && Date.parse(g.next_review_at) <= now);
  const topGoals = active.filter((g) => g.kind === 'goal');
  const rules = active.filter((g) => g.kind === 'rule');

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">{t('plans.title')}</h1>
        <Button variant="outline" onClick={() => openChatWith('我想定一个新目标：')}>
          <Sparkles className="size-4" /> {t('plans.addGoal')}
        </Button>
      </div>

      {goals === null ? (
        <div className="space-y-2"><Skeleton className="h-16 w-full" /><Skeleton className="h-16 w-full" /></div>
      ) : active.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('plans.empty')}</p>
      ) : (
        <>
          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.due')}</h2>
            {due.length === 0 ? (
              <p className="text-muted-foreground text-sm">{t('plans.noDue')}</p>
            ) : due.map((g) => (
              <Card key={g.id} className="flex flex-row items-center justify-between gap-2 p-3">
                <button className="min-w-0 flex-1 text-left" onClick={() => navigate(`/app/plans/${g.id}`)}>
                  <div className="truncate text-sm font-medium">{g.title}</div>
                </button>
                <Button size="sm" onClick={() => openChatWith(`复盘：${g.title}`)}>{t('plans.review')}</Button>
              </Card>
            ))}
          </section>

          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.goals')}</h2>
            {topGoals.map((g) => (
              <Card key={g.id} onClick={() => navigate(`/app/plans/${g.id}`)} className="hover:bg-accent flex flex-row cursor-pointer items-center gap-2 p-3">
                <Target className="text-muted-foreground size-4 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">{g.title}</div>
                  {g.target_date && (
                    <div className="text-muted-foreground text-xs">
                      {t('plans.targetDate')}: {format(parseISO(g.target_date), 'yyyy-MM-dd')}
                    </div>
                  )}
                </div>
                <ChevronRight className="text-muted-foreground size-4" />
              </Card>
            ))}
          </section>

          {rules.length > 0 && (
            <section className="space-y-2">
              <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.rules')}</h2>
              {rules.map((g) => (
                <Card key={g.id} onClick={() => navigate(`/app/plans/${g.id}`)} className="hover:bg-accent cursor-pointer p-3 text-sm">
                  {g.title}
                </Card>
              ))}
            </section>
          )}
        </>
      )}
    </div>
  );
}
