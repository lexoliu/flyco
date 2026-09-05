/**
 * Stage C's pages against the in-memory control plane (docs/ux.md §4 C).
 *
 * Every provider's branch is walked once, page by page, and the linked
 * card at the end is what settings will show. The credit page is asserted
 * both present and absent, since its existence is the quickstart's answer.
 */
import { describe, expect, it, vi } from "vitest";
import { fireEvent, waitFor } from "@solidjs/testing-library";
import type {
  AwsIamPolicy,
  HostView,
  ProviderAccountView,
  ProviderBonusHint,
} from "../../../api/client";
import { HOST_ENROLL_COMMAND } from "../../../test/setup";
import {
  command,
  json,
  postedTo,
  primary,
  problem,
  renderFlow,
  route,
  type,
} from "../testSupport";

const AZURE_STUDENTS: ProviderBonusHint = {
  provider: "azure",
  title: "Azure for Students",
  detail: "Verify with a school email address.",
  url: "https://azure.microsoft.com/free/students/",
  credit: 100_000_000,
};

const POLICY: AwsIamPolicy = {
  actions: [
    "ec2:DescribeInstanceTypes",
    "ec2:RunInstances",
    "ec2:TerminateInstances",
  ],
  document: '{"Version":"2012-10-17","Statement":[]}',
};

const HOST: HostView = {
  id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
  label: "mercury",
  state: "online",
  facts: {
    architecture: "arm64",
    vcpus: 12,
    memory_mib: 32 * 1024,
    disk_free_gib: 401,
    podman_version: "5.4.0",
    kernel: "6.8.0-45-generic",
    hostname: "mercury",
  },
  last_seen_unix: 1_790_000_600,
  created_at_unix: 1_790_000_000,
};

function linkedAccount(
  kind: ProviderAccountView["kind"],
  label: string,
): ProviderAccountView {
  return {
    id: "5f2b9a10-7c34-4d1e-9a6b-2f8c1d0e4b73",
    kind,
    label,
    linked_at_unix: 1_790_000_000,
  };
}

/** Answers the quickstart with `hints`, and the link with `account`. */
function cloud(hints: ProviderBonusHint[], account: ProviderAccountView): void {
  route(
    (path, method) => method === "POST" && path === "/v1/providers/quickstart",
    () => json(hints),
  );
  route(
    (path, method) => method === "POST" && path === "/v1/providers",
    () => json(account, 201),
  );
}

/** Opens stage C alone and chooses one place for sessions to run. */
async function choosePlace(name: RegExp) {
  const flow = renderFlow(["compute"]);
  await flow.findByRole("heading", {
    level: 1,
    name: "Where should sessions run?",
  });
  fireEvent.click(flow.getByRole("radio", { name }));
  fireEvent.click(primary(flow.container));
  return flow;
}

/** Answers the two bonus questions, landing on whatever follows them. */
async function answerBonus(
  flow: Awaited<ReturnType<typeof choosePlace>>,
  provider: string,
  answers: { newcomer: boolean; student: boolean },
): Promise<void> {
  await flow.findByRole("heading", { level: 1, name: `New to ${provider}?` });
  fireEvent.click(
    flow.getByRole("radio", { name: answers.newcomer ? /^Yes/ : /^No/ }),
  );
  fireEvent.click(primary(flow.container));
  await flow.findByRole("heading", { level: 1, name: "Are you a student?" });
  fireEvent.click(
    flow.getByRole("radio", { name: answers.student ? /^Yes/ : /^No/ }),
  );
  fireEvent.click(primary(flow.container));
}

/** Leaves the consent road for the vendor's terminal, by its quiet link. */
async function useCloudShell(
  flow: Awaited<ReturnType<typeof choosePlace>>,
): Promise<void> {
  await flow.findByRole("heading", { level: 1, name: /^Sign in with/ });
  fireEvent.click(
    flow.getByRole("button", { name: "Use Cloud Shell instead" }),
  );
}

