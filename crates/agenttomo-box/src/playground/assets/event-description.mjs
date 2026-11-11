// Shared descriptions for native process events and saved v1 records.
import { httpRequestTarget } from "./traffic-model.mjs";
export const formatCommand = (argv) => argv.map(arg => /\s/.test(arg) ? JSON.stringify(arg) : arg).join(" ");

export function describeBehavior(kind, data, domain) {
  const result = data.restart ? "interrupted; restart pending" : data.errno ? `errno ${data.errno}` : data.result ?? "result unavailable";
  const destination = domain ? `${domain} (${data.destination ?? "address unavailable"})` : data.destination;
  if (kind === "file.open") return `${data.syscall || "open"} ${data.path ?? "[unreadable path]"} → ${typeof result === "number" && result >= 0 ? `FD ${result}` : result}`;
  if (kind === "memory.mmap") {
    const target = data.anonymous ? "anonymous" : data.resolved_path ?? `FD ${data.fd ?? "?"}`;
    const outcome = data.errno || data.restart ? result : data.mapped_address ?? result;
    const prot = Number.isInteger(data.prot) ? `0x${data.prot.toString(16)}` : "?";
    const flags = Number.isInteger(data.flags) ? `0x${data.flags.toString(16)}` : "?";
    const elapsed = Number.isFinite(data.duration_ns) ? ` · ${(data.duration_ns / 1e6).toFixed(3)} ms` : "";
    return `mmap ${target} · ${data.length ?? "?"} B · offset ${data.offset ?? "?"} · prot ${prot} · flags ${flags} → ${outcome}${elapsed}`;
  }
  if (kind === "network.connect") {
    if (data.transport === "udp" && data.disconnect) return `udp · clear default peer (AF_UNSPEC) → ${result}`;
    return `${data.transport || "socket"}${data.transport === "udp" ? " · set default peer" : ""} ${destination ?? "[address unavailable]"} · ${data.redirected ? "local proxy" : "direct"} → ${result}${data.skip_reason ? ` · ${data.skip_reason}` : ""}`;
  }
  if (kind === "network.upstream") return `tcp ${destination} · upstream ${data.status}${data.error ? `: ${data.error}` : ""}`;
  if (kind === "network.transfer") return `tcp ${destination ?? "connection"} · ${data.status} · ↑ ${data.bytes_up ?? "?"} B · ↓ ${data.bytes_down ?? "?"} B${data.error ? ` · ${data.error}` : ""}`;
  if (kind === "network.protocol") return `${data.protocol} ${data.domain ?? "[domain unavailable]"} · ${data.domain_source} · ${data.destination ?? "address unavailable"}`;
  if (kind === "network.http.request") return `${data.method} ${httpRequestTarget(data)} · request #${data.request_id}`;
  if (kind === "network.stream.open") return `${data.protocol} · request #${data.request_id} · live messages in Traffic`;
  if (kind === "network.stream.closed") return `${data.protocol} · request #${data.request_id} · ${data.complete ? "closed" : "interrupted"}${data.error ? ` · ${data.error}` : ""}`;
  if (kind === "network.http.response") return `${data.version} ${data.status_code} · request #${data.request_id}`;
  if (kind === "network.http.request.body" || kind === "network.http.response.body") return `${kind.includes("request.body") ? "request" : "response"} #${data.request_id} · ${data.body?.status} · ${data.body?.captured_bytes} / ${data.body?.observed_bytes} B retained`;
  if (kind === "network.http.error" || kind === "network.tls.error") return `${data.stage || "HTTP exchange"} · ${data.error}`;
  if (kind === "network.udp.send" || kind === "network.udp.receive") {
    if (data.network_backend === "netfilter_tproxy") return `udp ${kind.endsWith("send") ? "↑" : "↓"} ${data.peer ?? data.destination} · 1 datagram · ${data.bytes} B${data.participants?.length > 1 ? ` · ${data.participants.length} flow participants` : ""}`;
    const peer = data.destination ?? data.source ?? (data.messages ? "multiple messages" : "peer unavailable");
    const count = data.datagrams ?? (typeof data.result === "number" && data.result >= 0 ? 1 : 0);
    const bytes = data.bytes ?? (data.messages ? data.messages.reduce((sum, message) => sum + (message.bytes ?? 0), 0) : "?");
    return `${data.syscall || kind.split(".").at(-1)} ${peer} · ${count} datagrams · ${bytes} B → ${result}${data.skip_reason ? ` · ${data.skip_reason}` : ""}`;
  }
  if (kind === "network.udp.peer") return `getpeername ${data.destination ?? "peer unavailable"} → ${result}`;
  if (kind === "network.udp.transfer") return `socket ${data.socket_id} · ${data.destination} · ↑ ${data.packets_up} datagrams / ${data.bytes_up} B · ↓ ${data.packets_down} datagrams / ${data.bytes_down} B`;
  if (kind === "network.udp.error") return `socket ${data.socket_id} · ${data.stage}: ${data.error}`;
  return JSON.stringify(data);
}
