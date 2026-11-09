import { orderByStart } from "./event-time.mjs";
import { networkActivities } from "./network-activity.mjs";

const lifecycle = new Set(["process.exec", "process.fork", "process.vfork", "process.clone", "process.exit"]);
const isLifecycle = entry => lifecycle.has(entry.event.kind);
function countBehaviors(behaviors) {
  const counts = new Map();
  for (const entry of behaviors) counts.set(entry.nodeId, (counts.get(entry.nodeId) ?? 0) + 1 + (entry.children?.length ?? 0));
  return counts;
}
const total = counts => [...counts.values()].reduce((sum, count) => sum + count, 0);

// Both presentations use the same identities, retention and filtering. Never
// filter the underlying tree: even a hidden exec/fork determines ownership.
export function activitySnapshot(tree, filter) {
  const { nodes, entries } = tree.processView();
  const visible = orderByStart(entries.filter(entry => filter.allows(entry.event.kind)));
  const counts = new Map();
  for (const node of nodes.values()) node.activity = 0;
  for (const entry of visible) {
    counts.set(entry.nodeId, (counts.get(entry.nodeId) ?? 0) + 1);
    for (let node = nodes.get(entry.nodeId); node; node = nodes.get(node.parentId)) {
      node.activity = Math.max(node.activity, entry.id);
    }
  }
  const processRecords = new Map();
  for (const entry of visible.filter(isLifecycle)) {
    if (!processRecords.has(entry.nodeId)) processRecords.set(entry.nodeId, []);
    processRecords.get(entry.nodeId).push(entry);
  }
  const records = entries.filter(entry => !isLifecycle(entry));
  const behaviors = networkActivities(records, visible.filter(entry => !isLifecycle(entry)));
  const behaviorCounts = countBehaviors(behaviors);
  const totalBehaviorCounts = countBehaviors(networkActivities(records));
  return { nodes, entries, visible, counts, processRecords, behaviors, behaviorCounts, totalBehaviorCounts,
    behaviorCount: total(behaviorCounts), totalBehaviorCount: total(totalBehaviorCounts) };
}
