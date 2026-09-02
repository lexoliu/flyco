/**
 * Reading a service principal out of whatever `az` printed.
 *
 * The Azure CLI prints two different documents for the same command, and a
 * user has no way of knowing which one they got. `--json-auth` (and the
 * deprecated `--sdk-auth` it replaced) prints `clientId` / `clientSecret` /
 * `tenantId` / `subscriptionId`; without either flag the CLI prints
 * `appId` / `password` / `tenant` and no subscription at all. Both are
 * correct answers to "run this command", so the wizard accepts both and asks
 * for the subscription id separately only when the document genuinely lacks
 * it.
 *
 * A parse failure names the key that is missing. "Invalid JSON" is a fact
 * about the text; "this block has no tenant in it" is something the user can
 * act on.
 */

/** The four fields flyco needs to open an Azure subscription. */
export interface AzureServicePrincipal {
  /** Application (client) id. */
  readonly clientId: string;
  /** Client secret issued for that application. */
  readonly clientSecret: string;
  /** Directory (tenant) the principal belongs to. */
  readonly tenantId: string;
  /**
   * Subscription to provision into.
   *
   * `null` when the pasted document did not carry one, which is the CLI's
   * default output. The wizard then asks for it with the one command that
   * prints it.
   */
  readonly subscriptionId: string | null;
}

/** Either the principal, or the sentence explaining why there is none. */
export type ParsedPrincipal =
  | { readonly ok: true; readonly principal: AzureServicePrincipal }
  | { readonly ok: false; readonly error: string };

/** The keys each field answers to, in the order they are tried. */
const FIELDS = {
  clientId: ["clientId", "appId"],
  clientSecret: ["clientSecret", "password"],
  tenantId: ["tenantId", "tenant"],
} as const;

/** What a missing field is called where the user can see it. */
const FIELD_LABEL: Record<keyof typeof FIELDS, string> = {
  clientId: "client id",
  clientSecret: "client secret",
  tenantId: "tenant id",
};

/** A string property of an object, ignoring anything that is not one. */
function text(document: Record<string, unknown>, key: string): string | null {
  const value = document[key];
  return typeof value === "string" && value.trim() !== "" ? value.trim() : null;
}

/**
 * Parses either shape of the CLI's output.
 *
 * Whitespace and a wrapping shell prompt are the user's, not an error: the
 * text is trimmed before it is parsed, and nothing else about it is
 * repaired.
 */
export function parseAzureServicePrincipal(source: string): ParsedPrincipal {
  const trimmed = source.trim();
  if (trimmed === "") {
    return { ok: false, error: "Paste the JSON block the command printed." };
  }

  let document: unknown;
  try {
    document = JSON.parse(trimmed);
  } catch {
    return {
      ok: false,
      error: "That is not JSON. Copy the whole block the command printed, braces included.",
    };
  }
  if (typeof document !== "object" || document === null || Array.isArray(document)) {
    return { ok: false, error: "That JSON is not an object, so it holds no credentials." };
  }

  const record = document as Record<string, unknown>;
  const found: Partial<Record<keyof typeof FIELDS, string>> = {};
  for (const field of Object.keys(FIELDS) as (keyof typeof FIELDS)[]) {
    const value = FIELDS[field].map((key) => text(record, key)).find((held) => held !== null);
    if (value === undefined || value === null) {
      return {
        ok: false,
        error: `This block has no ${FIELD_LABEL[field]} in it. Re-run the command and paste all of what it printed.`,
      };
    }
    found[field] = value;
  }

  return {
    ok: true,
    principal: {
      clientId: found.clientId ?? "",
      clientSecret: found.clientSecret ?? "",
      tenantId: found.tenantId ?? "",
      subscriptionId: text(record, "subscriptionId") ?? text(record, "subscription"),
    },
  };
}
