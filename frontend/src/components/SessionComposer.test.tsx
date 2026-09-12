import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import SessionComposer, {
  paletteEntries,
  paletteQuery,
  type SessionComposerProps,
} from "./SessionComposer";
import type { HarnessCommand } from "../api/wire";

/**
 * Verbatim rows of what a running harness reports: one command that takes
 * an argument, one that takes none, a plugin-qualified skill, the
 * harness's own `compact`, which flyco routes to its own request instead,
 * and the harness's own `context`, which the palette drops outright —
 * the usage ring beside send is the only door to that panel.
 */
const COMMANDS: HarnessCommand[] = [
  {
    name: "goal",
    description: "Set a goal — keep working until the condition is met",
    argument_hint: null,
  },
  {
    name: "effort",
    description: "Set effort level for model usage",
    argument_hint: "<low|medium|high|xhigh|max|ultracode|auto>",
  },
  {
    name: "compact",
    description: "Free up context by summarizing the conversation so far",
    argument_hint: "<optional custom summarization instructions>",
  },
  {
    name: "context",
    description: "Visualize current context usage as a colored grid",
    argument_hint: null,
  },
  {
    name: "presence:status",
    description: "What presence currently thinks the user is doing",
    argument_hint: null,
  },
];

function mount(overrides: Partial<SessionComposerProps> = {}) {
  const props: SessionComposerProps = {
    turnInFlight: false,
    commands: COMMANDS,
    onSend: vi.fn(),
    onStop: vi.fn(),
    onCommand: vi.fn(),
    machineUp: true,
    ...overrides,
  };
  return { props, ...render(() => <SessionComposer {...props} />) };
}

/** Types into the field the way a keystroke does. */
function type(field: HTMLTextAreaElement, value: string): void {
  field.value = value;
  field.dispatchEvent(new Event("input", { bubbles: true }));
}

function press(field: HTMLTextAreaElement, key: string): void {
  field.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
}

describe("SessionComposer", () => {
  it("takes a message when the session is running", () => {
    const { props, getByLabelText, getByRole } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    expect(field.disabled).toBe(false);
    type(field, "Also add a regression test.");
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).toHaveBeenCalledWith("Also add a regression test.");
  });

  it("says a `!` line runs in the machine's shell", () => {
    const { getByLabelText, getByText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "!git status");

    expect(getByText("Runs in the machine's bash")).toBeInTheDocument();
  });

  it("says when a message will be sent while a plan window is being waited out", () => {
    // The composer stays open through a usage-limit pause and the message
    // is held until the window turns over, so the field says so before a
    // word is typed rather than after it is sent (docs/ux.md §9.8).
    const { props, getByLabelText, getByRole, getByText } = mount({
      deferred: "Sent when the window resets, at 7:35 PM",
    });
    expect(getByText("Sent when the window resets, at 7:35 PM")).toBeInTheDocument();

    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;
    expect(field.disabled).toBe(false);
    type(field, "then run the migration");
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).toHaveBeenCalledWith("then run the migration");
  });

  it("keeps saying a `!` line runs now, even while a window is being waited out", () => {
    // A shell command runs on the machine there and then, whatever the
    // plan's limits are doing, so promising it would wait would be a lie.
    const { getByLabelText, getByText, queryByText } = mount({
      deferred: "Sent when the window resets, at 7:35 PM",
    });
    type(getByLabelText("Message the agent") as HTMLTextAreaElement, "!cargo test");

    expect(getByText("Runs in the machine's bash")).toBeInTheDocument();
    expect(queryByText("Sent when the window resets, at 7:35 PM")).toBeNull();
  });

  it("refuses a `!` line while no machine is connected, and says why", () => {
    // A shell command is delivered or it is nothing — the room cannot hold
    // it the way it holds a prompt — so the field refuses it rather than
    // letting the run die on the way.
    const { props, getByLabelText, getByRole, getByText } = mount({ machineUp: false });
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "!git status");

    expect(getByText(/no bash to run it/)).toBeInTheDocument();
    expect(getByRole("button", { name: "Send" })).toBeDisabled();
    press(field, "Enter");
    expect(props.onSend).not.toHaveBeenCalled();
    expect(field.value).toBe("!git status");
  });

  it("still takes a plain prompt while no machine is connected", () => {
    // A message waits in the room's mailbox for the machine's return, so
    // the field keeps taking it.
    const { props, getByLabelText, getByRole } = mount({ machineUp: false });
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "when you are back, run the tests");
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).toHaveBeenCalledWith("when you are back, run the tests");
  });

  it("refuses a typed-out /compact while no machine is connected", () => {
    const { props, getByLabelText, getByText } = mount({ machineUp: false });
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/compact");
    press(field, "Enter");

    expect(props.onCommand).not.toHaveBeenCalled();
    expect(props.onSend).not.toHaveBeenCalled();
    expect(field.value).toBe("/compact");
    expect(getByText(/\/compact needs it/)).toBeInTheDocument();
  });
});

