import { type ComponentType, type ReactNode, useEffect, useState } from 'react';
import { Link, Outlet, useLocation, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  Home, Wallet, Target, User, Globe, LogOut, ChevronDown,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle,
} from '@/components/ui/sheet';
import { Toaster } from '@/components/ui/sonner';
import { ChatFab } from '@/components/chat/chat-fab';
import { ledgerApi, setToken } from '@/lib/api';
import { ConfirmProvider } from '@/components/confirm-dialog';
import { cn } from '@/lib/utils';

type Icon = ComponentType<{ className?: string }>;
type NavLink = { kind: 'link'; to: string; key: string; icon: Icon };
type NavGroup = {
  kind: 'group';
  key: string;
  icon: Icon;
  /** path prefix that marks this group active */
  match: string;
  children: { to: string; key: string }[];
};
type NavItem = NavLink | NavGroup;

// Two-level nav: `Finance` is a parent that opens a submenu (desktop dropdown
// / mobile bottom-sheet) for Income & Cost + Investments. Everything else is
// a flat top-level destination.
const NAV: NavItem[] = [
  { kind: 'link', to: '/app', key: 'dashboard', icon: Home },
  {
    kind: 'group',
    key: 'finance',
    icon: Wallet,
    match: '/app/money',
    children: [
      { to: '/app/money', key: 'money' },
      { to: '/app/money/portfolio', key: 'investments' },
    ],
  },
  { kind: 'link', to: '/app/projects', key: 'project', icon: Target },
  { kind: 'link', to: '/app/profile', key: 'profile', icon: User },
];

const FINANCE = NAV.find(
  (i): i is NavGroup => i.kind === 'group' && i.key === 'finance',
)!;

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
  // Mobile-only: the Finance parent opens a bottom-sheet of its sub-items.
  const [financeOpen, setFinanceOpen] = useState(false);
  useEffect(() => {
    ledgerApi.me().then((j) => setEmail(j.user?.email ?? '')).catch(() => {});
  }, []);
  function logout() {
    setToken(null);
    navigate('/login');
  }
  return (
    <ConfirmProvider>
    <div className="bg-background flex min-h-svh flex-col">
      {/* Top bar — brand + inline tabs (desktop only) + lang/logout.
          Mobile drops the tabs (bottom-nav handles that) and just keeps
          brand + actions. Centered max-w container so left/right
          whitespace stays symmetric on wide screens. */}
      <header className="border-border bg-background sticky top-0 z-10 border-b">
        <div className="mx-auto flex h-14 max-w-5xl items-center gap-2 px-4 md:px-8">
          <Link to="/app" className="text-lg font-semibold tracking-tight">
            {t('brand')}
          </Link>
          <nav className="ml-6 hidden items-center gap-1 md:flex">
            {NAV.map((item) => {
              const active =
                item.kind === 'group'
                  ? location.pathname.startsWith(item.match)
                  : isActive(location.pathname, item.to);
              if (item.kind === 'group') {
                return (
                  <DropdownMenu key={item.key}>
                    <DropdownMenuTrigger asChild>
                      <button
                        type="button"
                        aria-current={active ? 'page' : undefined}
                        className={cn(
                          'flex items-center gap-1 rounded-md px-3 py-1.5 text-sm transition-colors outline-none',
                          active
                            ? 'bg-secondary text-secondary-foreground font-medium'
                            : 'text-muted-foreground hover:bg-secondary/60',
                        )}
                      >
                        {t(`nav.${item.key}`)}
                        <ChevronDown className="size-3.5" />
                      </button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="start">
                      {item.children.map((c) => (
                        <DropdownMenuItem key={c.to} asChild>
                          <Link to={c.to}>{t(`nav.${c.key}`)}</Link>
                        </DropdownMenuItem>
                      ))}
                    </DropdownMenuContent>
                  </DropdownMenu>
                );
              }
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

      {/* Mobile bottom tabs unchanged — top tabs would crowd the small viewport.
          The Finance parent is a button that opens a bottom-sheet of its
          sub-items instead of navigating directly. */}
      <nav
        className="border-border bg-background fixed inset-x-0 bottom-0 z-10 flex h-16 items-center justify-around border-t md:hidden"
        style={{ paddingBottom: 'env(safe-area-inset-bottom)' }}
      >
        {NAV.map((item) => {
          const active =
            item.kind === 'group'
              ? location.pathname.startsWith(item.match)
              : isActive(location.pathname, item.to);
          const cls = cn(
            'flex h-full flex-1 flex-col items-center justify-center gap-0.5 px-3 py-1 text-[11px]',
            active ? 'text-foreground' : 'text-muted-foreground',
          );
          if (item.kind === 'group') {
            return (
              <button
                key={item.key}
                type="button"
                onClick={() => setFinanceOpen(true)}
                aria-current={active ? 'page' : undefined}
                className={cls}
              >
                <item.icon className="size-5" />
                {t(`nav.${item.key}`)}
              </button>
            );
          }
          return (
            <Link
              key={item.to}
              to={item.to}
              aria-current={active ? 'page' : undefined}
              className={cls}
            >
              <item.icon className="size-5" />
              {t(`nav.${item.key}`)}
            </Link>
          );
        })}
      </nav>

      {/* Mobile Finance submenu sheet (slides up from the bottom). */}
      <Sheet open={financeOpen} onOpenChange={setFinanceOpen}>
        <SheetContent
          side="bottom"
          className="rounded-t-xl pb-[env(safe-area-inset-bottom)] md:hidden"
        >
          <SheetHeader>
            <SheetTitle>{t(`nav.${FINANCE.key}`)}</SheetTitle>
            <SheetDescription className="sr-only">
              {t(`nav.${FINANCE.key}`)}
            </SheetDescription>
          </SheetHeader>
          <div className="flex flex-col gap-1 px-2 pb-4">
            {FINANCE.children.map((c) => (
              <Link
                key={c.to}
                to={c.to}
                onClick={() => setFinanceOpen(false)}
                aria-current={
                  location.pathname === c.to ? 'page' : undefined
                }
                className={cn(
                  'rounded-md px-3 py-3 text-sm transition-colors',
                  location.pathname === c.to
                    ? 'bg-secondary text-secondary-foreground font-medium'
                    : 'hover:bg-secondary/60',
                )}
              >
                {t(`nav.${c.key}`)}
              </Link>
            ))}
          </div>
        </SheetContent>
      </Sheet>

      {chatSlot}
      <ChatFab />
      <Toaster richColors closeButton position="top-center" />
    </div>
    </ConfirmProvider>
  );
}

function isActive(pathname: string, to: string): boolean {
  // /app is the dashboard "index" — only active when path is exactly /app
  // (or /app/), not when on /app/ledger etc.
  if (to === '/app') return pathname === '/app' || pathname === '/app/';
  return pathname.startsWith(to);
}
