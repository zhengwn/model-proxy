import { useState } from "react";
import { ConfigProvider, Layout, Menu, Badge, Tag, Segmented, Spin, theme } from "antd";
import type { MenuProps } from "antd";
import {
  DashboardOutlined,
  CloudServerOutlined,
  ApiOutlined,
  NodeIndexOutlined,
  FileTextOutlined,
} from "@ant-design/icons";
import StatusPanel from "./components/StatusPanel";
import { ProviderManager } from "./components/ProviderManager";
import { ModelRoutesEditor } from "./components/ModelRoutesEditor";
import LogViewer from "./components/LogViewer";
import KiroPanel from "./components/KiroPanel";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { useServiceStatus } from "./hooks/useServiceStatus";
import { useProviders } from "./hooks/useProviders";
import { useTheme } from "./hooks/useTheme";
import type { ThemeMode } from "./hooks/useTheme";
import { LocaleProvider, useLocale } from "./i18n";
import { localeNames, type Locale } from "./i18n/zh";

const { Header, Sider, Content } = Layout;

interface AppLayoutProps {
  themeMode: ThemeMode;
  resolvedTheme: "dark" | "light";
  onThemeChange: (mode: ThemeMode) => void;
}

function AppLayout({ themeMode, resolvedTheme, onThemeChange }: AppLayoutProps) {
  const [activePage, setActivePage] = useState("status");
  const { status } = useServiceStatus();
  const { activeProvider } = useProviders();
  const isRunning = status?.running ?? false;
  const isDark = resolvedTheme === "dark";
  const { t, locale, setLocale } = useLocale();

  const menuItems: MenuProps["items"] = [
    { key: "status", icon: <DashboardOutlined />, label: t("nav.overview") },
    { key: "providers", icon: <CloudServerOutlined />, label: t("nav.providers") },
    { key: "kiro", icon: <ApiOutlined />, label: t("nav.kiro") },
    { key: "routes", icon: <NodeIndexOutlined />, label: t("nav.modelRoutes") },
    { key: "logs", icon: <FileTextOutlined />, label: t("nav.requestLogs") },
  ];

  const PAGE_MAP: Record<string, React.ReactNode> = {
    status: <StatusPanel />,
    providers: <ProviderManager />,
    kiro: <KiroPanel />,
    routes: <ModelRoutesEditor />,
    logs: <LogViewer />,
  };

  const themeOptions = [
    { label: t("theme.dark"), value: "dark" },
    { label: t("theme.light"), value: "light" },
    { label: t("theme.system"), value: "system" },
  ];

  const localeOptions = Object.entries(localeNames).map(([value, label]) => ({
    label,
    value,
  }));

  return (
    <Layout style={{ minHeight: "100vh" }}>
      <Sider width={160} theme={isDark ? "dark" : "light"}>
        <div
          style={{
            height: 56,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            borderBottom: "1px solid",
            borderColor: isDark ? "rgba(255,255,255,0.08)" : "rgba(0,0,0,0.06)",
          }}
        >
          <span
            style={{
              color: isDark ? "#fff" : "#1a1a1a",
              fontSize: 15,
              fontWeight: 600,
            }}
          >
            Model Proxy
          </span>
        </div>
        <Menu
          mode="inline"
          selectedKeys={[activePage]}
          onClick={(e) => setActivePage(e.key)}
          items={menuItems}
          style={{ background: "transparent", borderRight: "none" }}
        />
      </Sider>
      <Layout>
        <Header
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "flex-end",
            padding: "0 24px",
            height: 48,
            lineHeight: "48px",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 16 }}>
            <Segmented
              size="small"
              value={locale}
              onChange={(v) => setLocale(v as Locale)}
              options={localeOptions}
            />
            <Segmented
              size="small"
              value={themeMode}
              onChange={(v) => onThemeChange(v as ThemeMode)}
              options={themeOptions}
            />
            {status ? (
              <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
                <Badge
                  status={isRunning ? "success" : "error"}
                  text={
                    <span style={{ fontSize: 13 }}>
                      {isRunning ? t("status.running") : t("status.stopped")}
                    </span>
                  }
                />
                {activeProvider && (
                  <Tag color="blue" style={{ margin: 0 }}>
                    {activeProvider}
                  </Tag>
                )}
              </div>
            ) : (
              <Spin size="small" />
            )}
          </div>
        </Header>
        <Content
          style={{
            padding: 20,
            overflow: "auto",
            height: "calc(100vh - 48px)",
          }}
        >
          {PAGE_MAP[activePage] ?? <StatusPanel />}
        </Content>
      </Layout>
    </Layout>
  );
}

function App() {
  const { mode, resolved, setMode } = useTheme();
  const { antdLocale } = useLocale();

  return (
    <ErrorBoundary>
      <ConfigProvider
        locale={antdLocale}
        theme={{
          algorithm: resolved === "dark" ? theme.darkAlgorithm : theme.defaultAlgorithm,
          token: {
            colorPrimary: "#1677ff",
            borderRadius: 6,
          },
        }}
      >
        <AppLayout themeMode={mode} resolvedTheme={resolved} onThemeChange={setMode} />
      </ConfigProvider>
    </ErrorBoundary>
  );
}

function AppRoot() {
  return (
    <LocaleProvider>
      <App />
    </LocaleProvider>
  );
}

export default AppRoot;