describe("the choice", () => {
  it("offers the four places docs/ux.md §7 names, as radios", async () => {
    const { findByRole, getByRole } = renderFlow(["compute"]);
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });
    for (const title of ["Azure", "AWS", "Google Cloud", "Your own machine"]) {
      expect(
        getByRole("radio", { name: new RegExp(title) }),
      ).toBeInTheDocument();
    }
  });
});

describe("Azure", () => {
  it("walks credit, command, paste, subscription and key to the linked card", async () => {
    cloud([AZURE_STUDENTS], linkedAccount("azure", "Azure"));
    const flow = await choosePlace(/Azure/);
    const {
      container,
      findByRole,
      findByText,
      getByRole,
      getByText,
      getByLabelText,
      onDone,
    } = flow;

    await answerBonus(flow, "Azure", { newcomer: true, student: true });
    expect(postedTo("/v1/providers/quickstart")).toEqual({
      new_to_provider: true,
      is_student: true,
    });

    // A programme matched, so it has a page: the offer, and a quiet link.
    await findByRole("heading", { level: 1, name: "Azure gives you credit" });
    expect(getByText("Azure for Students")).toBeInTheDocument();
    expect(getByText("$100.00")).toBeInTheDocument();
    expect(getByRole("link", { name: /Sign up/ })).toHaveAttribute(
      "href",
      AZURE_STUDENTS.url,
    );
    expect(primary(container)).toHaveTextContent("Next");
    fireEvent.click(primary(container));
    await useCloudShell(flow);

    await findByRole("heading", {
      level: 1,
      name: "Run this in Azure Cloud Shell",
    });
    expect(
      getByRole("link", { name: /Open Azure Cloud Shell/ }),
    ).toHaveAttribute("href", "https://shell.azure.com");
    expect(getByText(command(/az ad sp create-for-rbac/))).toBeInTheDocument();
    expect(getByRole("button", { name: "Copy" })).toBeInTheDocument();
    fireEvent.click(primary(container));

    await findByRole("heading", { level: 1, name: "Paste the JSON block" });
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Paste the JSON block to continue",
    );
    const paste = getByLabelText("The JSON block the command printed");
    type(paste, '{"clientId": "app-1", "clientSecret": "s3cret"}');
    expect(await findByText(/no tenant id in it/)).toBeInTheDocument();
    expect(primary(container)).toBeDisabled();
    // The CLI's default output: no subscription in it.
    type(
      paste,
      '{"appId": "app-1", "password": "s3cret", "tenant": "tenant-1"}',
    );
    expect(await findByText("app-1")).toBeInTheDocument();
    expect(getByText("tenant-1")).toBeInTheDocument();
    expect(getByText("held, and never shown again")).toBeInTheDocument();
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    // So the one page that asks for it appears.
    await findByRole("heading", { level: 1, name: "Which subscription?" });
    expect(
      getByText(command("az account show --query id -o tsv")),
    ).toBeInTheDocument();
    expect(primary(container)).toBeDisabled();
    type(getByLabelText("Subscription id"), "sub-1");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    // The subscription page is the last one: it links.
    expect(primary(container)).toHaveTextContent("Link Azure");
    fireEvent.click(primary(container));

    // No key page and no "is linked" page: the link finishes the flow, and
    // the machine's login key is flyco's, minted on the control plane.
    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    const posted = postedTo("/v1/providers") as {
      label: string;
      credentials: Record<string, string>;
    };
    expect(posted.label).toBe("Azure");
    expect(posted.credentials).toEqual({
      kind: "azure",
      client_id: "app-1",
      client_secret: "s3cret",
      tenant_id: "tenant-1",
      subscription_id: "sub-1",
    });
  });

  it("links straight from the paste when the block names its subscription", async () => {
    cloud([], linkedAccount("azure", "Azure"));
    const flow = await choosePlace(/Azure/);
    const { onDone, container, findByRole, getByLabelText } = flow;
    await answerBonus(flow, "Azure", { newcomer: false, student: false });
    await useCloudShell(flow);

    // Nothing matched: no credit page, straight to the command.
    await findByRole("heading", {
      level: 1,
      name: "Run this in Azure Cloud Shell",
    });
    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "Paste the JSON block" });
    type(
      getByLabelText("The JSON block the command printed"),
      '{"clientId": "app-1", "clientSecret": "s3cret", "tenantId": "tenant-1", "subscriptionId": "sub-1"}',
    );
    await waitFor(() => expect(primary(container)).toBeEnabled());
    // A complete block: no subscription page, the paste page links.
    expect(primary(container)).toHaveTextContent("Link Azure");
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(
      (postedTo("/v1/providers") as { credentials: Record<string, string> })
        .credentials,
    ).toEqual({
      kind: "azure",
      client_id: "app-1",
      client_secret: "s3cret",
      tenant_id: "tenant-1",
      subscription_id: "sub-1",
    });
  });

  it("keeps the primary and reports the refusal when the link is refused", async () => {
    cloud([], linkedAccount("azure", "Azure"));
    route(
      (path, method) => method === "POST" && path === "/v1/providers",
      () =>
        problem(
          422,
          "invalid-credential",
          "Azure refused the service principal.",
        ),
    );
    const flow = await choosePlace(/Azure/);
    const { container, findByRole, getByLabelText, getByRole } = flow;
    await answerBonus(flow, "Azure", { newcomer: false, student: false });
    await useCloudShell(flow);
    await findByRole("heading", {
      level: 1,
      name: "Run this in Azure Cloud Shell",
    });
    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "Paste the JSON block" });
    type(
      getByLabelText("The JSON block the command printed"),
      '{"clientId": "app-1", "clientSecret": "s3cret", "tenantId": "tenant-1", "subscriptionId": "sub-1"}',
    );
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    expect(await findByRole("alert")).toHaveTextContent(
      "Azure refused the service principal.",
    );
    expect(primary(container)).toHaveTextContent("Link Azure");
    expect(
      getByRole("heading", { level: 1, name: "Paste the JSON block" }),
    ).toBeInTheDocument();
  });
});

