import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Card, Form, Input, Button, Alert, Typography, Tabs } from 'antd';
import { LockOutlined, MailOutlined, KeyOutlined } from '@ant-design/icons';
import { ledgerApi, setToken } from '@/lib/api';

const { Title, Text } = Typography;

interface LoginValues {
  email: string;
  password: string;
}
interface RegisterValues extends LoginValues {
  invite_code?: string;
}

export function Login() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [mode, setMode] = useState<'login' | 'register'>('login');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  async function onLogin(v: LoginValues) {
    setError('');
    setBusy(true);
    try {
      const r = await ledgerApi.login(v.email.trim(), v.password);
      setToken(r.token);
      navigate('/');
    } catch (err) {
      setError(String((err as Error).message || err));
    } finally {
      setBusy(false);
    }
  }

  async function onRegister(v: RegisterValues) {
    setError('');
    setBusy(true);
    try {
      const r = await ledgerApi.register(
        v.email.trim(),
        v.password,
        v.invite_code?.trim() || undefined,
      );
      setToken(r.token);
      navigate('/');
    } catch (err) {
      setError(String((err as Error).message || err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div
      style={{
        minHeight: '100vh',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 16,
        background: 'var(--ant-color-bg-layout)',
      }}
    >
      <Card style={{ width: '100%', maxWidth: 380 }}>
        <Title level={3} style={{ marginTop: 0, marginBottom: 4 }}>
          {t('login.title')}
        </Title>
        <Text type="secondary">{t('login.subtitle')}</Text>
        <Tabs
          activeKey={mode}
          onChange={(k) => {
            setMode(k as 'login' | 'register');
            setError('');
          }}
          items={[
            { key: 'login', label: t('login.submit') },
            { key: 'register', label: t('login.register') },
          ]}
          style={{ marginTop: 12 }}
        />
        {mode === 'login' ? (
          <Form layout="vertical" requiredMark={false} onFinish={onLogin}>
            <Form.Item label={t('login.email')} name="email" rules={[{ required: true }]}>
              <Input prefix={<MailOutlined />} autoComplete="email" />
            </Form.Item>
            <Form.Item label={t('login.password')} name="password" rules={[{ required: true }]}>
              <Input.Password prefix={<LockOutlined />} autoComplete="current-password" />
            </Form.Item>
            {error && (
              <Form.Item>
                <Alert type="error" message={error} showIcon />
              </Form.Item>
            )}
            <Button type="primary" htmlType="submit" loading={busy} block>
              {t('login.submit')}
            </Button>
          </Form>
        ) : (
          <Form layout="vertical" requiredMark={false} onFinish={onRegister}>
            <Form.Item label={t('login.email')} name="email" rules={[{ required: true }]}>
              <Input prefix={<MailOutlined />} autoComplete="email" />
            </Form.Item>
            <Form.Item label={t('login.password')} name="password" rules={[{ required: true }]}>
              <Input.Password prefix={<LockOutlined />} autoComplete="new-password" />
            </Form.Item>
            <Form.Item label={t('login.invite')} name="invite_code">
              <Input prefix={<KeyOutlined />} />
            </Form.Item>
            {error && (
              <Form.Item>
                <Alert type="error" message={error} showIcon />
              </Form.Item>
            )}
            <Button type="primary" htmlType="submit" loading={busy} block>
              {t('login.registerSubmit')}
            </Button>
          </Form>
        )}
      </Card>
    </div>
  );
}
