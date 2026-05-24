import { useEffect, useState, type ReactNode } from 'react';
import {
  Routes,
  Route,
  Navigate,
  useNavigate,
  Link,
} from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Globe, LogOut } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Toaster } from '@/components/ui/sonner';
import { getToken, setToken, ledgerApi } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Dashboard } from '@/pages/Dashboard';
import { Marketing } from '@/pages/Marketing';

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

function Shell({ children }: { children: ReactNode }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [email, setEmail] = useState('');

  useEffect(() => {
    ledgerApi
      .me()
      .then((j) => setEmail(j.user?.email ?? ''))
      .catch(() => {});
  }, []);

  function logout() {
    setToken(null);
    navigate('/login');
  }

  return (
    <div className="bg-muted/30 min-h-svh">
      <header className="border-border bg-background sticky top-0 z-10 flex h-14 items-center gap-3 border-b px-4 sm:px-6">
        <Link
          to="/"
          className="text-lg font-semibold tracking-tight"
        >
          {t('brand')}
        </Link>
        <div className="flex-1" />
        <span className="text-muted-foreground hidden text-xs sm:inline">{email}</span>
        <LangSwitch />
        <Button variant="ghost" size="sm" onClick={logout}>
          <LogOut /> <span className="hidden sm:inline">{t('common.logout')}</span>
        </Button>
      </header>
      <main className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-10">{children}</main>
    </div>
  );
}

function RequireAuth({ children }: { children: ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}

/// Root route: anonymous visitors get the marketing landing (SEO/GEO
/// content); authenticated users see the dashboard inside the shell.
/// Splitting per-state at the root keeps the unauth path crawler-friendly
/// while logged-in users land straight on their data.
function Root() {
  return getToken() ? (
    <Shell>
      <Dashboard />
    </Shell>
  ) : (
    <Marketing />
  );
}

export default function App() {
  return (
    <>
      <Routes>
        <Route path="/login" element={<Login />} />
        <Route path="/" element={<Root />} />
        <Route
          path="/app"
          element={
            <RequireAuth>
              <Shell>
                <Dashboard />
              </Shell>
            </RequireAuth>
          }
        />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
      <Toaster richColors closeButton position="top-center" />
    </>
  );
}