describe("AWS", () => {
  it("shows the policy, then takes the key, then links", async () => {
    cloud([], linkedAccount("aws", "AWS"));
    route(
      (path, method) =>
        method === "GET" && path === "/v1/providers/aws/iam-policy",
      () => json(POLICY),
    );
    const flow = await choosePlace(/AWS/);
    const {
      onDone,
      container,
      findByRole,
      getByRole,
      getByLabelText,
      getByText,
    } = flow;
    await answerBonus(flow, "AWS", { newcomer: true, student: false });

    await findByRole("heading", { level: 1, name: "Create an access key" });
    expect(
      await flow.findByText("3 actions, and nothing else"),
    ).toBeInTheDocument();
    expect(getByRole("button", { name: "Copy" })).toBeInTheDocument();
    expect(
      getByRole("link", { name: /Open the IAM console/ }),
    ).toBeInTheDocument();
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await findByRole("heading", { level: 1, name: "Enter the access key" });
    expect(primary(container)).toHaveTextContent("Link AWS");
    expect(primary(container)).toHaveAttribute(
      "title",
      "Enter the access key ID to continue",
    );
    type(getByLabelText("Access key ID"), "AKIAIOSFODNN7EXAMPLE");
    await waitFor(() =>
      expect(primary(container)).toHaveAttribute(
        "title",
        "Enter the secret access key to continue",
      ),
    );
    type(
      getByLabelText("Secret access key"),
      "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    );
    await waitFor(() => expect(primary(container)).toBeEnabled());

    // The session token is a field revealed in place, not a page.
    fireEvent.click(getByRole("button", { name: "I have a session token" }));
    type(getByLabelText("Session token"), "FwoGZXIvYXdzE");
    expect(getByText(/sts:AssumeRole/)).toBeInTheDocument();
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/providers")).toEqual({
      label: "AWS",
      credentials: {
        kind: "aws",
        access_key_id: "AKIAIOSFODNN7EXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        session_token: "FwoGZXIvYXdzE",
      },
    });
  });

  it("says when the policy failed to load, offers a retry, and holds Next until it lands", async () => {
    cloud([], linkedAccount("aws", "AWS"));
    let answer = () =>
      problem(500, "internal", "The policy could not be rendered.");
    route(
      (path, method) =>
        method === "GET" && path === "/v1/providers/aws/iam-policy",
      () => answer(),
    );
    const flow = await choosePlace(/AWS/);
    const { container, findByRole, findByText, getByRole, queryByRole } = flow;
    await answerBonus(flow, "AWS", { newcomer: false, student: false });
    await findByRole("heading", { level: 1, name: "Create an access key" });

    // The page owns the failure: the notice sits where the policy would
    // be, the way out is on the same line, and Next waits.
    expect(await findByRole("alert")).toHaveTextContent(
      "The policy could not be rendered.",
    );
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "The policy could not be read; retry it to continue",
    );

    answer = () => json(POLICY);
    fireEvent.click(getByRole("button", { name: "Retry" }));
    expect(await findByText("3 actions, and nothing else")).toBeInTheDocument();
    await waitFor(() => expect(queryByRole("alert")).not.toBeInTheDocument());
    expect(primary(container)).toBeEnabled();
  });
});

