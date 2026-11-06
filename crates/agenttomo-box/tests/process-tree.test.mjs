import test from "node:test";
import assert from "node:assert/strict";
import { LegacyProcessTree as ProcessTree } from "../src/playground/assets/legacy-process-tree.mjs";

function feed(tree) {
  let sequence = 0;
  return (kind, data) => tree.add({ sequence: ++sequence, monotonic_ns: sequence * 1e6, kind, data });
}

test("mmap summaries distinguish memory addresses, file mappings and failures", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("memory.mmap", { pid: 10, tid: 11, task_id: 1, anonymous: true, fd: -1, length: 12288, offset: 0, prot: 3, flags: 34, mapped_address: "0x20000000000001", result: 0, duration_ns: 125000 });
  event("memory.mmap", { pid: 10, tid: 11, task_id: 1, anonymous: false, fd: 4, resolved_path: "/tmp/mapped.bin", length: 4096, offset: 4096, prot: 1, flags: 2, mapped_address: "0x7f1234000000", result: 1, duration_ns: 2000000 });
  event("memory.mmap", { tid: 11, task_id: 1, anonymous: false, fd: -1, length: 4096, offset: 0, prot: 1, flags: 2, mapped_address: null, result: -9, errno: 9 });
  event("memory.mmap", { tid: 11, task_id: 1, anonymous: true, length: 4096, offset: 0, prot: 3, flags: 34, mapped_address: null, result: -512, restart: true });
  assert.match(tree.entries[0].description, /mmap.*anonymous.*12288 B.*0x20000000000001.*0\.125 ms/);
  assert.doesNotMatch(tree.entries[0].description, /FD/);
  assert.match(tree.entries[1].description, /\/tmp\/mapped\.bin.*4096 B.*offset 4096.*0x7f1234000000.*2\.000 ms/);
  assert.match(tree.entries[2].description, /FD -1.*errno 9/);
  assert.match(tree.entries[3].description, /interrupted; restart pending/);
  assert.equal(tree.byTask.get(1).pid, 10);
  assert.equal(tree.byTask.get(1).tid, 11);
});

test("thread nodes retain the real process PID; legacy records do not invent one", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { pid: 10, tid: 10, task_id: 1, argv: ["curl"] });
  event("process.clone", { pid: 10, parent_tid: 10, child_pid: 10, child_tid: 11, task_id: 1, child_task_id: 2 });
  assert.equal(tree.byTask.get(2).pid, 10);
  assert.equal(tree.byTask.get(2).tid, 11);
  event("network.connect", { pid: 10, tid: 11, task_id: 2, transport: "udp", destination: "192.0.2.1:443" });
  event("process.exit", { pid: 10, tid: 11, task_id: 2, exit_code: 0 });
  assert.equal(tree.byTask.get(2).pid, 10);
  event("file.open", { tid: 20, path: "/legacy" });
  assert.equal(tree.active.get(20).pid, null);
});

test("UDP connect and AF_UNSPEC disconnect are described as peer selection", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("network.connect", { tid: 10, transport: "udp", destination: "192.0.2.1:443", redirected: true, result: 0 });
  event("network.connect", { tid: 10, transport: "udp", disconnect: true, result: 0 });
  assert.match(tree.entries[0].description, /udp.*default peer.*192.0.2.1:443/);
  assert.match(tree.entries[1].description, /clear default peer.*AF_UNSPEC/);
  assert.doesNotMatch(tree.entries[1].description, /unavailable/);
});

test("observed HTTP/TLS names enrich only their TCP connection, including late records", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("network.connect", { pid: 10, tid: 10, task_id: 1, connection_id: 1, transport: "tcp", destination: "192.0.2.1:443", result: 0 });
  event("network.connect", { pid: 10, tid: 10, task_id: 1, connection_id: 2, transport: "tcp", destination: "192.0.2.1:443", result: 0 });
  event("process.exit", { pid: 10, tid: 10, task_id: 1, exit_code: 0 });
  event("network.protocol", { pid: 10, tid: 10, task_id: 1, connection_id: 1, destination: "192.0.2.1:443", protocol: "tls", domain: "secure.test", domain_source: "tls_sni" });
  assert.match(tree.entries[0].description, /secure.test.*192.0.2.1:443/);
  assert.doesNotMatch(tree.entries[1].description, /secure.test/);
  assert.match(tree.entries[3].description, /tls.*secure.test.*tls_sni/);
  assert.equal(tree.byTask.get(1).state, "exited");
  event("network.transfer", { pid: 10, tid: 10, task_id: 1, connection_id: 1, destination: "192.0.2.1:443", status: "closed", bytes_up: 12, bytes_down: 34 });
  assert.match(tree.entries[4].description, /secure.test.*12 B.*34 B/);
  event("network.protocol", { tid: 20, task_id: 2, connection_id: 3, protocol: "http", domain: "plain.test", domain_source: "http_host" });
  assert.equal(tree.byTask.get(2).state, "unknown");
  event("network.connect", { tid: 20, task_id: 2, connection_id: 3, transport: "tcp", destination: "192.0.2.2:80" });
  assert.match(tree.entries.at(-1).description, /plain.test.*192.0.2.2:80/);
  event("network.protocol", { tid: 30, connection_id: 4, protocol: "tls", domain: null, domain_source: "tls_sni" });
  assert.match(tree.entries.at(-1).description, /domain unavailable/);
});

