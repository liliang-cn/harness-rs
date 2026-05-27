import { useEffect, useState } from 'react';
import { Table, Tag, Select, Space, Button, Card, Typography, message } from 'antd';
import type { ColumnsType } from 'antd/es/table';
import { ReloadOutlined } from '@ant-design/icons';
import { adminApi, type AuditEvent, type UserStats } from '@/lib/api';

const { Title, Text } = Typography;

const KIND_LABEL: Record<string, string> = {
  login: '登录',
  login_failed: '登录失败',
  logout: '登出',
  register: '注册',
  chat_message: '聊天',
  tier_change: '改 tier',
  delete_user: '删除用户',
  password_reset: '重置密码',
  password_change: '改密',
  admin_config_change: '改配置',
};

function fmtTime(ms: number): string {
  const d = new Date(ms);
  const y = d.getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  const h = String(d.getHours()).padStart(2, '0');
  const mn = String(d.getMinutes()).padStart(2, '0');
  const s = String(d.getSeconds()).padStart(2, '0');
  return `${y}-${mm}-${dd} ${h}:${mn}:${s}`;
}

export function Audit() {
  const [users, setUsers] = useState<UserStats[]>([]);
  const [events, setEvents] = useState<AuditEvent[]>([]);
  const [userId, setUserId] = useState<string | undefined>(undefined);
  const [kind, setKind] = useState<string | undefined>(undefined);
  const [nextCursor, setNextCursor] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    adminApi.listUsers().then((j) => setUsers(j.users)).catch(() => {});
  }, []);

  async function load(reset = true) {
    setLoading(true);
    try {
      const params: Parameters<typeof adminApi.listAudit>[0] = { limit: 50 };
      if (userId) params.user_id = userId;
      if (kind) params.kind = kind;
      if (!reset && nextCursor !== null) params.before_ms = nextCursor;
      const j = await adminApi.listAudit(params);
      setEvents(reset ? j.events : [...events, ...j.events]);
      setNextCursor(j.next_before_ms);
    } catch (e) {
      message.error(`加载失败: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    load(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [userId, kind]);

  const userEmailById = (id: string | null) =>
    id ? users.find((u) => u.id === id)?.email ?? id : '匿名';

  const columns: ColumnsType<AuditEvent> = [
    {
      title: '时间',
      dataIndex: 'created_ms',
      key: 'time',
      width: 170,
      render: (ms: number) => (
        <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
          {fmtTime(ms)}
        </Text>
      ),
    },
    {
      title: '事件',
      dataIndex: 'kind',
      key: 'kind',
      width: 110,
      render: (k: string) => <Tag>{KIND_LABEL[k] ?? k}</Tag>,
    },
    {
      title: '用户',
      dataIndex: 'user_id',
      key: 'user',
      width: 220,
      render: (id: string | null) => userEmailById(id),
    },
    {
      title: 'target',
      dataIndex: 'target_id',
      key: 'target',
      width: 180,
      render: (t: string | null) =>
        t ? (
          <Text type="secondary" style={{ fontFamily: 'monospace', fontSize: 11 }}>
            {t}
          </Text>
        ) : (
          '—'
        ),
    },
    {
      title: 'in / out',
      key: 'tokens',
      width: 110,
      align: 'right',
      render: (_, e) =>
        e.tokens_in || e.tokens_out ? (
          <Text type="secondary" style={{ fontSize: 12 }}>
            {e.tokens_in} / {e.tokens_out}
          </Text>
        ) : (
          '—'
        ),
    },
    {
      title: 'meta',
      dataIndex: 'meta_json',
      key: 'meta',
      render: (m: string | null) =>
        m ? (
          <Text
            type="secondary"
            style={{ fontFamily: 'monospace', fontSize: 11, wordBreak: 'break-all' }}
          >
            {m}
          </Text>
        ) : (
          ''
        ),
    },
  ];

  return (
    <Card>
      <Title level={4} style={{ marginTop: 0 }}>
        审计日志
      </Title>
      <Space style={{ marginBottom: 16 }} wrap>
        <Select
          allowClear
          placeholder="用户（全部）"
          style={{ minWidth: 220 }}
          value={userId}
          onChange={setUserId}
          options={users.map((u) => ({ value: u.id, label: u.email }))}
        />
        <Select
          allowClear
          placeholder="事件（全部）"
          style={{ minWidth: 140 }}
          value={kind}
          onChange={setKind}
          options={Object.entries(KIND_LABEL).map(([k, l]) => ({ value: k, label: l }))}
        />
        <Button icon={<ReloadOutlined />} onClick={() => load(true)}>
          刷新
        </Button>
      </Space>
      <Table<AuditEvent>
        rowKey="id"
        loading={loading}
        columns={columns}
        dataSource={events}
        pagination={false}
        size="middle"
        scroll={{ x: 'max-content' }}
      />
      {nextCursor !== null && events.length > 0 && (
        <div style={{ marginTop: 16, textAlign: 'center' }}>
          <Button loading={loading} onClick={() => load(false)}>
            加载更多
          </Button>
        </div>
      )}
    </Card>
  );
}
