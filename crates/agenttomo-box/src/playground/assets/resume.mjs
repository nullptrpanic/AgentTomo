// Restore a token for this tab after reloading a clean workspace URL.
const target = new URL(location.href);
if (!target.searchParams.has("token")) {
  const key = "agenttomo.access." + (target.searchParams.get("monitor") || "owner");
  const token = sessionStorage.getItem(key);
  if (token) {
    target.searchParams.set("token", token);
    location.replace(target);
  }
}
