import {
  createContext, useCallback, useContext, useRef, useState, type ReactNode,
} from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';

interface ConfirmOptions {
  /** Main question — defaults to a generic "Are you sure?". */
  title?: string;
  description?: string;
  confirmText?: string;
  cancelText?: string;
  /** Style the confirm button as destructive (red). */
  destructive?: boolean;
}

type ConfirmFn = (opts?: ConfirmOptions) => Promise<boolean>;

const ConfirmCtx = createContext<ConfirmFn>(async () => false);

/** Promise-based confirm — `if (!(await confirm({ title }))) return;`. */
export function useConfirm(): ConfirmFn {
  return useContext(ConfirmCtx);
}

export function ConfirmProvider({ children }: { children: ReactNode }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [opts, setOpts] = useState<ConfirmOptions>({});
  const resolverRef = useRef<((v: boolean) => void) | null>(null);

  const confirm = useCallback<ConfirmFn>((o = {}) => {
    setOpts(o);
    setOpen(true);
    return new Promise<boolean>((resolve) => {
      resolverRef.current = resolve;
    });
  }, []);

  const settle = useCallback((v: boolean) => {
    setOpen(false);
    resolverRef.current?.(v);
    resolverRef.current = null;
  }, []);

  return (
    <ConfirmCtx.Provider value={confirm}>
      {children}
      <Dialog open={open} onOpenChange={(o) => { if (!o) settle(false); }}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>{opts.title ?? t('common.confirmTitle')}</DialogTitle>
            {opts.description && <DialogDescription>{opts.description}</DialogDescription>}
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => settle(false)}>
              {opts.cancelText ?? t('common.cancel')}
            </Button>
            <Button
              variant={opts.destructive ? 'destructive' : 'default'}
              onClick={() => settle(true)}
            >
              {opts.confirmText ?? t('common.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </ConfirmCtx.Provider>
  );
}
