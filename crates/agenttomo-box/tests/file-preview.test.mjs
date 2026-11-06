import test from "node:test";
import assert from "node:assert/strict";
import { previewContent } from "../src/playground/assets/file-preview.mjs";

const encode = text => new TextEncoder().encode(text);

test("text and empty snapshots preserve bytes without adding line numbers to copied text", () => {
  assert.equal(previewContent(encode("one\ntwo\n")).text, "one\ntwo\n");
  assert.equal(previewContent(new Uint8Array()).format, "text");
  assert.equal(previewContent(new Uint8Array()).text, "");
});

test("JSON raw stays exact and pretty mode is disabled for incomplete or unsafe values", () => {
  const bytes = encode('{"enabled":true,"count":2}\n');
  assert.equal(previewContent(bytes).text, '{"enabled":true,"count":2}\n');
  assert.equal(previewContent(bytes, { pretty: true }).text, '{\n  "enabled": true,\n  "count": 2\n}');
  const large = '{"id":9007199254740993}';
  assert.equal(previewContent(encode(large), { pretty: true }).text, large);
  for (const original of ['{"value":0.123456789012345678901}', '{"value":-0}', '{"same":1,"same":2}']) {
    assert.equal(previewContent(encode(original), { pretty: true }).text, original, "preview must not silently normalize numeric evidence");
  }
  assert.equal(previewContent(encode('{"id":'), { pretty: true, truncated: true }).json, false);
});

test("binary and invalid UTF-8 use hex and ASCII rather than replacement characters", () => {
  const view = previewContent(new Uint8Array([65, 0, 255, 60, 62]));
  assert.equal(view.format, "hex");
  assert.match(view.text, /^00000000  41 00 ff 3c 3e/);
  assert.match(view.text, /\|A\.\.<>\|$/);
  assert.equal(previewContent(new Uint8Array([0xff])).format, "hex");
});

test("a truncated UTF-8 suffix does not turn the whole text into binary", () => {
  const bytes = encode("hello 中文").slice(0, 8);
  const view = previewContent(bytes, { truncated: true });
  assert.equal(view.format, "text");
  assert.equal(view.text, "hello ");
  assert.equal(view.incompleteUtf8, true);
});
