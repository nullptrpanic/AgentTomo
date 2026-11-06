const valid = value => Number.isFinite(value) && value >= 0;

// New summaries have a real proxy start; old events keep their recorded time.
export function eventTime(event) {
  if (valid(event.data?.started_unix_ns)) return event.data.started_unix_ns;
  return valid(event.observed_unix_ns) ? event.observed_unix_ns : null;
}

export function orderByStart(entries) {
  // Sort only the projection. Retention, process state and raw evidence keep
  // arrival order, so late summaries cannot revive or extend exited processes.
  return [...entries].sort((a, b) => (eventTime(a.event) ?? Infinity) - (eventTime(b.event) ?? Infinity) || a.id - b.id);
}
