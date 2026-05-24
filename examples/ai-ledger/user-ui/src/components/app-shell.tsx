import { type ReactNode, useEffect, useState } from 'react';
import { Link, Outlet, useLocation, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  Home, Wallet, TrendingUp, User, Globe, LogOut,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Toaster } from '@/components/ui/sonner';
import { ledgerApi, setToken } from '@/lib/api';
import { cn } from '@/lib/utils';

const NAV = [
  { to: '/', key: 'dashboard', icon: Home },
  { to: '/ledger', key: 'ledger', icon: Wallet },
  { to: '/portfolio', key: 'portfolio', icon: TrendingUp },
  { to: '/profile', key: 'profile', icon: User },
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

export function AppShell({ chatSlot }: { chatSlot?: ReactNode }) {
  const { t } = useTranslation();
  const location = useLocation();
  const navigate = useNavigate();
  const [email, setEmail] = useState('');
  useEffect(() => {
    ledgerApi.me().then((j) => setEmail(j.user?.email ?? '')).catch(() => {});
  }, []);
  function logout() {
    setToken(null);
    navigate('/login');
  }
  return (
    <div className="bg-muted/20 flex min-h-svh">
      <aside className="border-border bg-background hidden w-56 shrink-0 flex-col border-r md:flex">
        <Link to="/" className="flex h-14 items-center px-4 text-lg font-semibold">
          {t('brand')}
        </Link>
        <nav className="flex flex-1 flex-col gap-1 px-2 py-2">
          {NAV.map((item) => {
            const active = isActive(location.pathname, item.to);
            return (
              <Link
                key={item.to}
                to={item.to}
                aria-current={active ? 'page' : undefined}
                className={cn(
                  'flex items-center gap-3 rounded-md px-3 py-2 text-sm',
                  active ? 'bg-secondary text-secondary-foreground font-medium'
                         : 'text-muted-foreground hover:bg-secondary/60',
                )}
              >
                <item.icon className="size-4" />
                {t(`nav.${item.key}`)}
              </Link>
            );
          })}
        </nav>
        <div className="border-border border-t p-3 text-xs">
          <div className="text-muted-foreground mb-2 truncate">{email}</div>
          <div className="flex items-center justify-between">
            <LangSwitch />
            <Button variant="ghost" size="sm" onClick={logout}>
              <LogOut className="size-4" /> {t('common.logout')}
            </Button>
          </div>
        </div>
      </aside>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="border-border bg-background sticky top-0 z-10 flex h-14 items-center gap-2 border-b px-4 md:hidden">
          <Link to="/" className="text-base font-semibold">{t('brand')}</Link>
          <div className="flex-1" />
          <LangSwitch />
          <Button variant="ghost" size="icon" onClick={logout} aria-label={t('common.logout')}>
            <LogOut />
          </Button>
        </header>
        <main className="mx-auto w-full max-w-3xl flex-1 px-4 pb-24 pt-4 md:pb-10 md:pt-10">
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
      </div>
      {chatSlot}
      <Toaster richColors closeButton position="top-center" />
    </div>
  );
}

function isActive(pathname: string, to: string): boolean {
  if (to === '/') return pathname === '/';
  return pathname.startsWith(to);
}
