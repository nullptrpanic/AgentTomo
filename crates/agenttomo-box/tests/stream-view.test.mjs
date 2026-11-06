import test from "node:test";
import assert from "node:assert/strict";
import { TrafficModel } from "../src/playground/assets/traffic-model.mjs";
import { StreamModel } from "../src/playground/assets/stream-model.mjs";
import { previewContent } from "../src/playground/assets/file-preview.mjs";

test("binary and unknown-extension payloads can request an explicit hex display", () => {
  const content = previewContent(new TextEncoder().encode("abc"), { forceHex: true });
  assert.equal(content.format, "hex");
  assert.ok(content.text.includes("61 62 63"));
});

test("stream lifecycle joins its HTTP exchange without losing request or raw details", () => {
  const model = new TrafficModel();
  const request = { kind: "network.http.request", data: { request_id: 7, url: "http://example/events", method: "GET", pid: 35 } };
  model.add(request);
  const opened = { kind: "network.stream.open", data: { request_id: 7, protocol: "sse" } };
  const closed = { kind: "network.stream.closed", data: { request_id: 7, protocol: "sse", complete: true } };
  assert.equal(model.add(opened)?.stream, opened);
  assert.equal(model.add(closed)?.streamClosed, closed);
  assert.equal(model.entries.get(7).request, request);
  assert.equal(model.visible("sse").length, 1);
});

test("late stream closure cannot resurrect an evicted exchange", () => {
  const model = new TrafficModel(1);
  for (const request_id of [1, 2]) model.add({ kind: "network.http.request", data: { request_id } });
  assert.equal(model.add({ kind: "network.stream.closed", data: { request_id: 1 } }), null);
  assert.equal(model.entries.size, 1);
});

test("active streams survive ordinary request churn and can still close", () => {
  const model = new TrafficModel(2);
  model.add({ kind: "network.http.request", data: { request_id: 1 } });
  model.add({ kind: "network.stream.open", data: { request_id: 1, protocol: "sse" } });
  for (const request_id of [2,3,4]) model.add({ kind: "network.http.request", data: { request_id } });
  assert.ok(model.entries.has(1));
  assert.equal(model.entries.size, 2);
  assert.equal(model.add({ kind: "network.stream.closed", data: { request_id: 1 } })?.id, 1);
  model.add({ kind: "network.http.request", data: { request_id: 5 } });
  assert.equal(model.entries.has(1), false);
  assert.equal(model.add({ kind: "network.http.response", data: { request_id: 2 } }), null);
});

test("sliding snapshots remove expired selections and follow only when requested", () => {
  const model = new StreamModel();
  const snapshot = ids => ({ messages: ids.map(id => ({ id })) });
  model.update(snapshot([1, 2, 3]));
  assert.equal(model.selected, 3);
  model.select(2);
  model.update(snapshot([2, 3, 4]));
  assert.equal(model.selected, 2);
  assert.equal(model.follow, false);
  assert.equal(model.update(snapshot([3, 4, 5])), true);
  assert.equal(model.selected, null);
  assert.deepEqual(model.messages.map(message => message.id), [3, 4, 5]);
  model.latest();
  assert.equal(model.selected, 5);
  model.update(snapshot([4, 5, 6]));
  assert.equal(model.selected, 6);
});
