import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Dashboard } from '@/pages/Dashboard';
import { Ledger } from '@/pages/Ledger';
import { Portfolio } from '@/pages/Portfolio';
import { Profile } from '@/pages/Profile';
import { AppShell } from '@/components/app-shell';

export default function App() {
  const authed = getToken();
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      {authed ? (
        <Route path="/" element={<AppShell />}>
          <Route index element={<Dashboard />} />
          <Route path="ledger" element={<Ledger />} />
          <Route path="portfolio" element={<Portfolio />} />
          <Route path="profile" element={<Profile />} />
        </Route>
      ) : (
        <Route path="/" element={<Marketing />} />
      )}
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
