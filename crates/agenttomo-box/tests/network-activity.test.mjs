import test from "node:test";
import assert from "node:assert/strict";
import { networkActivities } from "../src/playground/assets/network-activity.mjs";

function entry(id, kind, data = {}, nodeId = "process-1") {
  return { id, nodeId, description: kind, event: { sequence: id, kind, observed_unix_ns: id * 1e6, data } };
}
const tcp = (id, kind, data = {}, owner) => entry(id, kind, { connection_id: 12, destination: "153.3.238.28:443", ...data }, owner);
const http = (id, kind, data = {}, owner) => tcp(id, `network.http.${kind}`, { request_id: 7, started_unix_ns: 5e6, ...data }, owner);

test("TCP is one parent and each HTTP exchange is one child, with raw evidence retained", () => {
  const entries = [tcp(1, "network.upstream", { status: "connected" }),
    tcp(2, "network.protocol", { domain: "www.baidu.com", protocol: "tls" }),
    http(3, "request", { method: "GET", url: "https://www.baidu.com/" }),
    http(4, "request.body", { body: { observed_bytes: 0 } }),
    http(5, "response", { status_code: 200 }),
    http(6, "response.body", { body: { observed_bytes: 2443, complete: true }, duration_ns: 13e6 }),
    tcp(7, "network.transfer", { started_unix_ns: 0, status: "closed", bytes_up: 719, bytes_down: 4085 })];
  const before = JSON.stringify(entries), [flow] = networkActivities(entries);
  assert.equal(networkActivities(entries).length, 1);
  assert.equal(flow.type, "tcp");
  assert.equal(flow.started, 0);
  assert.match(flow.title, /www\.baidu\.com.*153\.3\.238\.28:443/);
  assert.match(flow.description, /719 B.*4085 B/);
  assert.equal(flow.records.length, 7);
  assert.equal(flow.children.length, 1);
  const [request] = flow.children;
  assert.equal(request.event.sequence, 3, "click must target the request inspector");
  assert.equal(request.title, "GET https://www.baidu.com/");
  assert.match(request.description, /200/);
  assert.match(request.description, /2443 B/);
  assert.doesNotMatch(request.description, /4085/, "connection bytes are not request body bytes");
  assert.equal(JSON.stringify(entries), before);
});

test("keep-alive requests are siblings ordered by start, not request ID or response completion", () => {
  const cards = networkActivities([
    http(1, "request", { request_id: 8, started_unix_ns: 30, method: "GET", url: "/second" }),
    http(2, "request", { request_id: 9, started_unix_ns: 20, method: "GET", url: "/first" }),
    http(3, "response", { request_id: 8, started_unix_ns: 30, status_code: 200 }),
    http(4, "response", { request_id: 9, started_unix_ns: 20, status_code: 201 }),
    http(5, "request", { connection_id: 13, request_id: 10, url: "/first" }),
  ]);
  assert.equal(cards.length, 2, "same destination does not mean same connection");
  assert.deepEqual(cards[0].children.map(card => card.requestId), [9, 8]);
  assert.deepEqual(cards[1].children.map(card => card.requestId), [10]);
});

test("late stages update stable cards without mixing process lifetimes", () => {
  const request = http(1, "request", { method: "POST", url: "/api" });
  const [before] = networkActivities([request]);
  const cards = networkActivities([request, http(2, "response", { status_code: 201 }),
    tcp(3, "network.transfer", { started_unix_ns: 1, status: "closed" }),
    http(4, "request", { url: "/other" }, "process-2")]);
  assert.equal(cards.length, 2);
  assert.equal(cards[0].id, before.id);
  assert.equal(cards[0].children[0].id, before.children[0].id);
  assert.ok(cards[0].activity > before.activity);
  assert.match(cards[0].children[0].description, /201/);
  assert.notEqual(cards[1].id, cards[0].id);
});

