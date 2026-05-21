import { useEffect, useState } from 'react';
import {
  Modal,
  Table,
  Button,
  Space,
  Tag,
  Typography,
  message,
  Empty,
} from 'antd';
import { PlusOutlined, LinkOutlined, CopyOutlined } from '@ant-design/icons';
import type { ColumnsType } from 'antd/es/table';
import { adminApi, type Invite } from '@/lib/api';

const { Text } = Typography;

function fmtDate(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  return d.toISOString().slice(0, 10);
}

function inviteUrl(code: string): string {
  return `${location.origin}/?invite=${encodeURIComponent(code)}`;
}

async function copyText(text: string, okMsg: string) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.cssText = 'position:fixed;left:-9999px';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    message.success(okMsg);
  } catch {
    message.error('复制失败');
  }
}

interface Props {
  open: boolean;
  onClose: () => void;
}

export function InvitesModal({ open, onClose }: Props) {
  const [invites, setInvites] = useState<Invite[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [creating, setCreating] = useState(false);

  async function refresh() {
    setLoading(true);
    try {
      const j = await adminApi.listInvites();
      setInvites(j.invites);
    } catch (e) {
      message.error(`加载失败: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }

  async function create() {
    setCreating(true);
    try {
      await adminApi.createInvite();
      message.success('已生成');
      await refresh();
    } catch (e) {
      message.error(`生成失败: ${(e as Error).message}`);
    } finally {
      setCreating(false);
    }
  }

  // Refresh whenever the modal becomes visible — invites can be consumed by
  // registrations between opens.
  useEffect(() => {
    if (open) refresh();
  }, [open]);

  const columns: ColumnsType<Invite> = [
    {
      title: '邀请码',
      dataIndex: 'code',
      key: 'code',
      render: (code: string) => (
        <Text code copyable={{ text: code, tooltips: ['复制', '已复制'] }}>
          {code}
        </Text>
      ),
    },
    {
      title: '剩余',
      dataIndex: 'uses_remaining',
      key: 'uses_remaining',
      width: 80,
      align: 'right',
      render: (n: number) => <Tag color="green">{n} 次</Tag>,
    },
    {
      title: '生成',
      dataIndex: 'created_at',
      key: 'created_at',
      width: 110,
      render: fmtDate,
    },
    {
      title: '操作',
      key: 'actions',
      width: 220,
      align: 'right',
      render: (_, inv) => (
        <Space>
          <Button
            size="small"
            icon={<CopyOutlined />}
            onClick={() => copyText(inv.code, '✓ 邀请码已复制')}
          >
            码
          </Button>
          <Button
            size="small"
            type="primary"
            icon={<LinkOutlined />}
            onClick={() => copyText(inviteUrl(inv.code), '✓ 邀请链接已复制')}
          >
            链接
          </Button>
        </Space>
      ),
    },
  ];

  return (
    <Modal
      open={open}
      onCancel={onClose}
      title="邀请码"
      width={720}
      footer={null}
      destroyOnClose
    >
      <Space style={{ width: '100%', justifyContent: 'space-between' }} align="start">
        <Text type="secondary" style={{ fontSize: 12 }}>
          只显示尚未被使用的码。已注册的邀请关系会出现在用户表的「邀请来源」列。
        </Text>
        <Button
          type="primary"
          icon={<PlusOutlined />}
          loading={creating}
          onClick={create}
        >
          生成
        </Button>
      </Space>
      <div style={{ marginTop: 12 }}>
        <Table<Invite>
          rowKey="code"
          loading={loading}
          columns={columns}
          dataSource={invites ?? []}
          pagination={false}
          size="small"
          locale={{
            emptyText: (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description="没有未使用的邀请码"
              />
            ),
          }}
        />
      </div>
    </Modal>
  );
}
