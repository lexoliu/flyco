import {
  createResource,
  type Resource,
  type ResourceActions,
  type ResourceFetcher,
  type ResourceOptions,
  type ResourceSource,
} from "solid-js";

/** The lifecycle a query is in, straight from the resource underneath. */
export type QueryState = Resource<unknown>["state"];

/**
 * A resource accessor that never throws.
 *
 * Solid's own accessor rethrows the fetcher's rejection at every read, so a
 * single `<Show when={res()}>` beside a `<ProblemNotice error={res.error} />`
 * tears the whole component down before the notice can render — the reader
 * sees a blank screen instead of the failure. This accessor answers
 * `undefined` while the query is in the error state, so the empty state and
 * the notice explaining it can sit side by side.
 */
export interface Query<T> {
  (): T | undefined;
  readonly state: QueryState;
  readonly loading: boolean;
  readonly error: unknown;
  readonly latest: T | undefined;
}

export type QueryReturn<T, R = unknown> = [Query<T>, ResourceActions<T | undefined, R>];

/** Wraps a resource's throwing accessor in one that reports the error instead. */
function guard<T>(resource: Resource<T>): Query<T> {
  const read = (): T | undefined => (resource.error === undefined ? resource() : undefined);
  return Object.defineProperties(read, {
    state: { get: () => resource.state },
    loading: { get: () => resource.loading },
    error: { get: () => resource.error },
    latest: { get: () => (resource.error === undefined ? resource.latest : undefined) },
  }) as Query<T>;
}

/**
 * `createResource` with an accessor that yields `undefined` on failure rather
 * than throwing, so every screen can render its error surface. Use this
 * everywhere; `createResource` itself is only imported here (enforced by
 * `query.boundary.test.ts`).
 */
export function createQuery<T, R = unknown>(
  fetcher: ResourceFetcher<true, T, R>,
  options?: ResourceOptions<NoInfer<T>, true>,
): QueryReturn<T, R>;
export function createQuery<T, S, R = unknown>(
  source: ResourceSource<S>,
  fetcher: ResourceFetcher<S, T, R>,
  options?: ResourceOptions<NoInfer<T>, S>,
): QueryReturn<T, R>;
export function createQuery<T, S, R = unknown>(
  first: ResourceFetcher<true, T, R> | ResourceSource<S>,
  second?: ResourceFetcher<S, T, R> | ResourceOptions<T, true>,
  third?: ResourceOptions<T, S>,
): QueryReturn<T, R> {
  const [resource, actions] =
    typeof second === "function"
      ? createResource<T, S, R>(first as ResourceSource<S>, second, third)
      : createResource<T, R>(first as ResourceFetcher<true, T, R>, second);
  return [guard(resource), actions];
}
