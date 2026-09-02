/**
 * Readiness: the one fact every screen is designed around.
 *
 * A session cannot exist until the user is signed in, a harness account is
 * linked, and compute is linked (docs/ux.md §1). That is derived from
 * `GET /v1/harness-accounts` and `GET /v1/providers`, and it is loaded
 * **once** by the app shell and shared through this context — a screen that
 * refetched it would show a different answer from the chip row beside it,
 * and would pay for two more requests to do it.
 *
 * A failed load is not "not ready": it is a page that could not tell, which
 * is why the errors are exposed rather than folded into a boolean. Anything
 * that links an account calls `refresh()` and every reader updates at once.
 */
import {
  type JSX,
  createContext,
  createMemo,
  createResource,
  useContext,
} from "solid-js";
import {
  listHarnessAccounts,
  listProviders,
  type HarnessAccountView,
  type ProviderAccountView,
} from "../api/client";

export interface Readiness {
  /** Linked harness accounts; empty until one is linked. */
  harness: () => HarnessAccountView[];
  /** Linked compute accounts; empty until one is linked. */
  compute: () => ProviderAccountView[];
  /** Whether a session can be created at all. */
  ready: () => boolean;
  /** Whether either list is still being loaded for the first time. */
  loading: () => boolean;
  /** Whatever stopped either list from loading, or `undefined`. */
  error: () => unknown;
  /** Reloads both lists — call after linking or unlinking anything. */
  refresh: () => Promise<void>;
}

const ReadinessContext = createContext<Readiness>();

export function ReadinessProvider(props: { children: JSX.Element }) {
  const [harness, harnessActions] = createResource(listHarnessAccounts);
  const [compute, computeActions] = createResource(listProviders);

  const value: Readiness = {
    harness: () => harness() ?? [],
    compute: () => compute() ?? [],
    ready: createMemo(
      () => (harness() ?? []).length > 0 && (compute() ?? []).length > 0,
    ),
    loading: () => harness.loading || compute.loading,
    error: () => harness.error ?? compute.error,
    refresh: async () => {
      await Promise.all([harnessActions.refetch(), computeActions.refetch()]);
    },
  };

  return (
    <ReadinessContext.Provider value={value}>{props.children}</ReadinessContext.Provider>
  );
}

/**
 * Reads the shared readiness state.
 *
 * Fast fail: a component reached outside the provider would silently render
 * as "nothing is linked", which is the most misleading answer available.
 */
export function useReadiness(): Readiness {
  const value = useContext(ReadinessContext);
  if (value === undefined) {
    throw new Error("useReadiness() was called outside a <ReadinessProvider>.");
  }
  return value;
}
