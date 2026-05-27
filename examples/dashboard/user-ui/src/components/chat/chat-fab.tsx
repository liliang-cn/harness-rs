import { useState } from 'react';
import { MessageSquare } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { ChatSheet } from './chat-sheet';

/**
 * Floating action button — bottom-right on desktop, bottom-right clear of
 * the 64px mobile bottom-tab nav. z-20 sits above the sticky nav (z-10)
 * but below the Sheet overlay (z-50).
 */
export function ChatFab() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button
        type="button"
        aria-label={t('chat.fab')}
        onClick={() => setOpen(true)}
        className="fixed right-4 bottom-20 z-20 size-14 rounded-full shadow-lg md:right-6 md:bottom-6"
      >
        <MessageSquare className="size-6" />
      </Button>
      <ChatSheet open={open} onOpenChange={setOpen} />
    </>
  );
}