describe("Google Cloud", () => {
  it("shows the commands, reads the dropped key, and links", async () => {
    cloud([], linkedAccount("gcp", "my-project"));
    const flow = await choosePlace(/Google Cloud/);
    const {
      onDone,
      container,
      findByRole,
      findByText,
      getByRole,
      getByLabelText,
      getByText,
    } = flow;
    await answerBonus(flow, "Google Cloud", { newcomer: false, student: true });
    await useCloudShell(flow);

    await findByRole("heading", {
      level: 1,
      name: "Run this in Google Cloud Shell",
    });
    expect(
      getByText(command(/gcloud iam service-accounts create flyco/)),
    ).toBeInTheDocument();
    // The key is downloaded by the command, for a user with only a browser.
    expect(
      getByText(command(/cloudshell download flyco-key\.json/)),
    ).toBeInTheDocument();
    expect(
      getByRole("link", { name: /Open Google Cloud Shell/ }),
    ).toHaveAttribute("href", "https://shell.cloud.google.com/?show=terminal");
    expect(getByRole("button", { name: "Copy" })).toBeInTheDocument();
    fireEvent.click(primary(container));

    await findByRole("heading", { level: 1, name: "Drop the key file" });
    expect(primary(container)).toHaveTextContent("Link Google Cloud");
    expect(primary(container)).toHaveAttribute(
      "title",
      "Drop the key file to continue",
    );

    const key = JSON.stringify({
      type: "service_account",
      project_id: "my-project",
      private_key:
        "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
      client_email: "flyco@my-project.iam.gserviceaccount.com",
    });
    const input = getByLabelText(
      "Service account key file",
    ) as HTMLInputElement;
    const file = new File([key], "flyco-key.json", {
      type: "application/json",
    });
    // jsdom's File has no `text()`; the page reads the file the way a
    // browser lets it, so the test supplies what jsdom does not.
    Object.defineProperty(file, "text", { value: () => Promise.resolve(key) });
    Object.defineProperty(input, "files", { value: [file] });
    fireEvent.change(input);

    expect(await findByText("my-project")).toBeInTheDocument();
    expect(
      getByText("flyco@my-project.iam.gserviceaccount.com"),
    ).toBeInTheDocument();
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/providers")).toEqual({
      label: "my-project",
      credentials: { kind: "gcp", service_account_json: key },
    });
  });
});

