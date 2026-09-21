import { render, fireEvent, waitFor } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { describe, expect, it, vi } from "vitest";
import McpCatalog from "./McpCatalog";
import type { CatalogMcpServer } from "../../api/client";

const DEEPWIKI: CatalogMcpServer = {
  name: "com.devin/deepwiki",
  title: "DeepWiki",
  description: "Documentation for any public GitHub repository.",
  version: "1.0.0",
  repository_url: null,
  website_url: null,
  suggested_name: "deepwiki",
  installs: [{ kind: "remote", label: "Remote · mcp.deepwiki.com", inputs: [] }],
};

const SMITHERY: CatalogMcpServer = {
  name: "ai.smithery/Hint-Services-obsidian-github-mcp",
  title: null,
  description: "Your Obsidian vault on GitHub.",
  version: "0.4.0",
  repository_url: null,
  website_url: null,
  suggested_name: "Hint-Services-obsidian-github-mcp",
  installs: [
    {
      kind: "remote",
      label: "Remote · server.smithery.ai",
      inputs: [
        {
          key: "var:smithery_api_key",
          label: "smithery_api_key",
          description: "Bearer token for Smithery authentication",
          required: true,
          secret: true,
          default: null,
        },
      ],
    },
  ],
};

const { listCatalogMcpServers, installCatalogMcpServer } = vi.hoisted(() => ({
  listCatalogMcpServers: vi.fn(async () => ({ servers: [DEEPWIKI, SMITHERY], next_cursor: null })),
  installCatalogMcpServer: vi.fn(async () => ({
    id: "s1",
    name: "deepwiki",
    enabled: true,
    updated_at_unix: 0,
    config: { transport: "http" as const, url: "https://mcp.deepwiki.com/mcp", headers: [] },
  })),
}));

vi.mock("../../api/client", async () => {
  const actual = await vi.importActual<typeof import("../../api/client")>("../../api/client");
  return { ...actual, listCatalogMcpServers, installCatalogMcpServer };
});

function mount() {
  const history = createMemoryHistory();
  history.set({ value: "/settings/tools/mcp-catalog", replace: true, scroll: false });
  const rendered = render(() => (
    <MemoryRouter history={history}>
      <Route path="/settings/tools" component={() => <h2>Tools</h2>} />
      <Route path="/settings/tools/mcp-catalog" component={McpCatalog} />
    </MemoryRouter>
  ));
  return { ...rendered, history };
}

describe("McpCatalog", () => {
  it("adds a server that needs nothing the moment its row is chosen, then returns to Tools", async () => {
    installCatalogMcpServer.mockClear();
    const { findByRole, history } = mount();
    const row = await findByRole("button", { name: /DeepWiki/ });
    fireEvent.click(row);
    await waitFor(() => expect(installCatalogMcpServer).toHaveBeenCalledTimes(1));
    expect(installCatalogMcpServer).toHaveBeenCalledWith({
      server: DEEPWIKI.name,
      kind: "remote",
      name: "deepwiki",
      values: {},
    });
    await waitFor(() => expect(history.get()).toBe("/settings/tools"));
  });

  it("asks for the values an entry needs on a page with one primary, then posts them", async () => {
    installCatalogMcpServer.mockClear();
    const { findByRole, findByLabelText, getAllByRole, getByRole } = mount();
    fireEvent.click(await findByRole("button", { name: /Hint-Services-obsidian-github-mcp/ }));

    // The details page: the name, the one input the registry named, and a
    // single primary.
    const key = await findByLabelText("smithery_api_key");
    expect(key).toHaveAttribute("type", "password");
    expect(key).toBeRequired();
    const name = getByRole("textbox", { name: "Name" });
    expect(name).toHaveValue("Hint-Services-obsidian-github-mcp");
    expect(getAllByRole("button", { name: "Add server" })).toHaveLength(1);
    expect(installCatalogMcpServer).not.toHaveBeenCalled();

    fireEvent.input(name, { target: { value: "obsidian" } });
    fireEvent.input(key, { target: { value: "sk-1" } });
    fireEvent.submit(getByRole("button", { name: "Add server" }).closest("form")!);
    await waitFor(() => expect(installCatalogMcpServer).toHaveBeenCalledTimes(1));
    expect(installCatalogMcpServer).toHaveBeenCalledWith({
      server: SMITHERY.name,
      kind: "remote",
      name: "obsidian",
      values: { "var:smithery_api_key": "sk-1" },
    });
  });

  it("takes a one-click add whose name is taken to the details page with the clash", async () => {
    installCatalogMcpServer.mockClear();
    installCatalogMcpServer.mockRejectedValueOnce(
      new Error("you already registered an MCP server called deepwiki"),
    );
    const { findByRole, findByText, getByRole } = mount();
    fireEvent.click(await findByRole("button", { name: /DeepWiki/ }));
    expect(
      await findByText("you already registered an MCP server called deepwiki"),
    ).toBeInTheDocument();
    expect(getByRole("textbox", { name: "Name" })).toHaveValue("deepwiki");
    expect(getByRole("button", { name: "Add server" })).toBeInTheDocument();
  });
});
