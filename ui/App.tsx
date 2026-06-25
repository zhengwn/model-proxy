import { useState } from "react";
import { ConfigProvider, Layout, Menu, Badge, Spin, theme, Dropdown, Button } from "antd";
import type { MenuProps } from "antd";
import {
  DashboardOutlined,
  CloudServerOutlined,
  ApiOutlined,
  NodeIndexOutlined,
  FileTextOutlined,
  GlobalOutlined,
  SunOutlined,
  MoonOutlined,
  DesktopOutlined,
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
import { type Locale } from "./i18n/locales";


const { Header, Sider, Content } = Layout;

interface AppLayoutProps {
  themeMode: ThemeMode;
  resolved: "dark" | "light";
  onThemeChange: (mode: ThemeMode) => void;
}

function AppLayout({ themeMode, resolved, onThemeChange }: AppLayoutProps) {
  const { token } = theme.useToken();
  const [activePage, setActivePage] = useState("status");
  const { status } = useServiceStatus();
  const { activeProvider } = useProviders();
  const isRunning = status?.running ?? false;
  const { t, setLocale } = useLocale();

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

  const themeMenuItems = [
    { key: "light", icon: <SunOutlined />, label: t("theme.light") },
    { key: "dark", icon: <MoonOutlined />, label: t("theme.dark") },
    { key: "system", icon: <DesktopOutlined />, label: t("theme.system") },
  ];

  const localeMenuItems = [
    { key: "zh", label: "中文" },
    { key: "en", label: "English" },
  ];

  const themeIcon = themeMode === "dark" ? <MoonOutlined /> : themeMode === "light" ? <SunOutlined /> : <DesktopOutlined />;

  return (
    <Layout style={{ minHeight: "100vh", background: token.colorBgLayout }}>
      <Sider 
        width={180} 
        theme={resolved === "dark" ? "dark" : "light"}
        style={{ 
          margin: "16px 0 16px 16px",
          borderRadius: 12,
          border: `1px solid ${token.colorBorderSecondary}`,
          background: token.colorBgContainer,
          overflow: "hidden"
        }}
      >
        <div
          style={{
            height: 56,
            display: "flex",
            alignItems: "center",
            gap: 10,
            padding: "0 16px",
            borderBottom: `1px solid ${token.colorBorderSecondary}`,
          }}
        >
          <div
            style={{
              width: 28,
              height: 28,
              borderRadius: 8,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              background: "linear-gradient(135deg, #6366f1 0%, #4f46e5 55%, #7c3aed 100%)",
              boxShadow: "0 2px 8px rgba(79,70,229,0.35)",
              flexShrink: 0,
            }}
          >
            <ApiOutlined style={{ color: "#fff", fontSize: 15 }} />
          </div>
          <span
            style={{
              color: token.colorText,
              fontSize: 15,
              fontWeight: 600,
              letterSpacing: 0.2,
            }}
          >
            Model Proxy
          </span>
        </div>
        <Menu
          mode="inline"
          theme={resolved === "dark" ? "dark" : "light"}
          selectedKeys={[activePage]}
          onClick={(e) => setActivePage(e.key)}
          items={menuItems}
          style={{ background: "transparent", borderRight: "none" }}
        />
      </Sider>
      <Layout>
        <Header
          style={{
            background: "transparent",
            borderBottom: "none",
            display: "flex",
            alignItems: "center",
            justifyContent: "flex-end",
            padding: "0 24px",
            height: 48,
            lineHeight: "48px",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <Dropdown menu={{ items: localeMenuItems, onClick: (e) => setLocale(e.key as Locale) }} placement="bottomRight">
              <Button type="text" icon={<GlobalOutlined />} style={{ color: token.colorTextSecondary }} />
            </Dropdown>
            <Dropdown menu={{ items: themeMenuItems, onClick: (e) => onThemeChange(e.key as ThemeMode) }} placement="bottomRight">
              <Button type="text" icon={themeIcon} style={{ color: token.colorTextSecondary }} />
            </Dropdown>
            {status ? (
              <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
                <Badge
                  status={isRunning ? "success" : "error"}
                  text={
                    <span style={{ fontSize: 13, color: token.colorText }}>
                      {isRunning ? t("status.running") : t("status.stopped")}
                    </span>
                  }
                />
                {activeProvider && (
                  <span
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      height: 24,
                      lineHeight: "24px",
                      fontSize: 12,
                      color: token.colorText,
                      background: token.colorBgContainer,
                      padding: "0 10px",
                      borderRadius: 12,
                      border: `1px solid ${token.colorBorderSecondary}`,
                      boxShadow: "0 1px 2px rgba(0,0,0,0.03)"
                    }}
                  >
                    {activeProvider}
                  </span>
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
            background: token.colorBgLayout,
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
            colorPrimary: "#4f46e5", // Refined indigo — distinct from stock AntD blue
            colorInfo: "#4f46e5",
            borderRadius: 12,
            fontFamily:
              "-apple-system, BlinkMacSystemFont, 'SF Pro Text', 'PingFang SC', 'HarmonyOS Sans SC', 'Helvetica Neue', sans-serif",
            ...(resolved === "dark"
              ? {
                  colorBgLayout: "#0c0d12",
                  colorBgContainer: "#16181f",
                  colorBgElevated: "#1d1f28",
                  colorBorderSecondary: "#262833",
                }
              : {
                  colorBgLayout: "#f5f6f9",
                  colorBgContainer: "#ffffff",
                  colorBorderSecondary: "#eceef2",
                }),
          },
          components: {
            Menu: {
              itemMarginInline: 12,
              itemBorderRadius: 8,
              itemSelectedBg:
                resolved === "dark" ? "rgba(79,70,229,0.18)" : "rgba(79,70,229,0.08)",
              itemSelectedColor: "#4f46e5",
            },
            Card: {
              boxShadowTertiary:
                resolved === "dark"
                  ? "0 1px 2px rgba(0,0,0,0.4), 0 4px 16px rgba(0,0,0,0.28)"
                  : "0 1px 2px rgba(16,24,40,0.04), 0 6px 20px rgba(16,24,40,0.06)",
            },
          },
        }}
      >
        <AppLayout themeMode={mode} resolved={resolved} onThemeChange={setMode} />
      </ConfigProvider>
    </ErrorBoundary>
  );
}

import { ProvidersProvider } from "./hooks/useProviders";

function AppRoot() {
  return (
    <LocaleProvider>
      <ProvidersProvider>
        <App />
      </ProvidersProvider>
    </LocaleProvider>
  );
}

export default AppRoot;
