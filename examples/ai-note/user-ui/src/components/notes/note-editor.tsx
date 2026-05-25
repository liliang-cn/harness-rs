import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetFooter } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { noteApi, type Note, type Space } from '@/lib/api';

export function NoteEditor({
  open, onOpenChange, space, note, onSaved,
}: {
  open: boolean; onOpenChange: (v: boolean) => void;
  space: Space; note: Note | null; onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const [tags, setTags] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setTitle(note?.title ?? '');
      setBody(note?.body ?? '');
      setTags(note?.tags.join(', ') ?? '');
    }
  }, [open, note]);

  async function save() {
    if (!body.trim()) { toast.error('empty'); return; }
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
        <SheetHeader><SheetTitle>{note ? t('notes.save') : t('notes.new')}</SheetTitle></SheetHeader>
        <div className="flex flex-1 flex-col gap-3 overflow-y-auto px-4">
          <Input placeholder={t('notes.title')} value={title} onChange={(e) => setTitle(e.target.value)} />
          <Textarea
            placeholder={t('notes.body')} value={body}
            onChange={(e) => setBody(e.target.value)} className="min-h-48 flex-1"
          />
          <Input placeholder={t('notes.tags')} value={tags} onChange={(e) => setTags(e.target.value)} />
        </div>
        <SheetFooter>
          <Button onClick={save} disabled={busy}>{t('notes.save')}</Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
