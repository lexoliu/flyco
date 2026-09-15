// The Pages project fronts the PWA; the `flyco` Worker keeps the API.
// `_routes.json` confines this Function to the API paths — `/v1/*` and
// `/install/*` — so every invocation here is a request the control plane
// owns, forwarded untouched: the method, headers, and body as they
// arrived, the response — including SSE streams — as the Worker sent it.
//
// `redirect: "manual"` matters: the OAuth callback answers with a 302 to
// `/`, and a following fetch would resolve that against the Worker's own
// origin instead of handing the redirect back to the browser.
const CONTROL_PLANE = "https://flyco.lexo-liu.workers.dev";

export const onRequest: PagesFunction = async (context) => {
  const url = new URL(context.request.url);
  const upstream = new URL(url.pathname + url.search, CONTROL_PLANE);
  return fetch(new Request(upstream, context.request), {
    redirect: "manual",
  });
};
