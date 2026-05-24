import { useEffect, useState, type ReactNode } from 'react';
import {
  Routes,
  Route,
  Navigate,
  useNavigate,
} from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Layout, Button, Space, Typography, Dropdown } from 'antd';
import { LogoutOutlined, GlobalOutlined } from '@ant-design/icons';

import { getToken, setToken, ledgerApi } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Dashboard } from '@/pages/Dashboard';

const { Header, Content } = Layout;
const { Text, Title } = Typography;

function LangSwitch() {
  const { i18n } = useTranslation();
  const items = [
    { key: 'en', label: 'English' },
    { key: 'zh', label: '中文' },
  ];
  return (
    <Dropdown
      menu={{
        items,
        selectable: true,
        selectedKeys: [i18n.language.startsWith('zh') ? 'zh' : 'en'],
        onClick: ({ key }) => i18n.changeLanguage(key),
      }}
      placement="bottomRight"
    >
      <Button type="text" icon={<GlobalOutlined />} aria-label="language" />
    </Dropdown>
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
    <Layout style={{ minHeight: '100vh', background: 'var(--ant-color-bg-layout)' }}>
      <Header
        style={{
          padding: '0 16px',
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          background: 'var(--ant-color-bg-container)',
          borderBottom: '1px solid var(--ant-color-border-secondary)',
        }}
      >
        <Title level={4} style={{ margin: 0, marginRight: 16 }}>
          {t('brand')}
        </Title>
        <div style={{ flex: 1 }} />
        <Space size={4}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            {email}
          </Text>
          <LangSwitch />
          <Button type="text" icon={<LogoutOutlined />} onClick={logout}>
            {t('common.logout')}
          </Button>
        </Space>
      </Header>
      <Content
        style={{
          padding: 24,
          maxWidth: 1024,
          margin: '0 auto',
          width: '100%',
          boxSizing: 'border-box',
        }}
      >
        {children}
      </Content>
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
        path="/"
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
  );
}
