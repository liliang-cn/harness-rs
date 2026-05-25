import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Notes } from '@/pages/Notes';
import { Search } from '@/pages/Search';
import { Profile } from '@/pages/Profile';
import { AppShell } from '@/components/app-shell';
import { SpaceProvider } from '@/components/space-context';

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
            <SpaceProvider>
              <AppShell />
            </SpaceProvider>
          </RequireAuth>
        }
      >
        <Route index element={<Notes />} />
        <Route path="search" element={<Search />} />
        <Route path="profile" element={<Profile />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
