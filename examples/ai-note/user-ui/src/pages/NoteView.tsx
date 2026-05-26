import { useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { ArrowLeft, Pencil, Trash2 } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { useConfirm } from '@/components/confirm-dialog';
import { renderMarkdown } from '@/lib/markdown';
import { noteApi, type Note } from '@/lib/api';

/** Read-only note view (rendered markdown). Route: `/app/notes/:id`.
 *  Edit → `/app/notes/:id/edit`. */
export function NoteView() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams();
  const confirm = useConfirm();
  const [note, setNote] = useState<Note | null>(null);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    noteApi
      .note(id)
      .then((j) => { if (!cancelled) setNote(j.note); })
      .catch(() => { toast.error(t('notes.empty')); navigate('/app'); });
    return () => { cancelled = true; };
  }, [id, navigate, t]);

  async function del() {
    if (!id) return;
    if (!(await confirm({ title: t('notes.deleteConfirm'), destructive: true }))) return;
    try {
      await noteApi.deleteNote(id);
      navigate('/app');
    } catch (e) {
      toast.error((e as Error).message);
    }
  }

  if (!note) return <Skeleton className="h-64 w-full" />;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="icon-sm" onClick={() => navigate('/app')} aria-label={t('notes.back')}>
          <ArrowLeft className="size-4" />
        </Button>
        <div className="flex-1" />
        <Button variant="outline" size="sm" onClick={() => navigate(`/app/notes/${id}/edit`)}>
          <Pencil className="size-4" /> {t('notes.editAction')}
        </Button>
        <Button variant="ghost" size="icon-sm" onClick={del} aria-label={t('notes.delete')}>
          <Trash2 className="size-4" />
        </Button>
      </div>

      <div>
        <h1 className="text-2xl font-semibold break-words">
          {note.title?.trim() || note.body.slice(0, 40)}
        </h1>
        <div className="text-muted-foreground mt-1.5 flex flex-wrap items-center gap-1.5 text-xs">
          {note.tags.map((tg) => (
            <span key={tg} className="bg-secondary rounded px-1.5 py-0.5">{tg}</span>
          ))}
          <span>{format(parseISO(note.updated_at), 'yyyy-MM-dd HH:mm')}</span>
        </div>
      </div>

      <article
        className="markdown-body text-sm"
        dangerouslySetInnerHTML={{ __html: renderMarkdown(note.body) }}
      />
    </div>
  );
}
