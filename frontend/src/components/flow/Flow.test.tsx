/**
 * The frame's own rules (docs/ux.md §4): one primary, disabled with the
 * reason as its title; `Back` restoring the previous page with its answers;
 * a rejected primary reported above the footer with the primary still there.
 */
import { describe, expect, it, vi } from "vitest";
import { fireEvent, waitFor } from "@solidjs/testing-library";
import { primary, problem, renderFlow, route, type } from "./testSupport";

describe("Flow", () => {
  it("renders exactly one primary, in the footer, on every page", async () => {
    const { container, findByRole, getByRole } = renderFlow([
      "meet",
      "agent",
      "compute",
    ]);

    await findByRole("heading", { level: 1, name: "Meet flyco" });
    expect(primary(container)).toHaveTextContent("Next");
    // Nothing else on the page is a button: the frame is the navigation.
    expect(container.querySelectorAll("button")).toHaveLength(1);

    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "Link the agents you use" });
    expect(primary(container)).toHaveTextContent("Next");
    // Back has arrived; the agent rows are radios, and none of them is a
    // primary.
    expect(getByRole("button", { name: "Back" })).toBeInTheDocument();
    expect(getByRole("radio", { name: /^Claude Code/ })).toBeInTheDocument();
  });

  it("disables the primary with the missing prerequisite as its title", async () => {
    const { container, findByRole, getByRole } = renderFlow(["compute"]);
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });

    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Choose where sessions run to continue",
    );

    fireEvent.click(getByRole("radio", { name: /Azure/ }));
    expect(primary(container)).toBeEnabled();
    expect(primary(container)).not.toHaveAttribute("title");
  });

  it("goes back one page, and the page shows the answer it was given", async () => {
    const { container, findByRole, getByRole } = renderFlow(["compute"]);
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });

    fireEvent.click(getByRole("radio", { name: /Azure/ }));
    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "New to Azure?" });
    expect(primary(container)).toHaveAttribute(
      "title",
      "Answer the question to continue",
    );

    fireEvent.click(getByRole("radio", { name: /^Yes/ }));
    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "Are you a student?" });

    fireEvent.click(getByRole("button", { name: "Back" }));
    await findByRole("heading", { level: 1, name: "New to Azure?" });
    expect(getByRole("radio", { name: /^Yes/ })).toBeChecked();
    expect(primary(container)).toBeEnabled();

    fireEvent.click(getByRole("button", { name: "Back" }));
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });
    expect(getByRole("radio", { name: /Azure/ })).toBeChecked();
  });

  it("offers no Back on the first page of the first run", async () => {
    const { findByRole, queryByRole } = renderFlow([
      "meet",
      "agent",
      "compute",
    ]);
    await findByRole("heading", { level: 1, name: "Meet flyco" });
    expect(queryByRole("button", { name: "Back" })).not.toBeInTheDocument();
  });

  it("hands Back on the first page to the caller that opened the flow", async () => {
    const onLeave = vi.fn();
    const { findByRole, getByRole } = renderFlow(["compute"], { onLeave });
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });

    fireEvent.click(getByRole("button", { name: "Back" }));
    expect(onLeave).toHaveBeenCalledOnce();
  });

  it("finishes when the last page's primary is pressed", async () => {
    const { container, findByRole, onDone } = renderFlow(["meet"]);
    await findByRole("heading", { level: 1, name: "Meet flyco" });

    fireEvent.click(primary(container));
    expect(onDone).toHaveBeenCalledOnce();
  });

  it("reports a refused primary above the footer and keeps the primary", async () => {
    route(
      (path, method) => method === "POST" && path === "/v1/harness-accounts",
      () =>
        problem(422, "invalid-credential", "That key was refused by OpenAI."),
    );
    const { container, findByRole, findByLabelText, getByRole } = renderFlow(
      ["agent"],
      {
        agents: ["codex"],
        answers: { routes: { claude_code: "sign-in", codex: "api-key" } },
        position: 1,
      },
    );
    await findByRole("heading", { level: 1, name: "Paste your API key" });

    type(await findByLabelText("OpenAI API key"), "sk-proj-refused");
    fireEvent.click(primary(container));

    const alert = await findByRole("alert");
    expect(alert).toHaveTextContent("That key was refused by OpenAI.");
    expect(primary(container)).toHaveTextContent("Link Codex");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    expect(
      getByRole("heading", { level: 1, name: "Paste your API key" }),
    ).toBeInTheDocument();
  });
});
