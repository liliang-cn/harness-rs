import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Dashboard } from '@/pages/Dashboard';
import { AppShell } from '@/components/app-shell';

export default function App() {
  const authed = getToken();
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      {authed ? (
        <Route path="/" element={<AppShell />}>
          <Route index element={<Dashboard />} />
          <Route
            path="ledger"
            element={
              <div className="text-muted-foreground py-8 text-center text-sm">
                Ledger (Task 2 placeholder)
              </div>
            }
          />
          <Route
            path="portfolio"
            element={
              <div className="text-muted-foreground py-8 text-center text-sm">
                Portfolio (Task 4 placeholder)
              </div>
            }
          />
          <Route
            path="profile"
            element={
              <div className="text-muted-foreground py-8 text-center text-sm">
                Profile (Task 6 placeholder)
              </div>
            }
          />
        </Route>
      ) : (
        <Route path="/" element={<Marketing />} />
      )}
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
