import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Dashboard } from '@/pages/Dashboard';
import { Ledger } from '@/pages/Ledger';
import { Portfolio } from '@/pages/Portfolio';
import { Profile } from '@/pages/Profile';
import { AppShell } from '@/components/app-shell';

// `/` is always the Marketing page — one URL, one content. Authenticated
// app lives under `/app/*` (the shell + nested routes). A logged-in user
// visiting `/` sees Marketing too (they can click `Open app` to jump
// in); login redirects to `/app`. This keeps the SEO landing stable
// regardless of session state and avoids the "same URL different content"
// surprise.
function RequireAuth({ children }: { children: React.ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Marketing />} />
      <Route path="/login" element={<Login />} />
      <Route
        path="/app"
        element={
          <RequireAuth>
            <AppShell />
          </RequireAuth>
        }
      >
        <Route index element={<Dashboard />} />
        <Route path="ledger" element={<Ledger />} />
        <Route path="portfolio" element={<Portfolio />} />
        <Route path="profile" element={<Profile />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
