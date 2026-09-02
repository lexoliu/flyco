/**
 * The break-glass SSH key, generated in the browser.
 *
 * Azure refuses to create a Linux machine with neither a password nor a key,
 * and flyco sets no passwords — so a key is required, and flyco's stance is
 * that it never holds the private half of one. Both facts are satisfied by
 * minting the pair here: the private key is offered to the user once, as a
 * download and a copy, and only the public half is ever sent.
 *
 * The formatting is `ed25519-keygen`'s, not ours. An `OpenSSH` private key is
 * a binary container with a length-prefixed grammar and a check-word pair,
 * and a hand-rolled encoder that gets one field wrong produces a file that
 * looks right and that `ssh` refuses at the moment the user most needs it to
 * work.
 */
import { getKeys } from "ed25519-keygen/ssh";

/** Bytes of entropy an Ed25519 seed takes. */
const SEED_BYTES = 32;

/** One generated key pair, in the two formats `OpenSSH` reads. */
export interface BreakGlassKey {
  /** `ssh-ed25519 AAAA… comment`, which is what flyco is sent. */
  readonly publicKey: string;
  /** The `OPENSSH PRIVATE KEY` block, which the user keeps or loses. */
  readonly privateKey: string;
  /** `SHA256:…`, so the user can match the key against a host later. */
  readonly fingerprint: string;
}

/**
 * Mints an Ed25519 pair from the platform's own CSPRNG.
 *
 * Throws where `crypto.getRandomValues` is unavailable rather than falling
 * back to anything weaker: a key generated from a predictable seed is a
 * login anybody can derive, and a wizard that quietly produced one would be
 * worse than one that refused.
 */
export function generateBreakGlassKey(comment: string): BreakGlassKey {
  if (typeof crypto?.getRandomValues !== "function") {
    throw new Error(
      "This browser exposes no cryptographic random source, so flyco cannot generate a key here.",
    );
  }

  const seed = crypto.getRandomValues(new Uint8Array(SEED_BYTES));
  const keys = getKeys(seed, comment);
  return {
    publicKey: keys.publicKey,
    privateKey: keys.privateKey,
    fingerprint: keys.fingerprint,
  };
}

/**
 * Hands the private key to the user as a file.
 *
 * A download rather than only a copy button, because this is the one moment
 * the key exists: nothing on flyco's side can reissue it, and a clipboard is
 * a place things get overwritten.
 */
export function downloadPrivateKey(key: BreakGlassKey, filename: string): void {
  const blob = new Blob([key.privateKey], { type: "application/x-pem-file" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  URL.revokeObjectURL(url);
}
