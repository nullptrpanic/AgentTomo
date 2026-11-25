import { createObservation } from "./observation.mjs";
import { panelCloseIcon, panelOpenIcon, externalLinkIcon } from "./icons.mjs";

const $ = id => document.getElementById(id);
const params = new URL(location.href).searchParams;
const token = params.get("token") || "";
const monitorOnly = params.has("monitor");
if (token) {
  try {
    sessionStorage.setItem("agenttomo.access." + (params.get("monitor") || "owner"), token);
    const address = new URL(location.href);
    address.searchParams.delete("token");
    history.replaceState(null, "", address);
  } catch { /* Storage-disabled browsers retain the access link for reload. */ }
}
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

async function selectWorkspaceAgent() {
  const picker = $("workspace-agent");
  const trigger = picker.querySelector("summary");
  const choices = picker.querySelector(".agent-options");
  const template = choices.querySelector("template");
  const url = new URL("/api/agents", location.href);
  const response = await fetch(url, {headers: {Authorization: `Bearer ${token}`}});
  if (!response.ok) throw new Error(`Cannot load workspace agents (HTTP ${response.status}).`);
  const available = await response.json();
  const requested = params.get("agent");
  const current = requested ?? available[0] ?? "";
  const valid = requested === null || available.includes(requested);
  const ids = new Set([...available, "codex", "agentkit"]);
  if (!available.length) ids.add("");
  let selected;
  for (const id of ids) {
    const row = template.content.firstElementChild.cloneNode(true);
    const input = row.querySelector("input");
    const name = id === "codex" ? "Codex" : id || "Terminal only";
    row.querySelector("span").textContent = name;
    input.value = id;
    input.disabled = Boolean(id) && !available.includes(id);
    if (input.disabled && id === "agentkit") row.title = "Configure the AgentKit executable, model and credentials on the server.";
    if (valid && id === current) {
      input.checked = true;
      selected = input;
      trigger.querySelector("span").textContent = name;
    }
    choices.append(row);
  }
  trigger.setAttribute("aria-label", `Workspace agent, ${trigger.textContent}`);
  picker.hidden = false;
  function closeMenu() {
    picker.open = false;
    trigger.focus();
  }
  picker.addEventListener("toggle", () => {
    if (picker.open) (selected ?? choices.querySelector("input:enabled")).focus({ preventScroll: true });
  });
  picker.addEventListener("keydown", event => {
    if (event.key === "Escape" && picker.open) {
      event.preventDefault();
      closeMenu();
    } else if (event.target === trigger && ["ArrowDown", "ArrowUp"].includes(event.key)) {
      event.preventDefault();
      picker.open = true;
    } else if (event.key === "Enter" && event.target instanceof HTMLInputElement) {
      event.preventDefault();
      event.target.click();
    }
  });
  document.addEventListener("pointerdown", event => { if (!picker.contains(event.target)) picker.open = false; });
  picker.addEventListener("focusout", event => { if (!picker.contains(event.relatedTarget)) picker.open = false; });
  choices.addEventListener("click", event => { if (event.target === selected) closeMenu(); });
  choices.addEventListener("change", ({ target }) => {
    target.checked = false;
    if (selected) selected.checked = true;
    closeMenu();
    if (!finalStatus && $("status").dataset.state !== "error" &&
        !window.confirm("Switch agents? This closes the current sandbox and deletes its temporary files.")) return;
    const next = new URL(location.href);
    if (target.value) next.searchParams.set("agent", target.value);
    else next.searchParams.delete("agent");
    location.assign(next);
  });
  if (!valid) {
    throw new Error("The requested agent is unavailable. Choose an available option to open a workspace.");
  }
  return current;
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
      open.dataset.session = message.id;
      if (monitorOnly) {
        document.title = `AgentTomo · Monitor ${message.id.slice(0, 8)}`;
        $("trace-session").textContent = `session ${message.id.slice(0, 8)}`;
      }
      setStatus(monitorOnly ? "Loading history" : "Starting container", "starting");
      break;
    }
    case "ready":
      if (message.monitor_token) {
        const url = new URL("/", location.href);
        url.searchParams.set("monitor", open.dataset.session);
        url.searchParams.set("token", message.monitor_token);
        open.href = url.href;
        open.removeAttribute("aria-disabled");
        open.removeAttribute("tabindex");
      }
      observation.start(message.pid); setStatus(monitorOnly ? "Following session" : "Connected", "live"); break;
    case "trace": observation.add(message.event); break;
    case "root_exit": observation.stop("The sandbox shell exited."); setStatus(`Shell exited (${message.code}) · collecting`, "starting"); break;
    case "complete": observation.stop("Sandbox finished."); finalStatus = true; setStatus("Collection complete", "complete"); break;
    case "monitor_status": setStatus(message.label, message.reconnecting ? "starting" : "live"); break;
    case "monitor_expired":
      observation.stop("This sandbox is unavailable. Session recordings are deleted when it ends.");
      finalStatus = true;
      setStatus("Session ended", "complete");
      notice("This session's recording is unavailable. Recordings are deleted when the sandbox ends; previously loaded content may still appear on this page.");
      break;
    case "monitor_end": {
      observation.stop("Sandbox finished.");
      finalStatus = true;
      const error = message.result?.error;
      const complete = message.result?.reason === "completed" && !message.incomplete && !error;
      // The final batch may arrive just before cleanup returns 404. Both paths
      // end this disposable session; neither leaves a replayable recording.
      setStatus(error ? "Session error" : "Session ended", error ? "error" : "complete");
      if (error) notice(error);
      else if (message.incomplete) notice("The recording ended with an incomplete event. All complete events are shown.");
      else notice(`${complete ? "Collection completed." : "This session stopped before collection completed."} Recordings are deleted when the sandbox ends; previously loaded content may still appear on this page.`);
      break;
    }
    case "control_error": notice(message.message); break;
    case "error": observation.stop(`Sandbox stopped: ${message.message}`); finalStatus = true; setStatus("Session error", "error"); notice(message.message); break;
    case "disconnected":
      if (!finalStatus) {
        observation.stop("Sandbox disconnected. Session recordings are deleted during cleanup.");
        setStatus("Disconnected", "error");
      }
      break;
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
    const agent = await selectWorkspaceAgent();
    observation.control(startWorkspace({ token, agent, onMessage, notice, onView:name => { if (name === "chat") observation.view("agent"); } }));
  }
} catch (error) {
  onMessage({ type: "error", message: error.message });
}
