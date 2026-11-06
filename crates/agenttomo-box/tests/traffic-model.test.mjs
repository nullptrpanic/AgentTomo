import { test } from "node:test";
import assert from "node:assert/strict";
import { TrafficModel } from "../src/playground/assets/traffic-model.mjs";

const event = (kind, id, fields = {}) => ({ kind: `network.http.${kind}`, data: { request_id: id, started_unix_ns: id * 1000000, ...fields } });

test("traffic joins separate body records by request id, including keep-alive and late events", () => {
  const model = new TrafficModel();
  model.add(event("response.body", 2, { body: { offset: 7, captured_bytes: 3 } }));
  model.add(event("request", 1, { connection_id: 9, pid: 50, url: "https://example.test/a" }));
  model.add(event("request", 2, { connection_id: 9, pid: 50, url: "https://example.test/b" }));
  model.add(event("response", 1, { status_code: 200 }));
  assert.equal(model.entries.get(2).responseBody.data.body.offset, 7);
  assert.equal(model.entries.get(1).responseBody, undefined);
  assert.deepEqual(model.visible("example.test/b").map(row => row.id), [2]);
  assert.equal(model.visible("50").length, 2);
  assert.equal(model.add({ kind: "network.udp.send", data: {} }), null);
});

test("UI retention evicts whole exchanges and late bodies do not resurrect them", () => {
  const model = new TrafficModel(2);
  for (const id of [1, 2, 3]) model.add(event("request", id));
  assert.deepEqual([...model.entries.keys()], [2, 3]);
  assert.equal(model.omitted, 1);
  assert.equal(model.add(event("request.body", 1)), null);
  assert.equal(model.entries.size, 2);
});
