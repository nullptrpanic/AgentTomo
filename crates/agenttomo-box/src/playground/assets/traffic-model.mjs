import { eventTime } from "./event-time.mjs";

const fields = new Map([
  ["network.http.request", "request"], ["network.http.response", "response"],
  ["network.http.request.body", "requestBody"], ["network.http.response.body", "responseBody"],
  ["network.http.error", "error"],
  ["network.stream.open", "stream"], ["network.stream.closed", "streamClosed"],
]);

export const httpEventField = kind => fields.get(kind);
// Older recordings prefixed CONNECT's authority with the proxy transport scheme.
export const httpRequestTarget = (data = {}) => data.method === "CONNECT"
  ? data.domain || data.url?.replace(/^https?:\/\//i, "") : data.url;

export class TrafficModel {
  constructor(limit = 1000) {
    this.limit = limit;
    this.entries = new Map();
    this.seenRanges = [];
    this.omitted = 0;
  }
  add(event) {
    const field = httpEventField(event.kind), id = event.data?.request_id;
    if (!field || !Number.isSafeInteger(id) || id < 1) return null;
    let row = this.entries.get(id);
    if (!row) {
      if (!this.remember(id)) return null;
      row = { id };
      this.entries.set(id, row);
    }
    row[field] = event;
    if (Number.isSafeInteger(event.data.connection_id)) row.connectionId = event.data.connection_id;
    row.latest = event;
    if (this.entries.size > this.limit) {
      const inactive = [...this.entries.values()].filter(row => !row.stream || row.streamClosed);
      const oldest = inactive[0]?.id ?? this.entries.keys().next().value;
      this.entries.delete(oldest);
      this.omitted++;
    }
    return row;
  }
  // Request IDs are allocated in order, but callbacks on separate connections can arrive late.
  // Store contiguous observed ranges so a gap never stands for an unseen request.
  remember(id) {
    const ranges = this.seenRanges;
    let index = ranges.findIndex(([, end]) => end >= id - 1);
    if (index < 0) index = ranges.length;
    const range = ranges[index];
    if (range && range[0] <= id && id <= range[1]) return false;
    if (range && range[0] <= id + 1) {
      range[0] = Math.min(range[0], id);
      range[1] = Math.max(range[1], id);
      if (ranges[index + 1]?.[0] === range[1] + 1) range[1] = ranges.splice(index + 1, 1)[0][1];
    } else {
      ranges.splice(index, 0, [id, id]);
      if (ranges.length > Math.max(1024, this.limit)) ranges.shift();
    }
    return true;
  }
  matching(event) {
    const id = event.data?.request_id;
    if (Number.isSafeInteger(id)) return this.entries.has(id) ? [this.entries.get(id)] : [];
    const connection = event.data?.connection_id;
    return Number.isSafeInteger(connection) ? this.visible("", connection) : [];
  }
  visible(query = "", connectionId = null) {
    const filter = query.trim().toLowerCase();
    return [...this.entries.values()].sort((a, b) =>
      (eventTime(a.request || a.latest) ?? Infinity) - (eventTime(b.request || b.latest) ?? Infinity) || a.id - b.id
    ).filter(row => {
      if (connectionId !== null && row.connectionId !== connectionId) return false;
      const data = row.request?.data || row.latest.data;
      return !filter || [httpRequestTarget(data), data.method, data.pid, row.response?.data.status_code, row.error?.data.error, row.stream?.data.protocol].join(" ").toLowerCase().includes(filter);
    });
  }
}
