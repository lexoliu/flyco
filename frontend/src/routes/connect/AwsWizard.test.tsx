import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import AwsWizard from "./AwsWizard";
import type { AwsIamPolicy } from "../../api/client";

const POLICY_PATH = "/v1/providers/aws/iam-policy";

const POLICY: AwsIamPolicy = {
  actions: ["ec2:DescribeInstanceTypes", "ec2:RunInstances", "ec2:TerminateInstances"],
  document: '{"Version":"2012-10-17","Statement":[]}',
};

function policyResponse(): Response {
  return new Response(JSON.stringify(POLICY), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

/** The control plane failing to render the policy, as an RFC 9457 problem. */
function outageResponse(): Response {
  return new Response(
    JSON.stringify({
      type: "https://flyco.dev/problems/internal",
      title: "Internal error",
      status: 500,
      detail: "The policy could not be rendered.",
    }),
    { status: 500, headers: { "content-type": "application/problem+json" } },
  );
}

/**
 * Routes the policy read to whichever answer the test wants and leaves
 * everything else to the shared stand-in from src/test/setup.ts.
 */
function answerPolicyWith(answer: () => Response): void {
  const base = vi.mocked(fetch).getMockImplementation();
  vi.mocked(fetch).mockImplementation((input, init) => {
    const url = new URL(String(input instanceof Request ? input.url : input));
    if (url.pathname === POLICY_PATH) {
      return Promise.resolve(answer());
    }
    if (base === undefined) {
      throw new Error("the shared fetch stand-in from src/test/setup.ts is not installed");
    }
    return base(input, init);
  });
}

function mount() {
  const onLink = vi.fn(() => Promise.resolve());
  const result = render(() => <AwsWizard onLink={onLink} linking={false} error={null} />);
  return { ...result, onLink };
}

function type(field: HTMLElement, value: string): void {
  fireEvent.input(field, { target: { value } });
}

describe("AwsWizard", () => {
  it("shows the policy and enables linking once a key is typed", async () => {
    answerPolicyWith(policyResponse);
    const { findByRole, getByLabelText, getByRole } = mount();

    expect(await findByRole("button", { name: "Copy policy" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Link AWS" })).toBeDisabled();

    type(getByLabelText("Access key ID"), "AKIAIOSFODNN7EXAMPLE");
    type(getByLabelText("Secret access key"), "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
    expect(getByRole("button", { name: "Link AWS" })).toBeEnabled();
  });

  it("says when the policy failed to load, offers a retry, and holds the link until it lands", async () => {
    let answer = outageResponse;
    answerPolicyWith(() => answer());
    const { findByRole, getByLabelText, getByRole, queryByRole } = mount();

    // Step 1 owns the failure: the notice sits where the policy would be,
    // and the way out of it is on the same line.
    const alert = await findByRole("alert");
    expect(alert).toHaveTextContent("The policy could not be rendered.");
    expect(getByRole("button", { name: "Retry" })).toBeInTheDocument();

    // A key typed before the policy exists has nothing to be attached to,
    // so the button waits for the policy, not just for the fields.
    type(getByLabelText("Access key ID"), "AKIAIOSFODNN7EXAMPLE");
    type(getByLabelText("Secret access key"), "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
    expect(getByRole("button", { name: "Link AWS" })).toBeDisabled();

    answer = policyResponse;
    getByRole("button", { name: "Retry" }).click();

    expect(await findByRole("button", { name: "Copy policy" })).toBeInTheDocument();
    await waitFor(() => expect(queryByRole("alert")).not.toBeInTheDocument());
    expect(getByRole("button", { name: "Link AWS" })).toBeEnabled();
  });
});
