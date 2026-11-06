import test from "node:test";
import assert from "node:assert/strict";
import { eventTime, orderByStart } from "../src/playground/assets/event-time.mjs";

test("late transfer summaries display their actual proxy start and sort before process exit", () => {
  const events = [
    { id: 1, event: {kind:'process.exec', observed_unix_ns:100, data:{}} },
    { id: 2, event: {kind:'process.exit', observed_unix_ns:300, data:{}} },
    { id: 3, event: {kind:'network.transfer', observed_unix_ns:400, data:{started_unix_ns:200,completed_unix_ns:390}} },
  ];
  assert.equal(eventTime(events[2].event),200);
  assert.deepEqual(orderByStart(events).map(entry=>entry.id),[1,3,2]);
  assert.deepEqual(events.map(entry=>entry.id),[1,2,3],"raw arrival order must remain intact");
  assert.equal(events[2].event.observed_unix_ns,400);
});

test("equal starts keep arrival order and legacy records fall back without inventing a start", () => {
  const events=[
    {id:1,event:{observed_unix_ns:50,data:{}}},
    {id:2,event:{observed_unix_ns:80,data:{started_unix_ns:50}}},
    {id:3,event:{observed_unix_ns:90,data:{started_unix_ns:-1}}},
    {id:4,event:{data:{}}},
  ];
  assert.deepEqual(orderByStart(events).map(entry=>entry.id),[1,2,3,4]);
  assert.equal(eventTime(events[2].event),90);
  assert.equal(eventTime(events[3].event),null);
});
