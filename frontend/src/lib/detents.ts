/**
 * The detent a key moves a slider's thumb to, or `null` for a key the
 * slider does not claim.
 *
 * The browser would step a range input on its own, and every step would
 * land on an integer — but the integers *are* the detents here, and which
 * key means which detent is a decision about the control rather than
 * about numbers: `Home` is the first stop, `End` the last, and both arrow
 * axes move by one stop so that a thumb reached by keyboard behaves like
 * the thumb reached by pointer. Owning it keeps that in one tested place;
 * the component prevents the default so nothing steps twice.
 */
export function detentForKey(key: string, position: number, count: number): number | null {
  const target = ((): number | null => {
    switch (key) {
      case "ArrowLeft":
      case "ArrowDown":
        return position - 1;
      case "ArrowRight":
      case "ArrowUp":
        return position + 1;
      case "Home":
        return 0;
      case "End":
        return count;
      default:
        return null;
    }
  })();
  return target === null ? null : Math.min(Math.max(target, 0), count);
}