describe("Your own machine", () => {
  it("mints the command as the page opens and waits, with no bonus questions", async () => {
    const {
      container,
      findByRole,
      findByText,
      getByRole,
      getByText,
      queryByText,
    } = await choosePlace(/Your own machine/);

    await findByRole("heading", { level: 1, name: "Run this on the machine" });
    expect(await findByText(command(HOST_ENROLL_COMMAND))).toBeInTheDocument();
    expect(queryByText("Are you a student?")).toBeNull();
    expect(getByRole("button", { name: "Copy" })).toBeInTheDocument();
    expect(getByText(/needs Podman/)).toBeInTheDocument();
    expect(getByText(/expires in/)).toBeInTheDocument();
    expect(getByRole("status")).toHaveTextContent("Waiting for the machine…");
    expect(primary(container)).toHaveTextContent("Next");
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Run the command on the machine to continue",
    );
  });

  it("finishes the flow the moment the machine arrives", async () => {
    route(
      (path, method) =>
        method === "GET" &&
        /^\/v1\/hosts\/enrollment-tokens\/[^/]+$/.test(path),
      () => json({ status: "enrolled", host: HOST }),
    );
    const { onDone } = await choosePlace(/Your own machine/);

    // The first poll is a few seconds out: nobody installs a daemon faster
    // than that, and a tighter loop would be a request per keystroke.
    await waitFor(() => expect(onDone).toHaveBeenCalledOnce(), {
      timeout: 6000,
    });
  });

  it("turns an expired command into Mint a new command", async () => {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/hosts/enrollment-tokens",
      () =>
        json(
          {
            id: "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14",
            token: "fh_2Qv8xLmR4pT7nWzKcYbA",
            expires_at_unix: Math.floor(Date.now() / 1000) - 1,
            command: HOST_ENROLL_COMMAND,
          },
          201,
        ),
    );
    const { container, findByRole, findByText } =
      await choosePlace(/Your own machine/);
    await findByRole("heading", { level: 1, name: "Run this on the machine" });

    expect(
      await findByText(/The command expired/, {}, { timeout: 3000 }),
    ).toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Mint a new command");
    expect(primary(container)).toBeEnabled();
  });
});

