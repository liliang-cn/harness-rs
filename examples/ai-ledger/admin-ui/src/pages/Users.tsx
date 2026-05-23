import { useEffect, useState } from 'react';
import {
  Table,
  Tag,
  Select,
  Button,
  Modal,
  Space,
  Typography,
  Card,
  message,
} from 'antd';
import { DeleteOutlined, KeyOutlined, LinkOutlined } from '@ant-design/icons';
import type { ColumnsType } from 'antd/es/table';
import { adminApi, type UserStats } from '@/lib/api';
import { InvitesModal } from '@/pages/InvitesModal';

const { Title, Text, Paragraph } = Typography;

const TIER_COLOR: Record<string, string> = {
  admin: 'red',
  paid: 'blue',
  trial: 'default',
};

function fmtDate(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  return d.toISOString().slice(0, 10);
}

export function Users() {
  const [users, setUsers] = useState<UserStats[] | null>(null);
  const [pricedAtModel, setPricedAtModel] = useState<string>('');
  const [loading, setLoading] = useState(false);
  const [deleteUser, setDeleteUser] = useState<UserStats | null>(null);
  const [resetUser, setResetUser] = useState<UserStats | null>(null);
  const [tempPw, setTempPw] = useState<string>('');
  const [invitesOpen, setInvitesOpen] = useState(false);

  async function refresh() {
    setLoading(true);
    try {
      const j = await adminApi.listUsers();
      setUsers(j.users);
      setPricedAtModel(j.priced_at_model ?? '');
    } catch (e) {
      message.error(`加载失败: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    refresh();
  }, []);

  async function changeTier(u: UserStats, tier: string) {
    if (tier === u.tier) return;
    try {
      await adminApi.patchUser(u.id, { tier });
      message.success(`已改为 ${tier}`);
      refresh();
    } catch (e) {
      message.error(`改 tier 失败: ${(e as Error).message}`);
    }
  }

  async function confirmDelete() {
    if (!deleteUser) return;
    try {
      await adminApi.deleteUser(deleteUser.id);
      message.success('已删除');
      setDeleteUser(null);
      refresh();
    } catch (e) {
      message.error(`删除失败: ${(e as Error).message}`);
    }
  }

  async function confirmReset() {
    if (!resetUser) return;
    try {
      const j = await adminApi.resetPassword(resetUser.id);
      setTempPw(j.temp_password);
    } catch (e) {
      message.error(`重置失败: ${(e as Error).message}`);
    }
  }

  // Resolve invited_by (user id) → inviter's email by looking it up in the
  // same users list.
  const inviterEmail = (uid: string | null): string | null => {
    if (!uid) return null;
    return users?.find((u) => u.id === uid)?.email ?? uid.slice(0, 8) + '…';
  };

  const columns: ColumnsType<UserStats> = [
    {
      title: '邮箱',
      dataIndex: 'email',
      key: 'email',
      render: (_, u) => (
        <div>
          <div>{u.email}</div>
          <Text type="secondary" style={{ fontSize: 11, fontFamily: 'monospace' }}>
            {u.id}
          </Text>
        </div>
      ),
    },
    {
      title: '邀请来源',
      key: 'invited_by',
      width: 180,
      render: (_, u) => {
        if (!u.invited_by) {
          return <Text type="secondary">—</Text>;
        }
        return (
          <div>
            <div style={{ fontSize: 12 }}>by {inviterEmail(u.invited_by)}</div>
            {u.invite_code_used && (
              <Text
                type="secondary"
                style={{ fontSize: 11, fontFamily: 'monospace' }}
                copyable={{ text: u.invite_code_used, tooltips: ['复制码', '已复制'] }}
              >
                {u.invite_code_used}
              </Text>
            )}
          </div>
        );
      },
    },
    {
      title: 'Tier',
      dataIndex: 'tier',
      key: 'tier',
      width: 90,
      filters: [
        { text: 'admin', value: 'admin' },
        { text: 'paid', value: 'paid' },
        { text: 'trial', value: 'trial' },
      ],
      onFilter: (v, u) => u.tier === v,
      render: (t: string) => <Tag color={TIER_COLOR[t] ?? 'default'}>{t}</Tag>,
    },
    {
      title: '注册',
      dataIndex: 'created_at',
      key: 'created_at',
      width: 110,
      sorter: (a, b) => (a.created_at < b.created_at ? -1 : 1),
      render: fmtDate,
    },
    {
      title: '流水',
      dataIndex: 'txn_count',
      key: 'txn_count',
      width: 80,
      align: 'right',
      sorter: (a, b) => a.txn_count - b.txn_count,
    },
    {
      title: '会话',
      dataIndex: 'chat_count',
      key: 'chat_count',
      width: 80,
      align: 'right',
      sorter: (a, b) => a.chat_count - b.chat_count,
    },
    {
      title: 'Tokens',
      key: 'tokens',
      width: 120,
      align: 'right',
      sorter: (a, b) => a.tokens_in + a.tokens_out - (b.tokens_in + b.tokens_out),
      render: (_, u) => (u.tokens_in + u.tokens_out).toLocaleString(),
    },
    {
      title: 'Cost (USD)',
      key: 'cost',
      width: 110,
      align: 'right',
      sorter: (a, b) => (a.cost_usd ?? 0) - (b.cost_usd ?? 0),
      render: (_, u) => {
        const c = u.cost_usd ?? 0;
        // Show 2 dp once we cross 1¢; below that, 4 dp keeps tiny-bill rounding honest.
        const txt = c >= 0.01 ? `$${c.toFixed(2)}` : c > 0 ? `$${c.toFixed(4)}` : '—';
        return <span style={{ fontFamily: 'ui-monospace, Menlo, monospace' }}>{txt}</span>;
      },
    },
    {
      title: '最后访问',
      dataIndex: 'last_seen_at',
      key: 'last_seen_at',
      width: 110,
      sorter: (a, b) => (a.last_seen_at ?? '').localeCompare(b.last_seen_at ?? ''),
      render: fmtDate,
    },
    {
      title: '操作',
      key: 'actions',
      width: 260,
      align: 'right',
      render: (_, u) => (
        <Space>
          <Select
            size="small"
            value={u.tier}
            style={{ width: 90 }}
            onChange={(v) => changeTier(u, v)}
            options={[
              { value: 'trial', label: 'trial' },
              { value: 'paid', label: 'paid' },
              { value: 'admin', label: 'admin' },
            ]}
          />
          <Button
            size="small"
            icon={<KeyOutlined />}
            onClick={() => {
              setResetUser(u);
              setTempPw('');
            }}
            title="重置密码"
          />
          <Button
            size="small"
            danger
            icon={<DeleteOutlined />}
            onClick={() => setDeleteUser(u)}
            title="删除"
          />
        </Space>
      ),
    },
  ];

  return (
    <Card>
      <Space style={{ width: '100%', justifyContent: 'space-between', marginBottom: 12 }}>
        <div>
          <Title level={4} style={{ margin: 0 }}>
            用户 {users ? `(${users.length})` : ''}
          </Title>
          {pricedAtModel && (
            <Text type="secondary" style={{ fontSize: 11 }}>
              cost 按当前模型 <Text code>{pricedAtModel}</Text> 估算
            </Text>
          )}
        </div>
        <Button
          icon={<LinkOutlined />}
          onClick={() => setInvitesOpen(true)}
        >
          邀请码
        </Button>
      </Space>
      <Table<UserStats>
        rowKey="id"
        loading={loading}
        columns={columns}
        dataSource={users ?? []}
        pagination={{ pageSize: 20, showSizeChanger: false }}
        size="middle"
        scroll={{ x: 'max-content' }}
      />

      <Modal
        open={deleteUser !== null}
        title={`删除用户 ${deleteUser?.email ?? ''}`}
        okText="确认删除"
        okButtonProps={{ danger: true }}
        cancelText="取消"
        onCancel={() => setDeleteUser(null)}
        onOk={confirmDelete}
      >
        <Paragraph>
          这会级联删除该用户所有流水、持仓、聊天记录、订阅、记忆。
          <Text strong>不可恢复。</Text>
        </Paragraph>
        <Text code style={{ fontSize: 12 }}>
          {deleteUser?.id}
        </Text>
      </Modal>

      <Modal
        open={resetUser !== null}
        title={`重置密码 ${resetUser?.email ?? ''}`}
        footer={
          !tempPw ? (
            <Space>
              <Button onClick={() => setResetUser(null)}>取消</Button>
              <Button type="primary" onClick={confirmReset}>
                生成临时密码
              </Button>
            </Space>
          ) : (
            <Button
              type="primary"
              onClick={() => {
                navigator.clipboard?.writeText(tempPw);
                message.success('已复制');
                setResetUser(null);
                setTempPw('');
              }}
            >
              复制并关闭
            </Button>
          )
        }
        onCancel={() => {
          setResetUser(null);
          setTempPw('');
        }}
      >
        {!tempPw ? (
          <Paragraph>
            生成一个 12 位临时密码，同时踢掉该用户所有现有会话。
          </Paragraph>
        ) : (
          <>
            <Paragraph>
              新临时密码（<Text strong>只显示一次</Text>，复制后转告该用户）:
            </Paragraph>
            <Text
              code
              copyable
              style={{ fontSize: 16, padding: '8px 12px', display: 'inline-block' }}
            >
              {tempPw}
            </Text>
          </>
        )}
      </Modal>

      <InvitesModal open={invitesOpen} onClose={() => setInvitesOpen(false)} />
    </Card>
  );
}
