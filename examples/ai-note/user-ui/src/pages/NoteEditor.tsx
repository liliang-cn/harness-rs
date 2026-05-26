import { useEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { ArrowLeft, Trash2 } from 'lucide-react';
import {
  MDXEditor,
  type MDXEditorMethods,
  headingsPlugin,
  listsPlugin,
  quotePlugin,
  linkPlugin,
  linkDialogPlugin,
  thematicBreakPlugin,
  markdownShortcutPlugin,
  toolbarPlugin,
  UndoRedo,
  BoldItalicUnderlineToggles,
  BlockTypeSelect,
  ListsToggle,
  CreateLink,
} from '@mdxeditor/editor';
import '@mdxeditor/editor/style.css';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { useSpace } from '@/components/space-context';
import { useConfirm } from '@/components/confirm-dialog';
import { noteApi, type Note } from '@/lib/api';

/** Full-page note editor. Routes: `/app/notes/new` (create) and
 *  `/app/notes/:id` (edit). Replaces the old bottom-sheet modal. */
export function NoteEditor() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams();
  const { space } = useSpace();
  const confirm = useConfirm();
  const isNew = !id;

  const [note, setNote] = useState<Note | null>(null);
  const [loaded, setLoaded] = useState(isNew); // new notes need no fetch
  const [title, setTitle] = useState('');
  const [tags, setTags] = useState('');
  const [busy, setBusy] = useState(false);
  const editorRef = useRef<MDXEditorMethods>(null);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    noteApi
      .note(id)
      .then((j) => {
        if (cancelled) return;
        setNote(j.note);
        setTitle(j.note.title);
        setTags(j.note.tags.join(', '));
        setLoaded(true);
      })
      .catch(() => {
        toast.error(t('notes.empty'));
        navigate('/app');
      });
    return () => {
      cancelled = true;
    };
  }, [id, navigate, t]);

  async function save() {
    const body = (editorRef.current?.getMarkdown() ?? '').trim();
    if (!body) {
      toast.error(t('notes.empty'));
      return;
    }
    setBusy(true);
    const tagArr = tags.split(',').map((s) => s.trim()).filter(Boolean);
    try {
      if (id) await noteApi.updateNote(id, { title, body, tags: tagArr });
      else await noteApi.createNote(space, title, body, tagArr);
      navigate('/app');
    } catch (e) {
      toast.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

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

  if (!loaded) return <Skeleton className="h-64 w-full" />;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="icon-sm" onClick={() => navigate('/app')} aria-label={t('notes.back')}>
          <ArrowLeft className="size-4" />
        </Button>
        <h1 className="flex-1 truncate text-lg font-semibold">
          {isNew ? t('notes.new') : t('notes.edit')}
        </h1>
        {!isNew && (
          <Button variant="ghost" size="icon-sm" onClick={del} aria-label={t('notes.delete')}>
            <Trash2 className="size-4" />
          </Button>
        )}
        <Button onClick={save} disabled={busy}>{t('notes.save')}</Button>
      </div>

      <Input
        placeholder={t('notes.title')}
        value={title}
        onChange={(e) => setTitle(e.target.value)}
      />
      <div className="min-h-[55svh] overflow-y-auto rounded-md border">
        <MDXEditor
          ref={editorRef}
          className="note-md"
          markdown={note?.body ?? ''}
          placeholder={t('notes.body')}
          contentEditableClassName="min-h-40 outline-none"
          plugins={[
            headingsPlugin(),
            listsPlugin(),
            quotePlugin(),
            linkPlugin(),
            linkDialogPlugin(),
            thematicBreakPlugin(),
            markdownShortcutPlugin(),
            toolbarPlugin({
              toolbarContents: () => (
                <>
                  <UndoRedo />
                  <BoldItalicUnderlineToggles />
                  <BlockTypeSelect />
                  <ListsToggle />
                  <CreateLink />
                </>
              ),
            }),
          ]}
        />
      </div>
      <Input
        placeholder={t('notes.tags')}
        value={tags}
        onChange={(e) => setTags(e.target.value)}
      />
    </div>
  );
}
