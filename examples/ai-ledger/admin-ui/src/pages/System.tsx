import { useEffect, useState } from 'react';
import {
  Card,
  Form,
  Input,
  Button,
  Select,
  Space,
  Tag,
  Alert,
  Typography,
  Descriptions,
  message,
} from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { adminApi, type ProviderConfigView } from '@/lib/api';

const { Text } = Typography;

interface CfgForm {
  deepseek_api_key?: string;
  gemini_api_key?: string;
  default_model_id?: string;
}

export function System() {
  const [cfg, setCfg] = useState<ProviderConfigView | null>(null);
  const [form] = Form.useForm<CfgForm>();
  const [saving, setSaving] = useState(false);
  const [logs, setLogs] = useState('');
  const [logLines, setLogLines] = useState(200);
  const [logBusy, setLogBusy] = useState(false);
  const [logErr, setLogErr] = useState('');

  async function loadCfg() {
    const j = await adminApi.getConfig();
    setCfg(j);
    form.setFieldsValue({ default_model_id: j.default_model_id });
  }

  async function loadLogs() {
    setLogBusy(true);
    setLogErr('');
    try {
      const j = await adminApi.getLogs(logLines);
      setLogs(j.lines || '');
      if (j.error) setLogErr(j.error);
    } catch (e) {
      setLogErr(String((e as Error).message));
    } finally {
      setLogBusy(false);
    }
  }

  useEffect(() => {
    loadCfg().catch(() => {});
    loadLogs().catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function onSave(values: CfgForm) {
    const body: CfgForm = {};
    if (values.deepseek_api_key?.trim()) body.deepseek_api_key = values.deepseek_api_key.trim();
    if (values.gemini_api_key?.trim()) body.gemini_api_key = values.gemini_api_key.trim();
    if (values.default_model_id && values.default_model_id !== cfg?.default_model_id) {
      body.default_model_id = values.default_model_id;
    }
    if (Object.keys(body).length === 0) {
      message.info('没有变化');
      return;
    }
    setSaving(true);
    try {
      const r = await adminApi.patchConfig(body);
      message.success(`已更新: ${r.changed.join(', ')}`);
      form.setFieldsValue({ deepseek_api_key: '', gemini_api_key: '' });
      await loadCfg();
    } catch (err) {
      message.error(`保存失败: ${(err as Error).message}`);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Space direction="vertical" size={24} style={{ width: '100%' }}>
      <Card title="Provider 配置" loading={!cfg}>
        {cfg && (
          <>
            <Descriptions
              size="small"
              column={1}
              styles={{ label: { width: 140 } }}
              style={{ marginBottom: 16 }}
            >
              <Descriptions.Item label="DeepSeek key">
                <Text code>{cfg.deepseek_key_masked || '未设置'}</Text>
              </Descriptions.Item>
              <Descriptions.Item label="Gemini key">
                <Text code>{cfg.gemini_key_masked || '未设置'}</Text>
              </Descriptions.Item>
              <Descriptions.Item label="可用模型">
                <Space wrap>
                  {cfg.available_models.map((m) => (
                    <Tag key={m.id} color={m.available ? 'blue' : 'default'}>
                      {m.id}
                    </Tag>
                  ))}
                </Space>
              </Descriptions.Item>
            </Descriptions>

            <Form
              form={form}
              layout="vertical"
              onFinish={onSave}
              initialValues={{ default_model_id: cfg.default_model_id }}
              style={{ maxWidth: 520 }}
            >
              <Form.Item
                label="DeepSeek API key"
                name="deepseek_api_key"
                extra="留空 = 不动"
              >
                <Input.Password placeholder={cfg.deepseek_key_masked || '未设置'} autoComplete="off" />
              </Form.Item>
              <Form.Item label="Gemini API key" name="gemini_api_key" extra="留空 = 不动">
                <Input.Password placeholder={cfg.gemini_key_masked || '未设置'} autoComplete="off" />
              </Form.Item>
              <Form.Item label="默认 model" name="default_model_id">
                <Select
                  options={cfg.available_models.map((m) => ({
                    value: m.id,
                    label: `${m.label}${m.available ? '' : ' (无 key)'}`,
                    disabled: !m.available,
                  }))}
                />
              </Form.Item>
              <Form.Item style={{ marginBottom: 0 }}>
                <Space>
                  <Button type="primary" htmlType="submit" loading={saving}>
                    保存
                  </Button>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    改完即时生效（无需重启）；写入 DB 后重启仍保留。
                  </Text>
                </Space>
              </Form.Item>
            </Form>
          </>
        )}
      </Card>

      <Card
        title="systemd journal · ai-ledger"
        extra={
          <Space>
            <Select
              size="small"
              value={logLines}
              onChange={setLogLines}
              options={[50, 200, 500, 1000, 2000].map((n) => ({ value: n, label: `${n} 行` }))}
              style={{ width: 100 }}
            />
            <Button size="small" icon={<ReloadOutlined />} loading={logBusy} onClick={loadLogs}>
              刷新
            </Button>
          </Space>
        }
      >
        {logErr && (
          <Alert
            type="warning"
            message={logErr}
            style={{ marginBottom: 12 }}
            showIcon
          />
        )}
        <pre
          style={{
            background: 'var(--ant-color-bg-elevated)',
            color: 'var(--ant-color-text)',
            padding: 12,
            fontFamily: 'ui-monospace, Menlo, monospace',
            fontSize: 11,
            lineHeight: 1.55,
            maxHeight: '60vh',
            overflow: 'auto',
            borderRadius: 6,
            margin: 0,
            whiteSpace: 'pre',
          }}
        >
          {logs || (logBusy ? '加载中…' : '(空)')}
        </pre>
      </Card>
    </Space>
  );
}
