import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';

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
            <div className="p-8 text-center text-muted-foreground">ok</div>
          </RequireAuth>
        }
      />
      <Route path="/app/*" element={<Navigate to="/app" replace />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