describe("the consent road", () => {
  const CONSENT_AZURE = {
    state: "authorized",
    account: "me@lexo.cool",
    choices: [
      { id: "0b1c2d3e-4f5a-4b6c-8d7e-9f0a1b2c3d4e", name: "Pay-As-You-Go" },
      { id: "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f", name: "Visual Studio" },
    ],
  };

  /** The quickstart matched nothing, so no credit page comes between the questions and the sign-in. */
  function noProgrammes(): void {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/providers/quickstart",
      () => json([]),
    );
  }

  /** Opens the vendor's page and answers the polls: pending once, then the consent. */
  function consentComesBack(cloud: "azure" | "gcp", consent: unknown) {
    noProgrammes();
    let polls = 0;
    route(
      (path, method) =>
        (method === "POST" && path === `/v1/providers/${cloud}/oauth/start`) ||
        (method === "GET" && path === `/v1/providers/${cloud}/oauth/attempt-1`),
      (init) =>
        init?.method === "POST"
          ? json({
              attempt_id: "attempt-1",
              authorize_url: `https://consent.${cloud}.invalid/authorize`,
            })
          : json(++polls === 1 ? { state: "pending" } : consent),
    );
  }

  it("signs in with Microsoft in a new tab, waits, asks which subscription, then the key, and links", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    consentComesBack("azure", CONSENT_AZURE);
    route(
      (path, method) =>
        method === "POST" &&
        path === "/v1/providers/azure/oauth/attempt-1/finish",
      () => json(linkedAccount("azure", "Pay-As-You-Go"), 201),
    );
    const flow = await choosePlace(/Azure/);
    const { container, findByRole, findByText, getByRole, onDone } = flow;
    await answerBonus(flow, "Azure", { newcomer: false, student: false });

    await findByRole("heading", { level: 1, name: "Sign in with Microsoft" });
    expect(primary(container)).toHaveTextContent("Sign in with Microsoft");
    fireEvent.click(primary(container));
    expect(
      await findByText(/Waiting for you to finish in the other tab/),
    ).toBeInTheDocument();
    expect(opened).toHaveBeenCalledWith(
      "https://consent.azure.invalid/authorize",
      "_blank",
      "noopener,noreferrer",
    );
    expect(primary(container)).toBeDisabled();

    // The consent came back on the second poll: the choice page, by itself.
    await findByRole(
      "heading",
      { level: 1, name: "Which subscription?" },
      { timeout: 6000 },
    );
    expect(await findByText(/Signed in as/)).toHaveTextContent("me@lexo.cool");
    expect(primary(container)).toBeDisabled();
    fireEvent.click(getByRole("radio", { name: /Visual Studio/ }));
    await waitFor(() => expect(primary(container)).toBeEnabled());
    // The choice page links: no key page, the machine key is flyco's.
    expect(primary(container)).toHaveTextContent("Link Azure");
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/providers/azure/oauth/attempt-1/finish")).toEqual({
      subscription_id: "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f",
    });
  });

  it("signs in with Google, asks which project, and links from that page", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    consentComesBack("gcp", {
      state: "authorized",
      account: "me@lexo.cool",
      choices: [{ id: "flyco-dev-1234", name: "flyco dev" }],
    });
    route(
      (path, method) =>
        method === "POST" &&
        path === "/v1/providers/gcp/oauth/attempt-1/finish",
      () => json(linkedAccount("gcp", "flyco-dev-1234"), 201),
    );
    const flow = await choosePlace(/Google Cloud/);
    const { container, findByRole, getByRole, onDone } = flow;
    await answerBonus(flow, "Google Cloud", {
      newcomer: false,
      student: false,
    });

    await findByRole("heading", { level: 1, name: "Sign in with Google" });
    fireEvent.click(primary(container));
    await findByRole(
      "heading",
      { level: 1, name: "Which project?" },
      { timeout: 6000 },
    );
    fireEvent.click(getByRole("radio", { name: /flyco dev/ }));
    await waitFor(() =>
      expect(primary(container)).toHaveTextContent("Link Google Cloud"),
    );
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/providers/gcp/oauth/attempt-1/finish")).toEqual({
      project_id: "flyco-dev-1234",
    });
  });

  it("turns a consent the control plane lost into Try again, on the same page", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    noProgrammes();
    route(
      (path, method) =>
        (method === "POST" && path === "/v1/providers/azure/oauth/start") ||
        (method === "GET" && path === "/v1/providers/azure/oauth/attempt-1"),
      (init) =>
        init?.method === "POST"
          ? json({
              attempt_id: "attempt-1",
              authorize_url: "https://consent.azure.invalid/a",
            })
          : problem(
              404,
              "provider-oauth-attempt-expired",
              "That sign-in has expired.",
            ),
    );
    const flow = await choosePlace(/Azure/);
    const { container, findByRole } = flow;
    await answerBonus(flow, "Azure", { newcomer: false, student: false });
    await findByRole("heading", { level: 1, name: "Sign in with Microsoft" });
    fireEvent.click(primary(container));

    expect(await findByRole("alert", {}, { timeout: 6000 })).toHaveTextContent(
      "That sign-in has expired.",
    );
    expect(primary(container)).toHaveTextContent("Try again");
    expect(primary(container)).toBeEnabled();
  });

  it("reports a refused consent on the same page and points at Cloud Shell", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    consentComesBack("azure", {
      state: "failed",
      problem: "microsoft-rejected",
      reason: "access_denied: AADSTS65004: User declined to consent.",
    });
    const flow = await choosePlace(/Azure/);
    const { container, findByRole, getByText } = flow;
    await answerBonus(flow, "Azure", { newcomer: false, student: false });
    await findByRole("heading", { level: 1, name: "Sign in with Microsoft" });
    fireEvent.click(primary(container));
    expect(await findByRole("alert", {}, { timeout: 6000 })).toHaveTextContent(
      "Microsoft did not grant the sign-in: access_denied: AADSTS65004",
    );
    expect(getByText(/Cloud Shell needs no approval/)).toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Try again");
    expect(primary(container)).toBeEnabled();
  });

  it("keeps the vendor's terminal behind a quiet link", async () => {
    noProgrammes();
    const flow = await choosePlace(/Google Cloud/);
    const { findByRole, getByRole } = flow;
    await answerBonus(flow, "Google Cloud", {
      newcomer: false,
      student: false,
    });
    await findByRole("heading", { level: 1, name: "Sign in with Google" });
    fireEvent.click(getByRole("button", { name: "Use Cloud Shell instead" }));
    await findByRole("heading", {
      level: 1,
      name: "Run this in Google Cloud Shell",
    });
  });
});