test("fork builds lineage while repeated exec stays in the same node", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  tree.start(10);
  event("process.exec", { tid: 10, argv: ["/bin/bash"] });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  const child = tree.active.get(11);
  event("process.exec", { tid: 11, argv: ["/bin/sh", "-c", "exec /bin/echo hello"] });
  event("process.exec", { tid: 11, argv: ["/bin/echo", "hello"] });
  event("process.exit", { tid: 11, exit_code: 0 });
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots.length, 1);
  assert.equal(child.parentId, tree.roots[0].id);
  assert.equal(child.name, "/bin/echo");
  assert.equal(child.state, "exited");
  assert.equal(child.events.length, 3);
  assert.match(child.events[1].description, /\/bin\/sh → \/bin\/echo hello/);
});

test("a child observed first is attached when its creation event arrives", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 21, argv: ["worker"] });
  const child = tree.active.get(21);
  event("process.fork", { parent_tid: 20, child_tid: 21 });
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots[0].tid, 20);
  assert.equal(child.parentId, tree.roots[0].id);
  assert.equal(child.name, "worker");
});

test("late ready messages and unreadable exec do not invent a new root or image", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["parent"] });
  event("process.exec", { tid: 11, capture: "unreadable" });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  assert.equal(tree.active.get(11).name, null);
  event("process.exit", { tid: 10, exit_code: 0 });
  tree.start(10);
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots[0].state, "exited");
  assert.equal(tree.roots[0].root, true);
});

test("reusing an exited TID creates a separate task history", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exec", { tid: 11, argv: ["first"] });
  const first = tree.active.get(11);
  event("process.exit", { tid: 11, exit_code: 3 });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exec", { tid: 11, argv: ["second"] });
  const second = tree.active.get(11);
  assert.notEqual(first.id, second.id);
  assert.equal(first.name, "first");
  assert.equal(second.name, "second");
  assert.equal(tree.roots[0].children.size, 2);
});

test("nonleader exec follows former_tid without merging the old leader history", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["leader"] });
  const leader = tree.active.get(10);
  event("process.clone", { parent_tid: 10, child_tid: 11 });
  const thread = tree.active.get(11);
  event("process.exit", { tid: 10, exit_code: 0 });
  event("process.exec", { tid: 10, former_tid: 11, argv: ["replacement"] });
  assert.equal(tree.active.get(10), thread);
  assert.equal(tree.active.has(11), false);
  assert.equal(thread.parentId, leader.id);
  assert.equal(thread.tid, 10);
  assert.equal(leader.state, "exited");
  assert.equal(leader.name, "leader");
  event("process.exit", { tid: 10, exit_code: 2 });
  assert.equal(thread.state, "exited");
  assert.equal(leader.events.at(-1).event.data.exit_code, 0);
});

test("behaviors use their event TID; missing ownership stays unattributed", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["worker"] });
  event("file.open", { tid: 10, path: "/tmp/input", result: 3 });
  event("network.connect", { tid: 10, destination: "127.0.0.1:8080" });
  event("file.open", { path: "/tmp/unknown" });
  assert.equal(tree.active.get(10).events.length, 3);
  assert.equal(tree.roots.find(node => node.tid === null).events.length, 1);
});

test("delayed proxy records retain their task after exit and TID reuse", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  tree.start(10);
  event("process.exec", { tid: 10, task_id: 1, argv: ["parent"] });
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 2 });
  const first = tree.active.get(11);
  event("process.exec", { tid: 11, task_id: 2, argv: ["curl"] });
  event("network.connect", { tid: 11, task_id: 2, connection_id: 1, destination: "127.0.0.1:80", redirected: true, result: 0 });
  event("process.exit", { tid: 11, task_id: 2, exit_code: 0 });
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 3 });
  const second = tree.active.get(11);
  event("network.transfer", { tid: 11, task_id: 2, connection_id: 1, destination: "127.0.0.1:80", status: "scope_finished", bytes_up: 42, bytes_down: null });
  assert.equal(first.events.at(-1).event.kind, "network.transfer");
  assert.equal(first.state, "exited");
  assert.equal(tree.active.get(11), second);
  assert.notEqual(first, second);
  assert.equal(tree.nodes.size, 3);
  assert.match(first.events.at(-1).description, /42/);
});

