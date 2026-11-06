import test from "node:test";
import assert from "node:assert/strict";
import { EventFilter } from "../src/playground/assets/event-filter.mjs";

test("exact event switches leave sibling kinds enabled", () => {
  const filter = new EventFilter();
  filter.set("file.open", false);
  filter.set("memory.mmap", false);
  assert.equal(filter.allows("file.open"), false);
  assert.equal(filter.allows("memory.mmap"), false);
  assert.equal(filter.allows("file.rename"), true);
  assert.equal(filter.allows("network.upstream"), true);
  assert.equal(filter.groupState("file").indeterminate, true);
});

test("wildcards affect their entire group including future kinds", () => {
  const filter = new EventFilter();
  filter.set("file.*", false);
  filter.observe("file.rename");
  assert.equal(filter.allows("file.open"), false);
  assert.equal(filter.allows("file.rename"), false);
  assert.equal(filter.allows("filex.open"), true);
  assert.deepEqual(filter.groupState("file"), { checked: false, indeterminate: false });
  filter.set("file.open", true);
  assert.equal(filter.allows("file.open"), true);
  assert.equal(filter.allows("file.rename"), false);
  assert.equal(filter.groupState("file").indeterminate, true);
  filter.set("file.*", true);
  assert.equal(filter.allows("file.rename"), true);
  assert.deepEqual(filter.groupState("file"), { checked: true, indeterminate: false });
});

test("all and none apply to new kinds and restore individual selections", () => {
  const filter = new EventFilter();
  filter.set("*", false);
  filter.observe("custom.action");
  assert.equal(filter.allows("custom.action"), false);
  filter.set("network.*", true);
  assert.equal(filter.allows("network.protocol"), true);
  assert.equal(filter.allows("file.open"), false);
  filter.set("*", true);
  assert.equal(filter.allows("custom.action"), true);
  assert.equal(filter.allows("file.open"), true);
  assert.equal(filter.active, false);
});
