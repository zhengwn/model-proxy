import React from "react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import KiroPanel from "../KiroPanel";
import { LocaleProvider } from "../../i18n";

const mockInvoke = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

describe("KiroPanel", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("clears credential loading after initial load in StrictMode", async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "kiro_list_credentials":
          return Promise.resolve({ credentials: [] });
        case "kiro_get_endpoint_health":
          return Promise.resolve({ endpoints: [] });
        case "kiro_get_thinking":
          return Promise.resolve({ mode: "auto" });
        case "kiro_get_settings":
          return Promise.resolve({ preferred_endpoint: "auto", endpoint_fallback: true });
        case "kiro_get_lb_config":
          return Promise.resolve({ mode: "priority" });
        default:
          return Promise.resolve({});
      }
    });

    render(
      <React.StrictMode>
        <LocaleProvider>
          <KiroPanel />
        </LocaleProvider>
      </React.StrictMode>
    );

    await screen.findByText(/No accounts configured|当前没有任何账号/);

    await waitFor(() => {
      expect(document.querySelector(".ant-spin-spinning")).not.toBeInTheDocument();
    });
  });
});
