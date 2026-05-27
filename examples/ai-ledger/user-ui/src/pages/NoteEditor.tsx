import { useEffect, useRef, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { ArrowLeft } from 'lucide-react';
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
import { ledgerApi, type Note } from '@/lib/api';

/** Full-page note editor. Routes: `/app/notes/new` (create) and
 *  `/app/notes/:id/edit` (edit). */
export function NoteEditor() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { id } = useParams();
  const [searchParams] = useSearchParams();
  // `?project=<id>` when creating a note from a project view
  const projectId = searchParams.get('project') ?? undefined;
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
    ledgerApi
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
        navigate('/app/projects');
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
    const tagArr = tags
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean);
    try {
      if (id) {
        await ledgerApi.updateNote(id, { title, body, tags: tagArr });
        navigate(`/app/notes/${id}`);
      } else {
        const { note: created } = await ledgerApi.createNote({
          title,
          body,
          tags: tagArr,
          project_id: projectId,
        });
        navigate(`/app/notes/${created.id}`);
      }
    } catch (e) {
      toast.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  }

  if (!loaded) return <Skeleton className="h-64 w-full" />;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={() => {
            if (id) {
              navigate(`/app/notes/${id}`);
            } else if (projectId) {
              navigate(`/app/projects/${projectId}`);
            } else {
              navigate('/app/projects');
            }
          }}
          aria-label={t('notes.back')}
        >
          <ArrowLeft className="size-4" />
        </Button>
        <h1 className="flex-1 truncate text-lg font-semibold">
          {isNew ? t('notes.new') : t('notes.edit')}
        </h1>
        <Button onClick={save} disabled={busy}>
          {t('notes.save')}
        </Button>
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
