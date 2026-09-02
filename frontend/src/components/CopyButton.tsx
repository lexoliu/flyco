/**
 * Copies one string, and says so for two seconds.
 *
 * Used wherever the app shows something the user has to take away with them
 * — an API key that will never be shown again above all. The confirmation
 * is the label changing, not a toast: the thing that changed is the thing
 * the user is already looking at.
 */
import { Show, createSignal, onCleanup } from "solid-js";
import { Check, Copy } from "lucide-solid";

export interface CopyButtonProps {
  /** The text placed on the clipboard. */
  value: string;
  /** What is being copied, so the button names itself. Defaults to "Copy". */
  label?: string | undefined;
  /** Class applied to the button, so a caller picks the pill it needs. */
  class: string | undefined;
}

export default function CopyButton(props: CopyButtonProps) {
  const [copied, setCopied] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));

  async function copy(): Promise<void> {
    await navigator.clipboard.writeText(props.value);
    setCopied(true);
    clearTimeout(timer);
    timer = setTimeout(() => setCopied(false), 2000);
  }

  return (
    <button type="button" class={props.class} onClick={() => void copy()}>
      <Show when={copied()} fallback={<Copy size={13} aria-hidden="true" />}>
        <Check size={13} aria-hidden="true" />
      </Show>
      {copied() ? "Copied" : (props.label ?? "Copy")}
    </button>
  );
}
