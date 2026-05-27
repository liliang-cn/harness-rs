import { useCallback, useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { format, parseISO } from 'date-fns';
import { ArrowLeft, Check, Trash2, Sparkles, Plus, FileText, ChevronRight } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { renderMarkdown } from '@/lib/markdown';
import { openChatWith } from '@/lib/chat-prefill';
import { useConfirm } from '@/components/confirm-dialog';
import { ledgerApi, type Project, type ProjectReview, type Note } from '@/lib/api';

/** Project detail page. Route: `/app/projects/:id`. */
export function ProjectView() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams();
  const confirm = useConfirm();
  const [data, setData] = useState<{
    project: Project;
    milestones: Project[];
    reviews: ProjectReview[];
  } | null>(null);
  const [notes, setNotes] = useState<Note[]>([]);

  const load = useCallback(() => {
    if (!id) return;
    ledgerApi
      .project(id)
      .then(setData)
      .catch(() => {
        toast.error(t('project.empty'));
        navigate('/app/projects');
      });
    ledgerApi
      .notes(id)
      .then((r) => setNotes(r.notes))
      .catch(() => setNotes([]));
  }, [id, navigate, t]);

  useEffect(() => {
    load();
  }, [load]);

  async function toggleMilestone(ms: Project) {
    const next = ms.status === 'done' ? 'active' : 'done';
    await ledgerApi.updateProject(ms.id, { status: next });
    load();
  }

  async function markDone() {
    if (!id) return;
    await ledgerApi.updateProject(id, { status: 'done' });
    toast.success(t('project.done'));
    navigate('/app/projects');
  }

  async function del() {
    if (!id) return;
    if (!(await confirm({ title: t('project.deleteConfirm'), destructive: true }))) return;
    await ledgerApi.deleteProject(id);
    navigate('/app/projects');
  }

  if (!data) return <Skeleton className="h-64 w-full" />;
  const { project, milestones, reviews } = data;

  return (
    <div className="space-y-5">
      <div className="flex items-center gap-2">
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={() => navigate('/app/projects')}
          aria-label={t('notes.back')}
        >
          <ArrowLeft className="size-4" />
        </Button>
        <h1 className="flex-1 truncate text-xl font-semibold">{project.name}</h1>
      </div>

      {project.target_date && (
        <div className="text-muted-foreground text-xs">
          {t('project.targetDate')}: {format(parseISO(project.target_date), 'yyyy-MM-dd')}
        </div>
      )}
      {project.detail.trim() && (
        <div
          className="markdown-body text-sm"
          dangerouslySetInnerHTML={{ __html: renderMarkdown(project.detail) }}
        />
      )}

      {/* Milestones */}
      <section className="space-y-2">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-medium">{t('project.milestones')}</h3>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => openChatWith(`把「${project.name}」拆解一下`)}
          >
            <Sparkles className="size-3.5" /> {t('project.addMilestone')}
          </Button>
        </div>
        {milestones.length === 0 ? (
          <p className="text-muted-foreground text-xs">—</p>
        ) : (
          milestones.map((ms) => (
            <button
              key={ms.id}
              onClick={() => toggleMilestone(ms)}
              className="hover:bg-accent flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm"
            >
              <span
                className={`flex size-4 items-center justify-center rounded border ${
                  ms.status === 'done' ? 'bg-primary text-primary-foreground' : ''
                }`}
              >
                {ms.status === 'done' && <Check className="size-3" />}
              </span>
              <span className={ms.status === 'done' ? 'text-muted-foreground line-through' : ''}>
                {ms.name}
              </span>
            </button>
          ))
        )}
      </section>

      {/* Reviews */}
      <section className="space-y-2">
        <h3 className="text-sm font-medium">{t('project.reviews')}</h3>
        {reviews.length === 0 ? (
          <p className="text-muted-foreground text-xs">—</p>
        ) : (
          reviews.map((rv) => (
            <div key={rv.id} className="border-border rounded-md border p-2 text-sm">
              <div className="text-muted-foreground mb-1 text-[11px]">
                {format(parseISO(rv.created_at), 'yyyy-MM-dd HH:mm')}
              </div>
              <div className="whitespace-pre-wrap">{rv.progress}</div>
              {rv.next_steps.trim() && (
                <div className="text-muted-foreground mt-1 whitespace-pre-wrap">
                  → {rv.next_steps}
                </div>
              )}
            </div>
          ))
        )}
      </section>

      {/* Notes */}
      <section className="space-y-2">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-medium">{t('project.notes')}</h3>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => navigate(`/app/notes/new?project=${id}`)}
          >
            <Plus className="size-3.5" /> {t('project.addNote')}
          </Button>
        </div>
        {notes.length === 0 ? (
          <p className="text-muted-foreground text-xs">—</p>
        ) : (
          notes.map((n) => (
            <Card
              key={n.id}
              onClick={() => navigate(`/app/notes/${n.id}`)}
              className="hover:bg-accent flex flex-row cursor-pointer items-center gap-2 p-3"
            >
              <FileText className="text-muted-foreground size-4 shrink-0" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm font-medium">
                  {n.title?.trim() || n.body.slice(0, 50)}
                </div>
                <div className="text-muted-foreground text-xs">
                  {format(parseISO(n.updated_at), 'yyyy-MM-dd')}
                </div>
              </div>
              <ChevronRight className="text-muted-foreground size-4 shrink-0" />
            </Card>
          ))
        )}
      </section>

      {/* Actions */}
      <div className="flex gap-2 pt-2">
        <Button onClick={() => openChatWith(`复盘：${project.name}`)}>
          {t('project.review')}
        </Button>
        <Button variant="outline" onClick={markDone}>
          <Check className="size-4" /> {t('project.markDone')}
        </Button>
        <Button variant="ghost" size="icon" onClick={del} aria-label={t('project.delete')}>
          <Trash2 className="size-4" />
        </Button>
      </div>
    </div>
  );
}
