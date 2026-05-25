import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
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
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetFooter } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { noteApi, type Note, type Space } from '@/lib/api';

export function NoteEditor({
  open, onOpenChange, space, note, onSaved,
}: {
  open: boolean; onOpenChange: (v: boolean) => void;
  space: Space; note: Note | null; onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState('');
  const [tags, setTags] = useState('');
  const [busy, setBusy] = useState(false);
  // MDXEditor is uncontrolled: seed it with `markdown` at mount and read the
  // current value via the ref at save time. The `key` below remounts it when
  // switching notes so the seed refreshes.
  const editorRef = useRef<MDXEditorMethods>(null);

  useEffect(() => {
    if (open) {
      setTitle(note?.title ?? '');
      setTags(note?.tags.join(', ') ?? '');
    }
  }, [open, note]);

  async function save() {
    const body = (editorRef.current?.getMarkdown() ?? '').trim();
    if (!body) { toast.error(t('notes.empty')); return; }
    setBusy(true);
    const tagArr = tags.split(',').map((s) => s.trim()).filter(Boolean);
    try {
      if (note) await noteApi.updateNote(note.id, { title, body, tags: tagArr });
      else await noteApi.createNote(space, title, body, tagArr);
      onOpenChange(false);
      onSaved();
    } catch (e) { toast.error((e as Error).message); }
    finally { setBusy(false); }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="bottom" className="flex h-[90svh] flex-col">
        <SheetHeader>
          <SheetTitle>{note ? t('notes.edit') : t('notes.new')}</SheetTitle>
        </SheetHeader>
        <div className="flex flex-1 flex-col gap-3 overflow-y-auto px-4">
          <Input
            placeholder={t('notes.title')}
            value={title}
            onChange={(e) => setTitle(e.target.value)}
          />
          <div className="flex-1 overflow-y-auto rounded-md border">
            <MDXEditor
              key={`${note?.id ?? 'new'}-${open}`}
              ref={editorRef}
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
        <SheetFooter>
          <Button onClick={save} disabled={busy}>{t('notes.save')}</Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
