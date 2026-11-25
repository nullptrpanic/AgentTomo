// Restore a token for this tab after reloading a clean workspace URL.
const target = new URL(location.href);
const key = "agenttomo.access." + (target.searchParams.get("monitor") || "owner");
try {
  if (target.searchParams.has("token")) {
    // This page already returned 401 with a token; clear the stale copy.
    sessionStorage.removeItem(key);
    target.searchParams.delete("token");
    history.replaceState(null, "", target);
  } else {
    const token = sessionStorage.getItem(key);
    if (token) {
      target.searchParams.set("token", token);
      location.replace(target);
    }
  }
} catch { /* Storage-disabled browsers stay on the prompt without looping. */ }