test("UDP groups actual datagrams, never deduplicates equal lengths or double-counts final totals", () => {
  const udp = (id, kind, data) => entry(id, `network.udp.${kind}`, { flow_id: 19, destination: "100.100.2.136:53", ...data });
  const entries = [udp(1, "send", { bytes: 31 }), udp(2, "send", { bytes: 31 }),
    udp(3, "receive", { bytes: 93 }), udp(4, "receive", { bytes: 117 })];
  const [live] = networkActivities(entries);
  assert.equal(networkActivities(entries).length, 1);
  assert.equal(live.type, "udp");
  assert.match(live.description, /↑ 2 packets · 62 B/);
  assert.match(live.description, /↓ 2 packets · 210 B/);
  assert.match(live.description, /retained window/);
  entries.push(udp(5, "transfer", { bytes_up: 62, bytes_down: 210, packets_up: 2, packets_down: 2, status: "scope_finished", started_unix_ns: 1 }));
  const [done] = networkActivities(entries);
  assert.equal(done.id, live.id);
  assert.match(done.description, /↑ 2 packets · 62 B.*↓ 2 packets · 210 B/);
  assert.doesNotMatch(done.description, /retained window/);
  entries.push(udp(6, "send", { flow_id: 20, bytes: 31 }));
  assert.equal(networkActivities(entries).length, 2);
});

test("filters remove HTTP children, retain TCP context, and never alter known response state", () => {
  const entries = [tcp(1, "network.upstream"), http(2, "request", { url: "/visible" }),
    http(3, "response", { status_code: 204 }), http(4, "request", { request_id: 8, url: "/hidden" })];
  const [flow] = networkActivities(entries, [entries[1]]);
  assert.equal(flow.children.length, 1);
  assert.match(flow.children[0].description, /204/);
  assert.equal(flow.records.length, 4, "evidence remains available behind the summary");
  assert.equal(networkActivities(entries, [entries[0]])[0].children.length, 0);
  assert.deepEqual(networkActivities(entries, []), []);
});

test("partial histories do not invent URLs or transport IDs, and old unkeyed events stay separate", () => {
  const body = http(9, "response.body", { body: { observed_bytes: 11, complete: true } });
  const cards = networkActivities([body, entry(10, "network.http.error", { request_id: 8, error: "reset" }),
    entry(11, "network.udp.send", { destination: "same", bytes: 31 }),
    entry(12, "network.udp.send", { destination: "same", bytes: 31 }),
    entry(13, "file.open", { path: "/tmp/test" })]);
  assert.equal(cards.length, 5);
  assert.equal(cards[0].children[0].title, "HTTP request #7");
  assert.equal(cards[1].type, "http", "missing connection ID does not invent a TCP parent");
  assert.match(cards[1].description, /reset/);
  assert.equal(cards[4].event.kind, "file.open");
  assert.equal(networkActivities([body])[0].records.length, 1, "projection keeps no independent history");
});

test("stream lifecycle updates one HTTP child and equal start times retain arrival order", () => {
  const entries = [http(1, "request", { request_id: 9, url: "/events" }),
    tcp(2, "network.stream.open", { request_id: 9, started_unix_ns: 5e6, protocol: "sse" }),
    http(3, "request", { request_id: 8, url: "/other" })];
  const [live] = networkActivities(entries);
  assert.deepEqual(live.children.map(card => card.requestId), [9, 8]);
  assert.match(live.children[0].description, /SSE.*Live/);
  entries.push(tcp(4, "network.stream.closed", { request_id: 9, started_unix_ns: 5e6 }));
  const [done] = networkActivities(entries);
  assert.equal(done.children[0].id, live.children[0].id);
  assert.match(done.children[0].description, /SSE.*Closed/);
});

test("folded TLS errors and interrupted streams remain visible in the summary", () => {
  const [failed] = networkActivities([tcp(1, "network.upstream", { status: "connected" }),
    tcp(2, "network.tls.error", { error: "certificate rejected" })]);
  assert.match(failed.description, /Failed.*certificate rejected/);
  const [flow] = networkActivities([http(3, "request", { url: "/stream" }),
    tcp(4, "network.stream.closed", { request_id: 7, protocol: "websocket", complete: false, error: "reset" })]);
  assert.match(flow.children[0].description, /WEBSOCKET.*Interrupted.*reset/);
});
