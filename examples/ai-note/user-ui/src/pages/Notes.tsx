import { lazy, Suspense, useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, Trash2 } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useSpace } from '@/components/space-context';
import { useConfirm } from '@/components/confirm-dialog';
import { noteApi, type Note } from '@/lib/api';

// The WYSIWYG editor (MDXEditor) is heavy — only load its chunk when the user
// actually opens a note. `everOpened` keeps it mounted after the first open so
// the Sheet close animation still plays.
const NoteEditor = lazy(() =>
  import('@/components/notes/note-editor').then((m) => ({ default: m.NoteEditor })),
);

export function Notes() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const confirm = useConfirm();
  const [notes, setNotes] = useState<Note[] | null>(null);
  const [editing, setEditing] = useState<Note | null>(null);
  const [open, setOpen] = useState(false);
  const [everOpened, setEverOpened] = useState(false);

  function openEditor(note: Note | null) {
    setEditing(note);
    setEverOpened(true);
    setOpen(true);
  }

  const load = useCallback(() => {
    setNotes(null);
    noteApi.notes(space).then((j) => setNotes(j.notes)).catch(() => setNotes([]));
  }, [space]);
  useEffect(load, [load]);

  async function del(e: React.MouseEvent, id: string) {
    e.stopPropagation();
    if (!(await confirm({ title: t('notes.deleteConfirm'), destructive: true }))) return;
    try { await noteApi.deleteNote(id); setNotes((c) => c?.filter((n) => n.id !== id) ?? null); toast.success(t('notes.deleted')); }
    catch (err) { toast.error((err as Error).message); }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">{t('nav.notes')}</h1>
        <Button onClick={() => openEditor(null)}>
          <Plus className="size-4" /> {t('notes.new')}
        </Button>
      </div>
      {notes === null ? (
        <div className="space-y-2"><Skeleton className="h-20 w-full" /><Skeleton className="h-20 w-full" /></div>
      ) : notes.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('notes.empty')}</p>
      ) : (
        <div className="space-y-2">
          {notes.map((n) => (
            <Card key={n.id} onClick={() => openEditor(n)}
              className="hover:bg-accent cursor-pointer p-3">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">{n.title?.trim() || n.body.slice(0, 40)}</div>
                  <div className="text-muted-foreground mt-1 line-clamp-2 text-xs">{n.body}</div>
                  <div className="text-muted-foreground mt-1.5 flex flex-wrap items-center gap-1.5 text-[11px]">
                    {n.tags.map((tg) => <span key={tg} className="bg-secondary rounded px-1.5 py-0.5">{tg}</span>)}
                    <span>{format(parseISO(n.updated_at), 'yyyy-MM-dd HH:mm')}</span>
                  </div>
                </div>
                <Button variant="ghost" size="icon-sm" onClick={(e) => del(e, n.id)} aria-label="delete">
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </Card>
          ))}
        </div>
      )}
      {everOpened && (
        <Suspense fallback={null}>
          <NoteEditor open={open} onOpenChange={setOpen} space={space} note={editing} onSaved={load} />
        </Suspense>
      )}
    </div>
  );
}
