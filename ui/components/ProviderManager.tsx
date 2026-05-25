import { useState } from "react";
import { Card, Spin, Alert, Button, Modal, message } from "antd";
import { PlusOutlined } from "@ant-design/icons";
import { useProviders } from "../hooks/useProviders";
import { ProviderList } from "./ProviderList";
import { ProviderForm } from "./ProviderForm";
import type { ProviderConfig } from "../types";

type View = "list" | "add" | "edit";

export function ProviderManager() {
  const {
    providers,
    activeProvider,
    loading,
    error,
    switching,
    switchProvider,
    addProvider,
    updateProvider,
    deleteProvider,
    loadProviders,
  } = useProviders();

  const [view, setView] = useState<View>("list");
  const [editingProvider, setEditingProvider] = useState<ProviderConfig | undefined>();

  // When providers finish loading and list is empty, auto-show add form
  const effectiveView = !loading && providers.length === 0 && view === "list" && !error ? "add" : view;

  const handleEdit = (provider: ProviderConfig) => {
    setEditingProvider(provider);
    setView("edit");
  };

  const handleDelete = (name: string) => {
    const isActive = name === activeProvider;

    if (isActive && providers.length > 1) {
      Modal.warning({
        title: "无法删除",
        content: "不能删除当前活跃的 Provider，请先切换到其他 Provider。",
        okText: "知道了",
      });
      return;
    }

    Modal.confirm({
      title: "确认删除",
      content: `确定要删除 Provider "${name}" 吗？`,
      okText: "删除",
      okType: "danger",
      cancelText: "取消",
      onOk: async () => {
        try {
          await deleteProvider(name);
          message.success(`已删除 Provider: ${name}`);
        } catch {
          // Error is already set in the hook
        }
      },
    });
  };

  const handleAdd = async (config: ProviderConfig) => {
    await addProvider(config);
    message.success(`已添加 Provider: ${config.name}`);
    setView("list");
  };

  const handleUpdate = async (config: ProviderConfig) => {
    // Pass original name so backend can handle rename
    await updateProvider(config, editingProvider?.name);
    message.success(`已更新 Provider: ${config.name}`);
    setView("list");
    setEditingProvider(undefined);
  };

  const handleCancel = () => {
    setView("list");
    setEditingProvider(undefined);
  };

  if (loading) {
    return <Spin tip="加载 Provider 列表..." style={{ display: "block", marginTop: 48 }} />;
  }

  if (error && providers.length === 0) {
    return (
      <Card>
        <Alert
          message="加载失败"
          description={error}
          type="error"
          showIcon
          action={
            <Button size="small" onClick={loadProviders}>
              重试
            </Button>
          }
        />
      </Card>
    );
  }

  if (effectiveView === "add") {
    return (
      <ProviderForm
        mode="add"
        existingNames={providers.map((p) => p.name)}
        onSubmit={handleAdd}
        onCancel={handleCancel}
        defaultTemplate="DeepSeek"
      />
    );
  }

  if (effectiveView === "edit" && editingProvider) {
    return (
      <ProviderForm
        mode="edit"
        initialValues={editingProvider}
        existingNames={providers.map((p) => p.name)}
        onSubmit={handleUpdate}
        onCancel={handleCancel}
      />
    );
  }

  return (
    <div>
      {error && (
        <Alert
          message={error}
          type="error"
          showIcon
          closable
          style={{ marginBottom: 16 }}
        />
      )}
      <div style={{ marginBottom: 16 }}>
        <Button
          type="primary"
          icon={<PlusOutlined />}
          onClick={() => setView("add")}
        >
          添加 Provider
        </Button>
      </div>
      {providers.length === 0 && !error ? (
        <Alert
          message="尚未配置 Provider"
          description="请点击上方按钮添加第一个 Provider。"
          type="info"
          showIcon
        />
      ) : (
        <ProviderList
          providers={providers}
          activeProvider={activeProvider}
          switching={switching}
          onSwitch={switchProvider}
          onEdit={handleEdit}
          onDelete={handleDelete}
        />
      )}
    </div>
  );
}
