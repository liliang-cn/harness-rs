import { useEffect, useMemo, useState } from 'react';
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
  InputNumber,
  Popconfirm,
  message,
} from 'antd';
import { DeleteOutlined, PlusOutlined, ReloadOutlined } from '@ant-design/icons';
import { adminApi, type ProviderConfigView, type RateCard } from '@/lib/api';

const { Text } = Typography;

interface CfgForm {
  deepseek_api_key?: string;
  gemini_api_key?: string;
  default_model_id?: string;
}

interface PricingRow {
  // Stable React key — random so adding rows doesn't collide with edits in-flight.
  rid: string;
  // The model id is editable; we don't use it as the React key.
  model: string;
  input: number;
  output: number;
}

function rateCardToRows(card: RateCard): PricingRow[] {
  return Object.entries(card)
    .map(([model, r]) => ({
      rid: Math.random().toString(36).slice(2),
      model,
      input: r.input,
      output: r.output,
    }))
    .sort((a, b) => a.model.localeCompare(b.model));
}

function rowsToRateCard(rows: PricingRow[]): { card: RateCard; error?: string } {
  const card: RateCard = {};
  const seen = new Set<string>();
  for (const r of rows) {
    const m = r.model.trim();
    if (!m) return { card, error: 'model id 不能为空' };
    if (seen.has(m)) return { card, error: `model id 重复: ${m}` };
    seen.add(m);
    if (!Number.isFinite(r.input) || !Number.isFinite(r.output) || r.input < 0 || r.output < 0) {
      return { card, error: `${m}: input/output 必须 ≥ 0` };
    }
    card[m] = { input: r.input, output: r.output };
  }
  return { card };
}

export function System() {
  const [cfg, setCfg] = useState<ProviderConfigView | null>(null);
  const [form] = Form.useForm<CfgForm>();
  const [saving, setSaving] = useState(false);
  const [logs, setLogs] = useState('');
  const [logLines, setLogLines] = useState(200);
  const [logBusy, setLogBusy] = useState(false);
  const [logErr, setLogErr] = useState('');
  const [pricingRows, setPricingRows] = useState<PricingRow[]>([]);
  const [pricingSaving, setPricingSaving] = useState(false);

  const pricingDirty = useMemo(() => {
    if (!cfg) return false;
    const fromServer = JSON.stringify(rateCardToRows(cfg.pricing).map(({ rid: _r, ...rest }) => rest));
    const local = JSON.stringify(pricingRows.map(({ rid: _r, ...rest }) => rest));
    return fromServer !== local;
  }, [cfg, pricingRows]);

  async function loadCfg() {
    const j = await adminApi.getConfig();
    setCfg(j);
    form.setFieldsValue({ default_model_id: j.default_model_id });
    setPricingRows(rateCardToRows(j.pricing ?? {}));
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

  async function savePricing() {
    const { card, error } = rowsToRateCard(pricingRows);
    if (error) {
      message.error(error);
      return;
    }
    setPricingSaving(true);
    try {
      await adminApi.patchConfig({ pricing: card });
      message.success('计费表已保存');
      await loadCfg();
    } catch (e) {
      message.error(`保存失败: ${(e as Error).message}`);
    } finally {
      setPricingSaving(false);
    }
  }

  function updateRow(rid: string, patch: Partial<PricingRow>) {
    setPricingRows((rows) => rows.map((r) => (r.rid === rid ? { ...r, ...patch } : r)));
  }
  function addRow() {
    setPricingRows((rows) => [
      ...rows,
      { rid: Math.random().toString(36).slice(2), model: '', input: 0, output: 0 },
    ]);
  }
  function removeRow(rid: string) {
    setPricingRows((rows) => rows.filter((r) => r.rid !== rid));
  }

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
        title="Token 计费表 (USD / 1M tokens)"
        extra={
          <Space>
            <Button
              size="small"
              icon={<PlusOutlined />}
              onClick={addRow}
            >
              添加模型
            </Button>
            <Button
              size="small"
              type="primary"
              onClick={savePricing}
              loading={pricingSaving}
              disabled={!pricingDirty}
            >
              保存计费表
            </Button>
          </Space>
        }
      >
        <Alert
          type="info"
          showIcon
          message="按 model id 配置每百万 token 的 USD 单价；未列出的模型按 0.10 / 0.60 的兜底费率计算。"
          style={{ marginBottom: 12 }}
        />
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'minmax(200px, 1.5fr) 120px 120px 56px',
            gap: 8,
            alignItems: 'center',
            fontSize: 12,
            color: 'var(--ant-color-text-secondary)',
            paddingBottom: 6,
            marginBottom: 4,
            borderBottom: '1px solid var(--ant-color-border-secondary)',
          }}
        >
          <div>Model ID</div>
          <div style={{ textAlign: 'right' }}>Input $/1M</div>
          <div style={{ textAlign: 'right' }}>Output $/1M</div>
          <div />
        </div>
        {pricingRows.length === 0 && (
          <Text type="secondary" style={{ display: 'block', padding: '12px 0' }}>
            还没有任何模型 — 点上方「添加模型」开始。
          </Text>
        )}
        {pricingRows.map((r) => (
          <div
            key={r.rid}
            style={{
              display: 'grid',
              gridTemplateColumns: 'minmax(200px, 1.5fr) 120px 120px 56px',
              gap: 8,
              alignItems: 'center',
              padding: '6px 0',
            }}
          >
            <Input
              size="small"
              placeholder="deepseek-v4-flash"
              value={r.model}
              onChange={(e) => updateRow(r.rid, { model: e.target.value })}
            />
            <InputNumber
              size="small"
              min={0}
              step={0.01}
              value={r.input}
              style={{ width: '100%' }}
              onChange={(v) => updateRow(r.rid, { input: typeof v === 'number' ? v : 0 })}
            />
            <InputNumber
              size="small"
              min={0}
              step={0.01}
              value={r.output}
              style={{ width: '100%' }}
              onChange={(v) => updateRow(r.rid, { output: typeof v === 'number' ? v : 0 })}
            />
            <Popconfirm
              title={`删除 ${r.model || '该行'}？`}
              onConfirm={() => removeRow(r.rid)}
              okText="删除"
              cancelText="取消"
            >
              <Button size="small" danger icon={<DeleteOutlined />} type="text" />
            </Popconfirm>
          </div>
        ))}
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
