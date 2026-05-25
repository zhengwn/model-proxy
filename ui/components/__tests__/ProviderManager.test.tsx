import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ProviderManager } from "../ProviderManager";
import type { ProvidersInfo } from "../../types";

// Mock Tauri invoke
const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

const mockProvidersInfo: ProvidersInfo = {
  providers: [
    {
      name: "openai",
      base_url: "https://api.openai.com/v1",
      api_key: "sk-openai",
      model: "gpt-4o",
      format: "openai",
      quirks: {
        reasoning_all_or_nothing: false,
        no_json_schema: false,
        supports_reasoning_effort: false,
        max_reasoning_effort: "high",
      },
    },
    {
      name: "anthropic",
      base_url: "https://api.anthropic.com",
      api_key: "sk-ant",
      model: "claude-sonnet-4-20250514",
      format: "anthropic",
      quirks: {
        reasoning_all_or_nothing: false,
        no_json_schema: false,
        supports_reasoning_effort: false,
        max_reasoning_effort: "high",
      },
    },
  ],
  active_provider: "openai",
};

describe("ProviderManager", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("shows loading spinner initially", () => {
    mockInvoke.mockReturnValue(new Promise(() => {})); // Never resolves
    render(<ProviderManager />);

    // Ant Design Spin renders with aria-busy="true" and role="status" via aria-live="polite"
    const spinner = document.querySelector(".ant-spin-spinning");
    expect(spinner).toBeInTheDocument();
  });

  it("shows error alert on load failure", async () => {
    mockInvoke.mockRejectedValue("Network error");
    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getByText("加载失败")).toBeInTheDocument();
    });

    expect(screen.getByText("Network error")).toBeInTheDocument();
  });

  it("renders provider list after successful load", async () => {
    mockInvoke.mockResolvedValue(mockProvidersInfo);
    render(<ProviderManager />);

    await waitFor(() => {
      // "openai" appears as both name and format tag, use getAllByText
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });

    // "anthropic" also appears as both name and format tag
    expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    // Active provider tag
    expect(screen.getByText("活跃")).toBeInTheDocument();
  });

  it("switch button triggers IPC and updates state", async () => {
    const user = userEvent.setup();
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === "get_providers") return Promise.resolve(mockProvidersInfo);
      if (cmd === "switch_provider") return Promise.resolve();
      return Promise.resolve();
    });

    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    });

    // The switch button has a "swap" icon aria-label and text "切换"
    const switchButton = screen.getByRole("button", { name: /切换/ });
    await user.click(switchButton);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith("switch_provider", {
        name: "anthropic",
      });
    });
  });

  it("delete button shows confirmation modal for non-active provider", async () => {
    const user = userEvent.setup();
    mockInvoke.mockResolvedValue(mockProvidersInfo);

    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    });

    // Find delete buttons by their aria-label
    const deleteButtons = screen.getAllByRole("button", { name: /delete/i });
    // Click the delete button for the non-active provider (anthropic - second one)
    await user.click(deleteButtons[1]);

    await waitFor(() => {
      expect(screen.getByText("确认删除")).toBeInTheDocument();
    });

    expect(
      screen.getByText('确定要删除 Provider "anthropic" 吗？')
    ).toBeInTheDocument();
  });

  it("delete active provider shows warning modal", async () => {
    const user = userEvent.setup();
    mockInvoke.mockResolvedValue(mockProvidersInfo);

    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });

    // Click the delete button for the active provider (openai - first one)
    const deleteButtons = screen.getAllByRole("button", { name: /delete/i });
    await user.click(deleteButtons[0]);

    await waitFor(() => {
      expect(screen.getByText("无法删除")).toBeInTheDocument();
    });

    expect(
      screen.getByText(
        "不能删除当前活跃的 Provider，请先切换到其他 Provider。"
      )
    ).toBeInTheDocument();
  });

  it("shows retry button on error", async () => {
    mockInvoke.mockRejectedValue("Connection failed");
    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getByText("加载失败")).toBeInTheDocument();
    });

    // Ant Design Button renders Chinese text with spaces between chars
    // Use a role-based query with regex
    expect(screen.getByRole("button", { name: /重.*试/ })).toBeInTheDocument();
  });

  it("retry button reloads providers", async () => {
    const user = userEvent.setup();
    mockInvoke
      .mockRejectedValueOnce("Connection failed")
      .mockResolvedValueOnce(mockProvidersInfo);

    render(<ProviderManager />);

    await waitFor(() => {
      expect(screen.getByText("加载失败")).toBeInTheDocument();
    });

    const retryButton = screen.getByRole("button", { name: /重.*试/ });
    await user.click(retryButton);

    await waitFor(() => {
      // After retry, provider list should be visible
      expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    });
  });
});
