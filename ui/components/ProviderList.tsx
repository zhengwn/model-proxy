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
import { useLocale } from "../i18n";

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
  const { t } = useLocale();
  const [testing, setTesting] = useState<string | null>(null);

  const handleSwitch = async (name: string) => {
    try {
      await onSwitch(name);
      message.success(t("provider.switchSuccess", { name }), 3);
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      message.error({ content: t("provider.switchFailed", { error: errMsg }), duration: 0 });
    }
  };

  const handleTest = async (provider: ProviderConfig) => {
    setTesting(provider.name);
    try {
      const result = await invoke<TestProviderResult>("test_provider", { provider });
      if (result.success) {
        message.success(
          t("provider.testSuccess", {
            name: provider.name,
            latency: result.latency_ms,
            model: result.model ? ` - Model: ${result.model}` : "",
          }),
          5
        );
      } else {
        message.error({
          content: t("provider.switchFailed", { error: result.error ?? "" }),
          duration: 8,
        });
      }
    } catch (e) {
      message.error(t("provider.testFailed", { error: typeof e === "string" ? e : String(e) }));
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
                {t("common.test")}
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
                  {t("provider.switch")}
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
                      {t("provider.active")}
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
                    {t("provider.model", { model: provider.model })}
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
