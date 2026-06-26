import { useEffect, useState } from "react";
import { Spin, Alert, Button, Modal, Drawer, message, Card } from "antd";
import { PlusOutlined } from "@ant-design/icons";
import { useProviders } from "../hooks/useProviders";
import { ProviderList } from "./ProviderList";
import { ProviderForm } from "./ProviderForm";
import type { ProviderConfig } from "../types";
import { useLocale } from "../i18n";

interface ProviderManagerProps {
  /** When true, opens the Add Provider drawer automatically (e.g. from first-launch guide). */
  autoOpenAdd?: boolean;
  /** Called once the auto-open intent has been consumed. */
  onAutoOpenConsumed?: () => void;
}

export function ProviderManager({ autoOpenAdd, onAutoOpenConsumed }: ProviderManagerProps = {}) {
  const { t } = useLocale();
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

  const [drawerOpen, setDrawerOpen] = useState(false);
  const [editingProvider, setEditingProvider] = useState<ProviderConfig | undefined>();

  // Honor an external request to open the Add Provider drawer (first-launch guide).
  useEffect(() => {
    if (autoOpenAdd) {
      setEditingProvider(undefined);
      setDrawerOpen(true);
      onAutoOpenConsumed?.();
    }
  }, [autoOpenAdd, onAutoOpenConsumed]);

  const handleEdit = (provider: ProviderConfig) => {
    setEditingProvider(provider);
    setDrawerOpen(true);
  };

  const handleDelete = (name: string) => {
    const isActive = name === activeProvider;

    if (isActive && providers.length > 1) {
      Modal.warning({
        title: t("provider.cannotDelete"),
        content: t("provider.cannotDeleteActive"),
        okText: t("common.ok"),
      });
      return;
    }

    Modal.confirm({
      title: t("common.confirmDelete"),
      content: t("provider.confirmDeleteMsg", { name }),
      okText: t("common.delete"),
      okType: "danger",
      cancelText: t("common.cancel"),
      onOk: async () => {
        try {
          await deleteProvider(name);
          message.success(t("provider.deleted", { name }));
        } catch {
          // Error is already set in the hook
        }
      },
    });
  };

  const handleAdd = async (config: ProviderConfig) => {
    await addProvider(config);
    message.success(t("provider.added", { name: config.name }));
    closeDrawer();
  };

  const handleUpdate = async (config: ProviderConfig) => {
    await updateProvider(config, editingProvider?.name);
    message.success(t("provider.updated", { name: config.name }));
    closeDrawer();
  };

  const closeDrawer = () => {
    setDrawerOpen(false);
    setEditingProvider(undefined);
  };

  if (loading) {
    return <Spin tip={t("provider.loading")} style={{ display: "block", marginTop: 48 }} />;
  }

  if (error && providers.length === 0) {
    return (
      <div>
        <Alert
          title={t("provider.loadFailed")}
          description={error}
          type="error"
          showIcon
          action={
            <Button size="small" onClick={loadProviders}>
              {t("common.retry")}
            </Button>
          }
        />
      </div>
    );
  }

  const isEditing = !!editingProvider;

  return (
    <Card
      title={t("nav.providers")}
      extra={
        <Button
          type="primary"
          icon={<PlusOutlined />}
          onClick={() => {
            setEditingProvider(undefined);
            setDrawerOpen(true);
          }}
        >
          {t("provider.addProvider")}
        </Button>
      }
    >
      {error && (
        <Alert
          title={error}
          type="error"
          showIcon
          closable
          style={{ marginBottom: 16 }}
        />
      )}
      {providers.length === 0 && !error ? (
        <Alert
          title={t("provider.notConfigured")}
          description={t("provider.notConfiguredDesc")}
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

      <Drawer
        title={isEditing ? t("provider.editProvider") : t("provider.addProvider")}
        width={640}
        open={drawerOpen}
        onClose={closeDrawer}
        destroyOnClose
      >
        <ProviderForm
          mode={isEditing ? "edit" : "add"}
          initialValues={editingProvider}
          existingNames={providers.map((p) => p.name)}
          onSubmit={isEditing ? handleUpdate : handleAdd}
          onCancel={closeDrawer}
          defaultTemplate={isEditing ? undefined : "deepseek"}
        />
      </Drawer>
    </Card>
  );
}
