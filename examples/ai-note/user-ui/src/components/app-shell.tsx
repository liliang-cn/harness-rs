import { type ReactNode, useEffect, useState } from 'react';
import { Link, Outlet, useLocation, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  NotebookPen, Search as SearchIcon, Target, User, Globe, LogOut,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Toaster } from '@/components/ui/sonner';
import { ChatFab } from '@/components/chat/chat-fab';
import { ConfirmProvider } from '@/components/confirm-dialog';
import { noteApi, setToken } from '@/lib/api';
import { cn } from '@/lib/utils';
import { useSpace } from '@/components/space-context';

const NAV = [
  { to: '/app', key: 'notes', icon: NotebookPen },
  { to: '/app/plans', key: 'plans', icon: Target },
  { to: '/app/search', key: 'search', icon: SearchIcon },
  { to: '/app/profile', key: 'profile', icon: User },
] as const;

function LangSwitch() {
  const { i18n } = useTranslation();
  const current = i18n.language.startsWith('zh') ? 'zh' : 'en';
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="language">
          <Globe />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem
          onClick={() => i18n.changeLanguage('en')}
          className={current === 'en' ? 'font-semibold' : ''}
        >
          English
        </DropdownMenuItem>
        <DropdownMenuItem
          onClick={() => i18n.changeLanguage('zh')}
          className={current === 'zh' ? 'font-semibold' : ''}
        >
          中文
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function SpaceToggle() {
  const { t } = useTranslation();
  const { space, setSpace } = useSpace();
  return (
    <div className="bg-muted ml-4 inline-flex rounded-full p-0.5 text-xs">
      {(['work', 'life'] as const).map((s) => (
        <button
          key={s}
          type="button"
          onClick={() => setSpace(s)}
          className={cn(
            'rounded-full px-3 py-1 transition-colors',
            space === s ? 'bg-background text-foreground shadow-sm font-medium'
                        : 'text-muted-foreground',
          )}
        >
          {t(`spaces.${s}`)}
        </button>
      ))}
    </div>
  );
}

export function AppShell({ chatSlot }: { chatSlot?: ReactNode }) {
  const { t } = useTranslation();
  const location = useLocation();
  const navigate = useNavigate();
  const [email, setEmail] = useState('');
  useEffect(() => {
    noteApi.me().then((j: { user: any }) => setEmail(j.user?.email ?? '')).catch(() => {});
  }, []);
  function logout() {
    setToken(null);
    navigate('/login');
  }
  return (
    <ConfirmProvider>
    <div className="bg-background flex min-h-svh flex-col">
      <header className="border-border bg-background sticky top-0 z-10 border-b">
        <div className="mx-auto flex h-14 max-w-5xl items-center gap-2 px-4 md:px-8">
          <Link to="/app" className="text-lg font-semibold tracking-tight">
            {t('brand')}
          </Link>
          <SpaceToggle />
          <nav className="ml-6 hidden items-center gap-1 md:flex">
            {NAV.map((item) => {
              const active = isActive(location.pathname, item.to);
              return (
                <Link
                  key={item.to}
                  to={item.to}
                  aria-current={active ? 'page' : undefined}
                  className={cn(
                    'rounded-md px-3 py-1.5 text-sm transition-colors',
                    active
                      ? 'bg-secondary text-secondary-foreground font-medium'
                      : 'text-muted-foreground hover:bg-secondary/60',
                  )}
                >
                  {t(`nav.${item.key}`)}
                </Link>
              );
            })}
          </nav>
          <div className="flex-1" />
          <span className="text-muted-foreground hidden text-xs md:inline">{email}</span>
          <LangSwitch />
          <Button variant="ghost" size="sm" onClick={logout} aria-label={t('common.logout')}>
            <LogOut className="size-4" />
            <span className="hidden sm:inline">{t('common.logout')}</span>
          </Button>
        </div>
      </header>

      <main className="mx-auto w-full max-w-5xl flex-1 px-4 pb-24 pt-6 md:px-8 md:pb-12 md:pt-10">
        <Outlet />
      </main>

      <nav
        className="border-border bg-background fixed inset-x-0 bottom-0 z-10 flex h-16 items-center justify-around border-t md:hidden"
        style={{ paddingBottom: 'env(safe-area-inset-bottom)' }}
      >
        {NAV.map((item) => {
          const active = isActive(location.pathname, item.to);
          return (
            <Link
              key={item.to}
              to={item.to}
              aria-current={active ? 'page' : undefined}
              className={cn(
                'flex h-full flex-1 flex-col items-center justify-center gap-0.5 px-3 py-1 text-[11px]',
                active ? 'text-foreground' : 'text-muted-foreground',
              )}
            >
              <item.icon className="size-5" />
              {t(`nav.${item.key}`)}
            </Link>
          );
        })}
      </nav>

      {chatSlot}
      <ChatFab />
      <Toaster richColors closeButton position="top-center" />
    </div>
    </ConfirmProvider>
  );
}

function isActive(pathname: string, to: string): boolean {
  if (to === '/app') return pathname === '/app' || pathname === '/app/';
  return pathname.startsWith(to);
}
