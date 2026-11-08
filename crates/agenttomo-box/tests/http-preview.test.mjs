import { test } from "node:test";
import assert from "node:assert/strict";
import { httpPreview } from "../src/playground/assets/http-preview.mjs";

const body = { status: "complete", captured_bytes: 20, observed_bytes: 20, complete: true };
test("HTTP decoded text respects Content-Type charset and retains an explicit raw view", () => {
  const headers = [["Content-Encoding", "gzip"], ["Content-Type", 'text/html; charset="gbk"']];
  const decoded = httpPreview({ bytes: new Uint8Array([0xd6, 0xd0, 0xce, 0xc4]), decoding: "complete" }, headers, body);
  assert.equal(decoded.text, "中文");
  assert.match(decoded.status, /decompressed/);
  const raw = httpPreview({ bytes: new Uint8Array([0x1f, 0x8b, 8]), decoding: "identity" }, headers, body, true);
  assert.equal(raw.format, "hex");
  assert.match(raw.text, /1f 8b 08/);
  assert.match(raw.status, /Original entity bytes/);
});
test("incomplete and unsupported content is never presented as fully decoded", () => {
  const bytes = new TextEncoder().encode('{"ok":true}');
  const headers = [["Content-Encoding", "gzip"]];
  const prefix = httpPreview({ bytes, decoding: "incomplete" }, headers, { ...body, status: "truncated", truncated: true });
  assert.equal(prefix.partial, true);
  assert.equal(prefix.json, false);
  assert.match(prefix.status, /checksum not verified/);
  for (const decoding of ["unsupported", "invalid", "identity"]) {
    const preview = httpPreview({ bytes, decoding }, headers, body);
    assert.equal(preview.format, "hex");
    assert.doesNotMatch(preview.status, / · decompressed/);
  }
});
test("empty HTTP entities remain empty even when representation headers mention gzip", () => {
  const preview = httpPreview({ bytes: new Uint8Array(), decoding: "identity" }, [["Content-Encoding", "gzip"]], { ...body, captured_bytes: 0, observed_bytes: 0 });
  assert.equal(preview.text, "");
  assert.equal(preview.format, "text");
  assert.equal(preview.partial, false);
  assert.doesNotMatch(preview.status, /not decoded|failed/);
});
