import { useState } from "react";
import { List, Tag, Button, Space, message, Typography } from "antd";
import {
  CheckCircleOutlined,
  SwapOutlined,
  EditOutlined,
  DeleteOutlined,
  ThunderboltOutlined,
} from "@ant-design/icons";
import { invoke } from "@tauri-apps/api/core";
import type { ProviderConfig, TestProviderResult } from "../types";

const { Text } = Typography;

export interface ProviderListProps {
  providers: ProviderConfig[];
  activeProvider: string;
  switching: boolean;
  onSwitch: (name: string) => Promise<void>;
  onEdit: (provider: ProviderConfig) => void;
  onDelete: (name: string) => void;
}

export function ProviderList({
  providers,
  activeProvider,
  switching,
  onSwitch,
  onEdit,
  onDelete,
}: ProviderListProps) {
  const [testing, setTesting] = useState<string | null>(null);

  const handleSwitch = async (name: string) => {
    try {
      await onSwitch(name);
      message.success(`已切换到 Provider: ${name}`, 3);
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      message.error({ content: `切换失败: ${errMsg}`, duration: 0 });
    }
  };

  const handleTest = async (provider: ProviderConfig) => {
    setTesting(provider.name);
    try {
      const result = await invoke<TestProviderResult>("test_provider", { provider });
      if (result.success) {
        message.success(
          `${provider.name} 连接成功 (${result.latency_ms}ms)${result.model ? ` - 模型: ${result.model}` : ""}`,
          5
        );
      } else {
        message.error({
          content: `${provider.name} 连接失败: ${result.error}`,
          duration: 8,
        });
      }
    } catch (e) {
      message.error(`测试失败: ${typeof e === "string" ? e : String(e)}`);
    } finally {
      setTesting(null);
    }
  };

  return (
    <List
      dataSource={providers}
      renderItem={(provider) => {
        const isActive = provider.name === activeProvider;
        const isTesting = testing === provider.name;
        return (
          <List.Item
            actions={[
              <Button
                key="test"
                icon={<ThunderboltOutlined />}
                size="small"
                loading={isTesting}
                onClick={() => handleTest(provider)}
              >
                测试
              </Button>,
              !isActive && (
                <Button
                  key="switch"
                  icon={<SwapOutlined />}
                  size="small"
                  disabled={switching}
                  loading={switching}
                  onClick={() => handleSwitch(provider.name)}
                >
                  切换
                </Button>
              ),
              <Button
                key="edit"
                icon={<EditOutlined />}
                size="small"
                onClick={() => onEdit(provider)}
              />,
              <Button
                key="delete"
                icon={<DeleteOutlined />}
                size="small"
                danger
                onClick={() => onDelete(provider.name)}
              />,
            ].filter(Boolean)}
          >
            <List.Item.Meta
              title={
                <Space>
                  <span style={{ fontWeight: isActive ? 700 : 400 }}>
                    {provider.name}
                  </span>
                  {isActive && (
                    <Tag icon={<CheckCircleOutlined />} color="success">
                      活跃
                    </Tag>
                  )}
                  <Tag color={provider.format === "openai" ? "blue" : "purple"}>
                    {provider.format}
                  </Tag>
                </Space>
              }
              description={
                <Space direction="vertical" size={0}>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    模型: {provider.model}
                  </Text>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {provider.base_url}
                  </Text>
                </Space>
              }
            />
          </List.Item>
        );
      }}
    />
  );
}
