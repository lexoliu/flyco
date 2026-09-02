/**
 * Reading a Google service-account key well enough to confirm it.
 *
 * The whole document is what flyco stores and what the driver signs with, so
 * nothing here rewrites it — the parse exists only so the wizard can show the
 * user *which* account they just dropped in. A user with three projects open
 * has three near-identical key files in their downloads folder, and
 * `project_id` and `client_email` are how they tell them apart.
 *
 * The one thing this does refuse is a file that is not a service-account key
 * at all: an OAuth client secret is the other JSON Google hands out from the
 * same console page, it has none of these fields, and linking it would fail
 * later with an error about a signature rather than here with the reason.
 */

/** What a service-account key says about itself. */
export interface GcpServiceAccount {
  /** The project the key opens. */
  readonly projectId: string;
  /** The service account's own address, which is how it is granted roles. */
  readonly clientEmail: string;
}

/** Either the identity, or the sentence explaining why there is none. */
export type ParsedServiceAccount =
  | { readonly ok: true; readonly account: GcpServiceAccount }
  | { readonly ok: false; readonly error: string };

/** The `type` every service-account key carries. */
const SERVICE_ACCOUNT_TYPE = "service_account";

function text(document: Record<string, unknown>, key: string): string | null {
  const value = document[key];
  return typeof value === "string" && value.trim() !== "" ? value.trim() : null;
}

/** Reads the identity out of a key file's contents. */
export function parseGcpServiceAccount(source: string): ParsedServiceAccount {
  let document: unknown;
  try {
    document = JSON.parse(source);
  } catch {
    return { ok: false, error: "That file is not JSON, so it is not a service-account key." };
  }
  if (typeof document !== "object" || document === null || Array.isArray(document)) {
    return { ok: false, error: "That JSON is not an object, so it holds no key." };
  }

  const record = document as Record<string, unknown>;
  if (text(record, "type") !== SERVICE_ACCOUNT_TYPE) {
    return {
      ok: false,
      error:
        "That is not a service-account key. Download the key from the service account itself, not an OAuth client.",
    };
  }
  if (text(record, "private_key") === null) {
    return {
      ok: false,
      error: "That key file has no private key in it, so nothing can be signed with it.",
    };
  }

  const projectId = text(record, "project_id");
  const clientEmail = text(record, "client_email");
  if (projectId === null || clientEmail === null) {
    return {
      ok: false,
      error: "That key file names no project and account, so flyco cannot say what it opens.",
    };
  }

  return { ok: true, account: { projectId, clientEmail } };
}
