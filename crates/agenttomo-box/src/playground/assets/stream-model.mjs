// Server snapshots are authoritative: expired IDs and payloads never survive
// in a second browser-side history after the disk window moves.
export class StreamModel {
  constructor() { this.messages = []; this.selected = null; this.follow = true; }
  update(snapshot) {
    this.snapshot = snapshot;
    this.messages = snapshot.messages;
    const expired = this.selected !== null && !this.messages.some(message => message.id === this.selected);
    if (this.follow) this.selected = this.messages.at(-1)?.id ?? null;
    else if (expired) this.selected = null;
    return expired;
  }
  select(id) { this.follow = false; this.selected = this.messages.some(message => message.id === id) ? id : null; }
  latest() { this.follow = true; this.selected = this.messages.at(-1)?.id ?? null; }
}