test("proxy records can precede process records without inventing a live reused TID", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("network.upstream", { tid: 11, task_id: 2, destination: "127.0.0.1:80", status: "connected" });
  const early = tree.roots[0];
  assert.equal(early.state, "unknown");
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 2 });
  event("process.exec", { tid: 11, task_id: 2, argv: ["curl"] });
  assert.equal(tree.active.get(11), early);
  assert.equal(early.state, "running");
  assert.equal(tree.nodes.size, 2);
  assert.equal(early.parentId, tree.roots[0].id);
});

test("pruned tasks receive late proxy records without attaching to a reused TID", () => {
  const tree = new ProcessTree(2);
  const event = feed(tree);
  event("process.exec", { tid: 11, task_id: 1, argv: ["first"] });
  event("process.exit", { tid: 11, task_id: 1, exit_code: 0 });
  event("process.exec", { tid: 11, task_id: 2, argv: ["second"] });
  event("file.open", { tid: 11, task_id: 2, path: "/input", result: 3 });
  const second = tree.active.get(11);
  assert.equal(tree.nodes.size, 1);
  event("network.transfer", { tid: 11, task_id: 1, status: "closed", bytes_up: 3, bytes_down: 4 });
  assert.equal(tree.active.get(11), second);
  assert.equal(second.events.some(entry => entry.event.kind === "network.transfer"), false);
  assert.equal(tree.roots.find(node => node !== second).state, "unknown");
});

test("file and TCP behavior descriptions show the observed outcomes", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("file.open", { tid: 1, path: "/missing", syscall: "openat", result: -2, errno: 2 });
  event("network.connect", { tid: 1, destination: "example:443", redirected: true, result_source: "proxy_endpoint", result: 0 });
  event("network.upstream", { tid: 1, destination: "example:443", status: "failed", error: "Connection refused" });
  assert.match(tree.entries[0].description, /\/missing.*errno 2/);
  assert.match(tree.entries[1].description, /proxy/);
  assert.match(tree.entries[2].description, /Connection refused/);
});

test("event retention preserves necessary ancestors and reclaims finished leaves", () => {
  const tree = new ProcessTree(3);
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["root"] });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exit", { tid: 10, exit_code: 0 });
  const parent = tree.roots[0];
  event("process.exec", { tid: 11, argv: ["child"] });
  event("file.open", { tid: 11, path: "/a" });
  event("file.open", { tid: 11, path: "/b" });
  assert.equal(parent.events.length, 0);
  assert.equal(parent.omitted, 3);
  assert.equal(tree.roots[0], parent);
  event("process.exit", { tid: 11, exit_code: 0 });
  event("process.exec", { tid: 20, argv: ["other"] });
  event("file.open", { tid: 20, path: "/a" });
  event("file.open", { tid: 20, path: "/b" });
  assert.equal(tree.nodes.size, 1);
  assert.equal(tree.roots[0].tid, 20);
  assert.equal(tree.entries.length, 3);
});

test("UDP calls belong to their senders while shared proxy totals stay unattributed", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, task_id: 1, argv: ["dns-client"] });
  event("network.udp.send", { tid: 10, task_id: 1, socket_id: 7, flow_id: 1, syscall: "sendto", destination: "127.0.0.1:53", redirected: true, result: 22, bytes: 22, datagrams: 1 });
  event("network.udp.receive", { tid: 10, task_id: 1, socket_id: 7, source: "127.0.0.1:53", source_restored: true, result: 38, bytes: 38 });
  event("process.exit", { tid: 10, task_id: 1, exit_code: 0 });
  event("network.udp.transfer", { socket_id: 7, flow_id: 1, destination: "127.0.0.1:53", packets_up: 1, packets_down: 1, bytes_up: 22, bytes_down: 38 });
  event("network.udp.error", { socket_id: 7, flow_id: 2, stage: "send", error: "Network unreachable" });
  assert.match(tree.entries[1].description, /sendto.*127.0.0.1:53.*22 B/);
  assert.match(tree.entries[2].description, /127.0.0.1:53.*38 B/);
  const proxy = tree.roots.find(node => node.tid === null);
  assert.equal(proxy.events.length, 2);
  assert.match(proxy.events[0].description, /socket 7.*1 datagrams.*22 B/);
  assert.match(proxy.events[1].description, /Network unreachable/);
  assert.equal(tree.byTask.get(1).state, "exited");
  assert.equal(tree.active.has(10), false);
});

