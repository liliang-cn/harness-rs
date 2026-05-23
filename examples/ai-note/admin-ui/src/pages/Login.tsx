import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Card, Form, Input, Button, Alert, Typography } from 'antd';
import { LockOutlined, UserOutlined } from '@ant-design/icons';
import { login } from '@/lib/api';

const { Title, Paragraph, Text } = Typography;

interface FormValues {
  email: string;
  password: string;
}

export function Login() {
  const navigate = useNavigate();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  async function onFinish(values: FormValues) {
    setError('');
    setBusy(true);
    try {
      const user = await login(values.email.trim(), values.password);
      if (user.tier !== 'admin') {
        setError(`tier=${user.tier} — admin only`);
        return;
      }
      navigate('/users');
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
      <Card style={{ width: '100%', maxWidth: 360 }}>
        <Title level={4} style={{ marginTop: 0, marginBottom: 4 }}>
          admin 登录
        </Title>
        <Text type="secondary" style={{ fontSize: 12 }}>
          ai-note 管理后台
        </Text>
        <Form
          layout="vertical"
          requiredMark={false}
          onFinish={onFinish}
          style={{ marginTop: 16 }}
        >
          <Form.Item
            label="邮箱"
            name="email"
            rules={[{ required: true, message: '邮箱不能为空' }]}
          >
            <Input prefix={<UserOutlined />} autoComplete="email" />
          </Form.Item>
          <Form.Item
            label="密码"
            name="password"
            rules={[{ required: true, message: '密码不能为空' }]}
          >
            <Input.Password prefix={<LockOutlined />} autoComplete="current-password" />
          </Form.Item>
          {error && (
            <Form.Item>
              <Alert type="error" message={error} showIcon />
            </Form.Item>
          )}
          <Form.Item style={{ marginBottom: 0 }}>
            <Button type="primary" htmlType="submit" loading={busy} block>
              登录
            </Button>
          </Form.Item>
        </Form>
        <Paragraph type="secondary" style={{ marginTop: 16, marginBottom: 0, fontSize: 11 }}>
          用主应用的 admin 账户登录。
        </Paragraph>
      </Card>
    </div>
  );
}
