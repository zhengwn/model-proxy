import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ProviderList } from "../ProviderList";
import type { ProviderConfig } from "../../types";

// Mock antd message to avoid act() warnings
vi.mock("antd", async () => {
  const actual = await vi.importActual<typeof import("antd")>("antd");
  return {
    ...actual,
    message: {
      success: vi.fn(),
      error: vi.fn(),
    },
  };
});

const mockProviders: ProviderConfig[] = [
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
  {
    name: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    api_key: "sk-ds",
    model: "deepseek-chat",
    format: "openai",
    quirks: {
      reasoning_all_or_nothing: true,
      no_json_schema: true,
      supports_reasoning_effort: true,
      max_reasoning_effort: "max",
    },
  },
];

describe("ProviderList", () => {
  const defaultProps = {
    providers: mockProviders,
    activeProvider: "openai",
    switching: false,
    onSwitch: vi.fn().mockResolvedValue(undefined),
    onEdit: vi.fn(),
    onDelete: vi.fn(),
  };

  it("renders all providers with their names", () => {
    render(<ProviderList {...defaultProps} />);

    // Provider names may also appear in format tags (e.g. "openai" is both a name and format)
    expect(screen.getAllByText("openai").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("anthropic").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("deepseek")).toBeInTheDocument();
  });

  it("active provider has the '活跃' tag", () => {
    render(<ProviderList {...defaultProps} />);

    expect(screen.getByText("活跃")).toBeInTheDocument();
  });

  it("non-active providers have a '切换' button", () => {
    render(<ProviderList {...defaultProps} />);

    const switchButtons = screen.getAllByText("切换");
    // Two non-active providers should have switch buttons
    expect(switchButtons).toHaveLength(2);
  });

  it("switch button is disabled when switching is true", () => {
    render(<ProviderList {...defaultProps} switching={true} />);

    const switchButtons = screen.getAllByText("切换");
    switchButtons.forEach((btn) => {
      expect(btn.closest("button")).toBeDisabled();
    });
  });

  it("clicking switch button calls onSwitch with the provider name", async () => {
    const user = userEvent.setup();
    const onSwitch = vi.fn().mockResolvedValue(undefined);
    render(<ProviderList {...defaultProps} onSwitch={onSwitch} />);

    const switchButtons = screen.getAllByText("切换");
    await user.click(switchButtons[0]);

    expect(onSwitch).toHaveBeenCalledWith("anthropic");
  });

  it("active provider does not have a switch button", () => {
    render(<ProviderList {...defaultProps} />);

    // The active provider "openai" should not have a switch button in its row
    // We have 3 providers but only 2 switch buttons
    const switchButtons = screen.getAllByText("切换");
    expect(switchButtons).toHaveLength(2);
  });
});
