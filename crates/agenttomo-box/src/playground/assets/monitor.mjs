// Every monitor owns an independent cursor, never the container's WebSocket.
export function followMonitor({ id, token, onMessage }) {
  const controller = new AbortController();
  let offset = 0, initialized = false, rootPid = null, timer;
  const stop = () => { clearTimeout(timer); controller.abort(); };
  window.addEventListener("pagehide", stop, { once: true });

  async function poll() {
    let delay = 250;
    try {
      const url = new URL(`/api/sessions/${id}/events`, location.href);
      url.searchParams.set("token", token);
      url.searchParams.set("offset", offset);
      const response = await fetch(url, { cache: "no-store", signal: controller.signal });
      if (response.status === 404) {
        onMessage({ type: "monitor_expired" });
        stop();
        return;
      }
      if (!response.ok) {
        const error = new Error(`Cannot read this session's recording (HTTP ${response.status}).`);
        error.fatal = response.status >= 400 && response.status < 500;
        throw error;
      }
      const batch = await response.json();
      if (!Array.isArray(batch.events) || !Number.isSafeInteger(batch.next_offset) || batch.next_offset < offset || batch.has_more && batch.next_offset === offset) {
        const error = new Error("Invalid recording cursor. Refresh to read this session again.");
        error.fatal = true;
        throw error;
      }
      if (!initialized) {
        onMessage({ ...batch.session, type: "session", id });
        initialized = true;
      }
      if (Number.isSafeInteger(batch.root_pid) && batch.root_pid !== rootPid) {
        rootPid = batch.root_pid;
        onMessage({ type: "ready", pid: rootPid });
      }
      for (const event of batch.events) onMessage({ type: "trace", event });
      offset = batch.next_offset;
      if (!batch.active && !batch.has_more) {
        onMessage({ type: "monitor_end", result: batch.result, incomplete: batch.incomplete });
        stop();
        return;
      }
      onMessage({ type: "monitor_status", label: batch.has_more ? "Loading history" : rootPid === null ? "Starting container" : "Following session" });
      if (batch.has_more) delay = 0;
    } catch (error) {
      if (controller.signal.aborted) return;
      if (error.fatal || error instanceof SyntaxError) {
        onMessage({ type: "error", message: error.message });
        stop();
        return;
      }
      onMessage({ type: "monitor_status", label: "Reconnecting", reconnecting: true });
      delay = 1000;
    }
    if (!controller.signal.aborted) timer = setTimeout(poll, delay);
  }
  poll();
  return stop;
}
