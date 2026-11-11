// Inspection happens on the host. Needle values never enter docker exec arguments.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";

export async function assertSandboxSecretsAbsent(container, secrets) {
  const needles = secrets.filter(value => value.length > 0).map(value => Buffer.from(value));
  assert.ok(needles.length > 0, "the credential scan needs a host-side credential fixture");
  const containsSecret = bytes => needles.some(needle => bytes.includes(needle));
  for (const file of ["environ", "cmdline"]) {
    const bytes = execFileSync("docker", ["exec", container, "sh", "-c", `cat /proc/[0-9]*/${file} 2>/dev/null || true`], {maxBuffer:16 * 1024 * 1024, timeout:15000});
    assert.ok(!containsSecret(bytes), `Original credential found in sandbox process ${file}`);
  }
  const reader = spawn("docker", ["exec", container, "tar", "--ignore-failed-read", "-C", "/records", "-cf", "-", "."], {stdio:["ignore", "pipe", "pipe"]});
  const overlap = Math.max(...needles.map(needle => needle.length)) - 1;
  let tail = Buffer.alloc(0), leaked = false, errors = "";
  reader.stdout.on("data", chunk => {
    const bytes = Buffer.concat([tail, chunk]);
    leaked ||= containsSecret(bytes);
    tail = bytes.subarray(Math.max(0, bytes.length - overlap));
  });
  reader.stderr.on("data", chunk => errors += chunk);
  const [code] = await once(reader, "close");
  const expectedWarnings = errors.split("\n").filter(Boolean).every(line => /file changed as we read it|Exiting with failure status due to previous errors/.test(line));
  assert.ok(code === 0 || code === 1 && expectedWarnings, "Sandbox records could not be fully inspected");
  assert.ok(!leaked, "Original credential found in sandbox records");
}
