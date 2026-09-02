import { render } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";
import ToolsSection from "./ToolsSection";

vi.mock("../../api/client", async () => {
  const actual = await vi.importActual<typeof import("../../api/client")>("../../api/client");
  return {
    ...actual,
    listMcpServers: vi.fn(async () => {
      throw new Error("the MCP registry is unreachable");
    }),
    listSkills: vi.fn(async () => {
      throw new Error("the skill store is unreachable");
    }),
  };
});

describe("ToolsSection", () => {
  it("keeps both blocks on screen and says why when their lists fail to load", async () => {
    const { findByText, getByRole, getByText } = render(() => <ToolsSection />);

    // A rejected resource must not unmount the block: the reason is shown
    // and the way to add something is still there.
    expect(await findByText("the MCP registry is unreachable")).toBeInTheDocument();
    expect(getByRole("button", { name: "Add server" })).toBeInTheDocument();
    expect(await findByText("the skill store is unreachable")).toBeInTheDocument();
    expect(getByText(/Skills are folders of instructions/)).toBeInTheDocument();
  });
});
