import { eventTime, orderByStart } from "./event-time.mjs";
import { httpEventField, httpRequestTarget } from "./traffic-model.mjs";

const validId = value => Number.isSafeInteger(value) && value >= 0;
const amount = value => Number.isFinite(value) && value >= 0 ? `${value} B` : "—";
const duration = data => Number.isFinite(data?.duration_ns) ? `${(data.duration_ns / 1e6).toFixed(1)} ms` : null;
const join = parts => parts.filter(part => part !== null && part !== undefined && part !== "").join(" · ");
const key = (entry, type, id) => `${entry.nodeId}:${type}:${id}`;
const start = card => card.started ?? eventTime(card.event) ?? Infinity;
const chronological = cards => cards.sort((a, b) => start(a) - start(b) || (a.firstId ?? a.id) - (b.firstId ?? b.id));

function group(groups, entry, type, id) {
  const identity = key(entry, type, id);
  if (!groups.has(identity)) groups.set(identity, { id: identity, nodeId: entry.nodeId, type, records: [], children: [] });
  return groups.get(identity);
}

function finish(card, representative) {
  card.records = orderByStart(card.records);
  card.event = representative.event;
  card.started = eventTime(card.records[0].event);
  card.firstId = Math.min(...card.records.map(entry => entry.id));
  card.activity = Math.max(...card.records.map(entry => entry.id));
  return card;
}

function httpSummary(card) {
  const row = {};
  for (const entry of card.records) row[httpEventField(entry.event.kind)] = entry;
  const request = row.request?.event.data;
  const response = row.response?.event.data;
  const body = row.responseBody?.event.data;
  const stream = row.stream?.event.data;
  const closed = row.streamClosed?.event.data;
  card.requestId = card.records[0].event.data.request_id;
  card.label = `HTTP · #${card.requestId}`;
  const target = httpRequestTarget(request);
  card.title = target ? `${request.method || "HTTP"} ${target}` : `HTTP request #${card.requestId}`;
  card.description = join([
    row.error ? `Failed: ${row.error.event.data.error}` : response?.status_code,
    stream || closed ? `${(stream?.protocol || closed?.protocol)?.toUpperCase() || "Stream"} · ${closed ? closed.complete === false ? "Interrupted" : "Closed" : "Live"}`
      : row.error ? null : body ? body.body?.complete ? "Complete" : "Incomplete" : response ? "Receiving" : "Pending",
    row.requestBody ? `↑ ${amount(row.requestBody.event.data.body?.observed_bytes)}` : null,
    body ? `↓ ${amount(body.body?.observed_bytes)}` : null,
    closed?.error, duration(closed || body),
    body?.body?.truncated ? "Capture truncated" : null,
    !request ? "Request headers not in retained window" : null,
  ]);
  return finish(card, row.request || card.records[0]);
}

function tcpSummary(card) {
  const latest = kind => card.records.findLast(entry => entry.event.kind === kind);
  const transfer = latest("network.transfer"), upstream = latest("network.upstream");
  const failure = latest("network.tls.error");
  const protocol = latest("network.protocol");
  const request = latest("network.http.request");
  let domain = protocol?.event.data.domain;
  if (!domain && request) { try {
    const data = request.event.data, target = httpRequestTarget(data);
    domain = new URL(data.method === "CONNECT" ? `http://${target}` : target).hostname;
  } catch { /* Only use a readable request authority. */ } }
  const destination = card.records.find(entry => entry.event.data.destination)?.event.data.destination;
  const data = transfer?.event.data || failure?.event.data || upstream?.event.data;
  card.connectionId = card.records[0].event.data.connection_id;
  card.label = `TCP · #${card.connectionId}`;
  card.title = domain && destination ? `${domain} (${destination})` : domain || destination || "Address unavailable";
  card.description = join([data?.status || (data?.error ? "Failed" : "Observed"), data?.error,
    transfer ? `↑ ${amount(data.bytes_up)} · ↓ ${amount(data.bytes_down)}` : null,
    duration(data), card.requestCount ? `${card.children.length === card.requestCount ? card.requestCount : `${card.children.length}/${card.requestCount}`} HTTP requests` : null]);
  return finish(card, transfer || failure || upstream || protocol || card.records[0]);
}

function udpSummary(card) {
  const transfer = card.records.findLast(entry => entry.event.kind === "network.udp.transfer");
  const data = transfer?.event.data;
  const totals = { bytes_up: 0, bytes_down: 0, packets_up: 0, packets_down: 0 };
  for (const { event } of card.records) {
    const direction = event.kind === "network.udp.send" ? "up" : event.kind === "network.udp.receive" ? "down" : null;
    if (direction) { totals[`bytes_${direction}`] += event.data.bytes ?? 0; totals[`packets_${direction}`]++; }
  }
  const counts = data || totals;
  const error = card.records.findLast(entry => entry.event.data.error)?.event.data.error;
  card.flowId = card.records[0].event.data.flow_id;
  card.label = `UDP · #${card.flowId}`;
  card.title = card.records[0].event.data.destination || card.records[0].event.data.peer || "Address unavailable";
  card.description = join([data?.status, error,
    `↑ ${counts.packets_up ?? "—"} packets · ${amount(counts.bytes_up)}`,
    `↓ ${counts.packets_down ?? "—"} packets · ${amount(counts.bytes_down)}`,
    !transfer ? "retained window" : duration(data)]);
  return finish(card, transfer || card.records[0]);
}

// A stateless projection of the shared bounded window. IDs are scoped to the
// owning process lifetime; neither destination nor packet size is an identity.
export function networkActivities(entries, visible = entries) {
  const allowed = new Set(visible.map(entry => entry.id));
  const flows = new Map(), requests = new Map(), cards = [];
  for (const entry of entries) {
    const { kind, data: d = {} } = entry.event;
    if (kind.startsWith("network.udp.") && validId(d.flow_id)) {
      group(flows, entry, "udp", d.flow_id).records.push(entry);
      continue;
    }
    const isTcp = kind.startsWith("network.") && !kind.startsWith("network.udp.") && validId(d.connection_id);
    const isHttp = httpEventField(kind) && validId(d.request_id);
    if (isTcp) group(flows, entry, "tcp", d.connection_id).records.push(entry);
    if (isHttp) group(requests, entry, "http", d.request_id).records.push(entry);
    if (!isTcp && !isHttp && allowed.has(entry.id)) cards.push(entry);
  }
  const matches = card => card.records.some(entry => allowed.has(entry.id));
  for (const request of requests.values()) {
    const entry = request.records.find(entry => validId(entry.event.data.connection_id));
    const parent = entry && flows.get(key(entry, "tcp", entry.event.data.connection_id));
    if (parent) {
      // A partial legacy record can lack the connection ID; associate only if
      // another retained record of this same request explicitly supplies it.
      parent.records.push(...request.records.filter(entry => !validId(entry.event.data.connection_id)));
      parent.requestCount = (parent.requestCount ?? 0) + 1;
      if (matches(request)) parent.children.push(httpSummary(request));
    } else if (matches(request)) cards.push(httpSummary(request));
  }
  for (const flow of flows.values()) {
    if (!matches(flow)) continue;
    chronological(flow.children);
    cards.push(flow.type === "tcp" ? tcpSummary(flow) : udpSummary(flow));
  }
  return chronological(cards);
}
