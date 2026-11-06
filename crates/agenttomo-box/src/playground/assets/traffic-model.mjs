const fields = new Map([
  ["network.http.request", "request"], ["network.http.response", "response"],
  ["network.http.request.body", "requestBody"], ["network.http.response.body", "responseBody"],
  ["network.http.error", "error"],
  ["network.stream.open", "stream"], ["network.stream.closed", "streamClosed"],
]);

export class TrafficModel {
  constructor(limit = 1000) {
    this.limit = limit;
    this.entries = new Map();
    this.evictedThrough = 0;
    this.omitted = 0;
  }
  add(event) {
    const field = fields.get(event.kind), id = event.data?.request_id;
    if (!field || !Number.isSafeInteger(id) || id <= this.evictedThrough && !this.entries.has(id)) return null;
    let row = this.entries.get(id);
    if (!row) {
      row = { id };
      this.entries.set(id, row);
    }
    row[field] = event;
    row.latest = event;
    if (this.entries.size > this.limit) {
      const inactive = [...this.entries.values()].filter(row => !row.stream || row.streamClosed);
      const oldest = Math.min(...(inactive.length ? inactive.map(row => row.id) : this.entries.keys()));
      this.entries.delete(oldest);
      this.evictedThrough = Math.max(this.evictedThrough, oldest);
      this.omitted++;
    }
    return row;
  }
  visible(query = "") {
    const filter = query.trim().toLowerCase();
    return [...this.entries.values()].sort((a, b) => a.id - b.id).filter(row => {
      const data = row.request?.data || row.latest.data;
      return !filter || [data.url, data.method, data.pid, row.response?.data.status_code, row.error?.data.error, row.stream?.data.protocol].join(" ").toLowerCase().includes(filter);
    });
  }
}
