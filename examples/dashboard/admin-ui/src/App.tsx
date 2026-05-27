import {
  Routes,
  Route,
  Navigate,
  useNavigate,
  useLocation,
} from 'react-router-dom';
import { useEffect, useState, type ReactNode } from 'react';
import { Layout, Menu, Dropdown, Button, Space, Typography, Grid } from 'antd';
import {
  TeamOutlined,
  AuditOutlined,
  ControlOutlined,
  LogoutOutlined,
  SunOutlined,
  MoonOutlined,
  DesktopOutlined,
  MenuFoldOutlined,
  MenuUnfoldOutlined,
} from '@ant-design/icons';
import { getToken, setToken } from '@/lib/api';
import { useTheme, type ThemeMode } from '@/lib/theme';
import { Login } from '@/pages/Login';
import { Users } from '@/pages/Users';
import { Audit } from '@/pages/Audit';
import { System } from '@/pages/System';

const { Sider, Header, Content } = Layout;
const { useBreakpoint } = Grid;
const { Text } = Typography;

const MENU_ITEMS = [
  { key: '/users', icon: <TeamOutlined />, label: '用户' },
  { key: '/audit', icon: <AuditOutlined />, label: '审计日志' },
  { key: '/system', icon: <ControlOutlined />, label: '系统' },
];

function ThemeSwitch() {
  const { mode, setMode } = useTheme();
  const items = [
    { key: 'light', icon: <SunOutlined />, label: 'Light' },
    { key: 'dark', icon: <MoonOutlined />, label: 'Dark' },
    { key: 'system', icon: <DesktopOutlined />, label: 'System' },
  ];
  const currentIcon =
    mode === 'light' ? <SunOutlined /> : mode === 'dark' ? <MoonOutlined /> : <DesktopOutlined />;
  return (
    <Dropdown
      menu={{
        items,
        selectable: true,
        selectedKeys: [mode],
        onClick: ({ key }) => setMode(key as ThemeMode),
      }}
      placement="bottomRight"
    >
      <Button type="text" icon={currentIcon} aria-label="切换主题" />
    </Dropdown>
  );
}

function Shell({ children }: { children: ReactNode }) {
  const navigate = useNavigate();
  const location = useLocation();
  const screens = useBreakpoint();
  const [collapsed, setCollapsed] = useState<boolean>(false);
  const [email, setEmail] = useState<string>('');

  useEffect(() => {
    // Auto-collapse only when we KNOW we're below lg (avoid the first-paint
    // race where every breakpoint is still undefined).
    if (screens.lg === false) setCollapsed(true);
    else if (screens.lg === true) setCollapsed(false);
  }, [screens.lg]);

  useEffect(() => {
    fetch('/api/me', { headers: { Authorization: `Bearer ${getToken()}` } })
      .then((r) => (r.ok ? r.json() : null))
      .then((j) => {
        if (j?.user?.email) setEmail(j.user.email);
      })
      .catch(() => {});
  }, []);

  function logout() {
    setToken(null);
    navigate('/login');
  }

  const activeKey = MENU_ITEMS.find((m) => location.pathname.startsWith(m.key))?.key ?? '/users';

  return (
    <Layout style={{ minHeight: '100%' }}>
      <Sider
        collapsible
        collapsed={collapsed}
        onCollapse={setCollapsed}
        trigger={null}
        breakpoint="lg"
        collapsedWidth={screens.xs ? 0 : 64}
      >
        <div
          style={{
            height: 48,
            margin: 12,
            display: 'flex',
            alignItems: 'center',
            color: 'rgba(255,255,255,0.92)',
            fontWeight: 600,
            fontSize: collapsed ? 14 : 15,
            letterSpacing: collapsed ? 0 : 0.2,
            whiteSpace: 'nowrap',
            overflow: 'hidden',
          }}
        >
          {collapsed ? 'AL' : 'ai-ledger / admin'}
        </div>
        <Menu
          theme="dark"
          mode="inline"
          selectedKeys={[activeKey]}
          onClick={({ key }) => navigate(key)}
          items={MENU_ITEMS}
        />
      </Sider>
      <Layout>
        <Header
          style={{
            padding: '0 16px',
            display: 'flex',
            alignItems: 'center',
            gap: 8,
            background: 'var(--ant-color-bg-container)',
          }}
        >
          <Button
            type="text"
            icon={collapsed ? <MenuUnfoldOutlined /> : <MenuFoldOutlined />}
            onClick={() => setCollapsed((v) => !v)}
            aria-label="切换侧栏"
          />
          <div style={{ flex: 1 }} />
          <Space>
            <Text type="secondary" style={{ fontSize: 12 }}>
              {email}
            </Text>
            <ThemeSwitch />
            <Button type="text" icon={<LogoutOutlined />} onClick={logout}>
              登出
            </Button>
          </Space>
        </Header>
        <Content style={{ padding: 24, overflow: 'auto' }}>{children}</Content>
      </Layout>
    </Layout>
  );
}

function RequireAuth({ children }: { children: ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}

export default function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route
        path="/users"
        element={
          <RequireAuth>
            <Shell><Users /></Shell>
          </RequireAuth>
        }
      />
      <Route
        path="/audit"
        element={
          <RequireAuth>
            <Shell><Audit /></Shell>
          </RequireAuth>
        }
      />
      <Route
        path="/system"
        element={
          <RequireAuth>
            <Shell><System /></Shell>
          </RequireAuth>
        }
      />
      <Route path="*" element={<Navigate to="/users" replace />} />
    </Routes>
  );
}
