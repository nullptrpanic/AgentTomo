import { createObservation } from "./observation.mjs";
import { panelCloseIcon, panelOpenIcon, externalLinkIcon } from "./icons.mjs";

const $ = id => document.getElementById(id);
const params = new URL(location.href).searchParams;
const token = params.get("token") || "";
const monitorOnly = params.has("monitor");
const observation = createObservation(token);
let finalStatus = false;

function setStatus(label, state) {
  $("status").textContent = label;
  $("status").dataset.state = state;
}
function notice(message) {
  $("notice").hidden = false;
  $("notice").textContent = message;
}

const toggle = $("toggle-monitor");
const open = $("open-monitor");
toggle.innerHTML = panelCloseIcon;
open.innerHTML = externalLinkIcon;
toggle.addEventListener("click", () => {
  const collapsed = document.body.classList.toggle("monitor-collapsed");
  toggle.setAttribute("aria-expanded", String(!collapsed));
  const label = collapsed ? "Show monitor" : "Hide monitor";
  toggle.setAttribute("aria-label", label);
  toggle.title = label;
  toggle.innerHTML = collapsed ? panelOpenIcon : panelCloseIcon;
});

function onMessage(message) {
  switch (message.type) {
    case "session": {
      observation.session(message);
      const url = new URL("/", location.href);
      url.searchParams.set("token", token);
      url.searchParams.set("monitor", message.id);
      open.href = url.href;
      open.removeAttribute("aria-disabled");
      open.removeAttribute("tabindex");
      if (monitorOnly) {
        document.title = `AgentTomo · Monitor ${message.id.slice(0, 8)}`;
        $("trace-session").textContent = `session ${message.id.slice(0, 8)}`;
      }
      setStatus(monitorOnly ? "Loading history" : "Starting container", "starting");
      break;
    }
    case "ready": observation.start(message.pid); setStatus(monitorOnly ? "Following session" : "Connected", "live"); break;
    case "trace": observation.add(message.event); break;
    case "root_exit": setStatus(`Shell exited (${message.code}) · collecting`, "starting"); break;
    case "complete": finalStatus = true; setStatus("Collection complete", "complete"); break;
    case "monitor_status": setStatus(message.label, message.reconnecting ? "starting" : "live"); break;
    case "monitor_end": {
      finalStatus = true;
      const error = message.result?.error;
      const complete = message.result?.reason === "completed" && !message.incomplete && !error;
      setStatus(error ? "Session error" : complete ? "Collection complete" : "Session ended", error ? "error" : "complete");
      if (error) notice(error);
      else if (message.incomplete) notice("The recording ended with an incomplete event. All complete events are shown.");
      else if (!complete) notice("This session stopped before collection completed. Saved events remain available.");
      break;
    }
    case "error": finalStatus = true; setStatus("Session error", "error"); notice(message.message); break;
    case "disconnected": if (!finalStatus) setStatus("Disconnected", "starting"); break;
  }
}

try {
  if (monitorOnly) {
    document.body.classList.add("monitor-only");
    $("page-name").textContent = "Monitor";
    $("page-description").textContent = "Read-only view of the same session.";
    $("page-lifecycle").textContent = "Closing this monitor does not stop the session.";
    $("empty-trace").querySelector("p").textContent = "Activity from the original workspace will appear here.";
    const id = params.get("monitor");
    if (!/^[a-f\d]{32}$/i.test(id)) throw new Error("Invalid session ID. Open a monitor from the workspace page.");
    const { followMonitor } = await import("./monitor.mjs");
    followMonitor({ id, token, onMessage });
  } else {
    const { startWorkspace } = await import("./workspace.mjs");
    startWorkspace({ token, onMessage, notice });
  }
} catch (error) {
  onMessage({ type: "error", message: error.message });
}
