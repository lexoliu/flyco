/**
 * Seam for session creation.
 *
 * The control plane exposes `POST /v1/sessions` (see /openapi.json). The
 * generated client (src/api/types.ts) will provide the typed request;
 * until it lands this throws rather than guessing the request shape.
 */
export async function requestNewSession(): Promise<never> {
  throw new Error(
    "Starting a session is not wired yet: requestNewSession() needs the generated client for POST /v1/sessions (see src/api/types.ts).",
  );
}