describe("the palette's rows", () => {
  it("puts flyco's own first and drops the harness's copies of what it owns", () => {
    const rows = paletteEntries(COMMANDS);
    expect(rows.slice(0, 3).map((row) => row.name)).toEqual(["compact", "archive", "resize"]);
    expect(rows.filter((row) => row.name === "compact")).toHaveLength(1);
    // `/context` is not a command at all — the usage ring is its door — so
    // neither flyco's nor the harness's row may appear.
    expect(rows.filter((row) => row.name === "context")).toHaveLength(0);
    expect(rows[0]?.run).toBe("compact");
    expect(rows.map((row) => row.name)).toEqual([
      "compact",
      "archive",
      "resize",
      "goal",
      "effort",
      "presence:status",
    ]);
  });

  it("shows only flyco's own until the harness has reported a list", () => {
    expect(paletteEntries([]).map((row) => row.name)).toEqual(["compact", "archive", "resize"]);
  });

  it("greys the machine-bound rows while no machine is connected", () => {
    // `/compact` is delivered to the daemon or it is nothing; `/archive`
    // and `/resize` are control-plane actions that keep working without
    // one, and so does everything the harness listed — a harness command
    // is sent as a message, which the mailbox holds.
    const rows = paletteEntries(COMMANDS, false);
    expect(rows.find((row) => row.name === "compact")?.disabled).toBe(true);
    expect(rows.find((row) => row.name === "archive")?.disabled).toBeUndefined();
    expect(rows.find((row) => row.name === "resize")?.disabled).toBeUndefined();
    expect(rows.find((row) => row.name === "goal")?.disabled).toBeUndefined();
  });

  it("opens on a slash and closes once an argument is being typed", () => {
    expect(paletteQuery("")).toBeNull();
    expect(paletteQuery("hello")).toBeNull();
    expect(paletteQuery("/")).toBe("");
    expect(paletteQuery("/eff")).toBe("eff");
    expect(paletteQuery("/goal keep going")).toBeNull();
  });
});

describe("the palette", () => {
  it("filters by prefix on the name and shows the description and hint", () => {
    const { getByLabelText, getAllByRole, getByText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/");
    expect(getAllByRole("option")).toHaveLength(6);

    type(field, "/eff");
    const rows = getAllByRole("option");
    expect(rows).toHaveLength(1);
    expect(rows[0]?.textContent).toContain("/effort");
    expect(rows[0]?.textContent).toContain("<low|medium|high|xhigh|max|ultracode|auto>");
    expect(getByText("Set effort level for model usage")).toBeInTheDocument();
  });

  it("sends a command that takes no argument the moment it is chosen", () => {
    const { props, getByLabelText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/goal");
    press(field, "Enter");

    expect(props.onSend).toHaveBeenCalledWith("/goal");
    expect(field.value).toBe("");
  });

  it("writes a command that wants an argument into the field instead", () => {
    const { props, getByLabelText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/eff");
    press(field, "Enter");

    expect(props.onSend).not.toHaveBeenCalled();
    expect(field.value).toBe("/effort ");
  });

  it("runs flyco's own commands itself rather than sending them", () => {
    const { props, getByLabelText } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/archive");
    press(field, "Enter");

    expect(props.onCommand).toHaveBeenCalledWith("archive");
    expect(props.onSend).not.toHaveBeenCalled();
  });

  it("moves through the rows with the arrow keys", () => {
    const { props, getByLabelText, getAllByRole } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/");
    press(field, "ArrowDown");
    press(field, "ArrowDown");
    press(field, "ArrowDown");
    expect(getAllByRole("option")[3]?.getAttribute("aria-selected")).toBe("true");

    press(field, "ArrowUp");
    expect(getAllByRole("option")[2]?.getAttribute("aria-selected")).toBe("true");

    press(field, "Enter");
    expect(props.onCommand).toHaveBeenCalledWith("resize");
  });

  it("closes on Escape without touching what was typed", () => {
    const { getByLabelText, queryAllByRole } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/goa");
    expect(queryAllByRole("option")).toHaveLength(1);

    press(field, "Escape");
    expect(queryAllByRole("option")).toHaveLength(0);
    expect(field.value).toBe("/goa");
  });

  it("sends a command and its argument as one ordinary message", () => {
    const { props, getByLabelText, getByRole, queryAllByRole } = mount();
    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;

    type(field, "/goal keep going until the tests pass");
    // The command is chosen by now, so the list is out of the way.
    expect(queryAllByRole("option")).toHaveLength(0);
    getByRole("button", { name: "Send" }).click();

    expect(props.onSend).toHaveBeenCalledWith("/goal keep going until the tests pass");
  });
});
