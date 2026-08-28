import { A } from "@solidjs/router";
import styles from "./NotFound.module.css";

export default function NotFound() {
  return (
    <div class={styles.page}>
      <h1>404</h1>
      <p>This page doesn't exist.</p>
      <A href="/">Back to sessions</A>
    </div>
  );
}