test("UDP batch descriptions show datagram counts and kernel errors", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("network.udp.send", { tid: 10, syscall: "sendmmsg", datagrams: 2, bytes: 8, result: 2, messages: [{destination: "127.0.0.1:53"}, {destination: "127.0.0.1:54"}] });
  event("network.udp.receive", { tid: 10, syscall: "recvmsg", result: -11, errno: 11 });
  assert.match(tree.entries[0].description, /2 datagrams.*8 B/);
  assert.match(tree.entries[1].description, /errno 11/);
});

test("task elapsed time spans creation to exit without resetting on exec", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["bash"] });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exec", { tid: 11, argv: ["sh"] });
  event("process.exec", { tid: 11, argv: ["sleep"] });
  const child = tree.active.get(11);
  assert.equal(child.startedNs, 2e6);
  assert.equal(child.endedNs, null);
  event("process.exit", { tid: 11, exit_code: 0 });
  assert.equal(child.endedNs - child.startedNs, 3e6);
  assert.equal(tree.active.get(10).startedNs, 1e6);
});

test("late proxy events and a reused TID cannot change a completed duration", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 11, task_id: 1, argv: ["first"] });
  event("process.exit", { tid: 11, task_id: 1, exit_code: 0 });
  const first = tree.byTask.get(1);
  event("process.exec", { tid: 11, task_id: 2, argv: ["second"] });
  event("network.transfer", { tid: 11, task_id: 1, status: "closed" });
  assert.equal(first.endedNs - first.startedNs, 1e6);
  assert.equal(tree.byTask.get(2).startedNs, 3e6);
  assert.equal(tree.byTask.get(2).endedNs, null);
});

test("elapsed bounds survive event pruning and never invent a missing start", () => {
  const tree = new ProcessTree(2);
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["worker"] });
  event("file.open", { tid: 10, path: "/a" });
  event("file.open", { tid: 10, path: "/b" });
  event("process.exit", { tid: 10, exit_code: 0 });
  const worker = tree.roots[0];
  assert.equal(worker.events.some(entry => entry.event.kind === "process.exec"), false);
  assert.equal(worker.endedNs - worker.startedNs, 3e6);
  event("file.open", { tid: 20, path: "/unknown" });
  assert.equal(tree.active.get(20).startedNs, null);
});

test("out-of-order creation and nonleader exec preserve task elapsed bounds", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["leader"] });
  event("process.exec", { tid: 11, argv: ["thread"] });
  event("process.clone", { parent_tid: 10, child_tid: 11 });
  const thread = tree.active.get(11);
  assert.equal(thread.startedNs, 2e6);
  const leader = tree.active.get(10);
  event("process.exec", { tid: 10, former_tid: 11, argv: ["replacement"] });
  assert.equal(leader.endedNs, 4e6);
  event("process.exit", { tid: 10, exit_code: 0 });
  assert.equal(thread.endedNs - thread.startedNs, 3e6);
});

test("proxy UDP records retain category and never revive an exited sender", () => {
  const tree = new ProcessTree();
  const add = feed(tree);
  add("process.exec", {pid:10, tid:10, task_id:1, argv:["worker"]});
  add("process.exit", {pid:10, tid:10, task_id:1, exit_code:0});
  const observation = {kind:"network.udp.send", source:"udp_proxy", category:"network", data:{pid:10, tid:10, task_id:1, network_backend:"netfilter_tproxy", peer:"192.0.2.1:53", bytes:29}};
  tree.add(observation);
  assert.equal(tree.byTask.get(1).state, "exited");
  assert.equal(tree.entries.at(-1).event.category, "network");
  assert.match(tree.entries.at(-1).description, /udp.*192\.0\.2\.1:53.*1 datagram.*29 B/);
  assert.doesNotMatch(tree.entries.at(-1).description, /result unavailable/);
});

test("new descendant behavior marks ancestor activity without reopening or reviving tasks", () => {
  const tree = new ProcessTree();
  const add = feed(tree);
  add("process.exec", {pid:10, tid:10, task_id:1, argv:["shell"]});
  add("process.fork", {pid:10, parent_tid:10, child_pid:11, child_tid:11, task_id:1, child_task_id:2});
  const before = tree.byTask.get(1).activity;
  add("file.open", {pid:11, tid:11, task_id:2, path:"/a", result:3});
  assert.ok(tree.byTask.get(1).activity > before);
  assert.equal(tree.byTask.get(1).activity, tree.byTask.get(2).activity);
});
