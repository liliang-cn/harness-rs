import { useRef, useState } from 'react';
import { Paperclip, Loader2 } from 'lucide-react';
import { uploadAttachment, type Attachment } from '@/lib/api';
import { toast } from 'sonner';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';

interface Props {
  onAttached: (a: Attachment) => void;
  disabled?: boolean;
}

export function AttachmentButton({ onAttached, disabled }: Props) {
  const { t } = useTranslation();
  const ref = useRef<HTMLInputElement>(null);
  const [uploading, setUploading] = useState(false);

  async function onPick(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    e.target.value = '';
    if (!file) return;
    setUploading(true);
    try {
      const att = await uploadAttachment(file);
      onAttached(att);
    } catch (err) {
      toast.error(`${t('chat.attachFailed')}: ${(err as Error).message}`);
    } finally {
      setUploading(false);
    }
  }

  return (
    <>
      <input ref={ref} type="file" accept="image/*,application/pdf" hidden onChange={onPick} />
      <button
        type="button"
        aria-label={t('chat.attach')}
        onClick={() => ref.current?.click()}
        disabled={disabled || uploading}
        className={cn(
          'flex size-9 shrink-0 items-center justify-center rounded-full transition-colors',
          'text-muted-foreground hover:text-foreground hover:bg-muted',
          (disabled || uploading) && 'cursor-not-allowed opacity-50',
        )}
      >
        {uploading ? <Loader2 className="size-4 animate-spin" /> : <Paperclip className="size-4" />}
      </button>
    </>
  );
}
