# secguard IPv4 TCP/UDP Implementation Plan

> Execute in this session without subagents, commits, new dependencies, or changes to the Rust crates. The user explicitly requested implementation and confirmed IPv4 TCP, UDP, and DNS.

**Goal:** Extend the existing single-file Linux launcher with transparent outbound IPv4 UDP and replies while retaining TCP forwarding and capability dropping.

**Architecture:** Keep the TCP REDIRECT/listener-FD path. Add namespace-local UDP TPROXY and marked policy routing. Pass three namespace-owned socket FDs (TCP receiver, UDP receiver, raw UDP reply sender) to the outer process using the existing UNIX socketpair. The outer process creates an ordinary UDP socket per application source endpoint and uses the inner reply FD to return checksummed UDP packets with each actual sender's address and port.

**Tech Stack:** Bash, Python standard library, iproute2, iptables, Linux namespaces and transparent sockets.

**Spec:** The scope and constraints below capture the approved discussion.

## Global constraints

- IPv4 outbound unicast TCP/UDP and corresponding replies; DNS uses these transports without protocol-specific production code.
- Application commands require no explicit proxy configuration and receive no capabilities or forwarding/control descriptors; no_new_privs remains enabled.
- Outer-container configuration, routes, firewall rules, and resolver files remain untouched.
- No TUN, FUSE, eBPF, new packages, or fresh procfs mount. The namespace setup process must be allowed to create a namespace-local raw reply socket; the outer process obtains no outer-network raw socket or outer capabilities.
- Preserve `--mode direct|redirect` and `-c`; redirect adds TCP/UDP tuple logs.
- Retain the existing interpretation of loopback destinations as outer-container endpoints, including Docker DNS stubs. This PoC does not isolate files or provide complete hostile-workload isolation.
- Datagram forwarding does not promise raw-IP fidelity, ICMP propagation, multicast/broadcast, unsolicited inbound connectivity, or IPv6.
- All Linux-only behavior must be distinguished from portable tests; this workspace currently runs on macOS without a Linux runtime.

## Review focus

- One client socket sending to multiple destinations must not merge flows or swap reply addresses.
- Zero-length, binary, large, repeated, and multiple-response UDP datagrams must preserve boundaries.
- Reply packets must bypass interception, including when the application occupies the remote endpoint's port. Later requests must continue reaching the UDP receiver.
- Startup failures, bridge closure, child exit, and flow expiration must release resources and fail visibly.
- TCP half-close, target exit status, no_new_privs, and empty capability sets must survive the change.

## Tasks

- [x] Add tests in `tests/test_secguard.py`: real TCP half-close and UDP relay tests, flow limits/expiry, failure handling; watch new UDP tests fail before implementation.
- [x] Extend `secguard.sh`: bounded UDP relay, TPROXY setup and return socket, outer event loop, fail-closed startup, updated help.
- [x] Add Linux integration tests: execute the actual launcher against local outer TCP/UDP fixtures; check original reply tuples, multiple destinations, maximum UDP payloads, DNS packets, capabilities, and descriptor inheritance.
- [x] Run `bash -n secguard.sh` and `python3 tests/test_secguard.py -v`; provide the Linux integration command and report the local platform limitation.
- [x] Self-review resource cleanup, routing, IPC boundaries, and compatibility; document usage and known semantics in `docs/secguard.md`.
- [ ] Run `python3 tests/test_secguard.py --integration -v` on the target Linux container; not executable on the current macOS host.

## Execution record

- Initial state: `secguard.sh` is an untracked user file; existing procfs and no_new_privs fixes are present. No unrelated files are modified.
- Linux integration is unavailable on the current macOS host. Portable tests and a runnable Linux integration suite will be provided; Linux routing success will not be claimed from mocks.
- Ruling: pass socket FDs instead of datagram messages. Socket namespace membership already provides the required boundary and reuses the existing mechanism; no additional IPC protocol is needed.
- Ruling: use a namespace-local raw sender for replies instead of binding a transparent UDP socket to each remote port. This avoids conflicts with application wildcard port bindings. Cost: the environment must allow creating that scoped raw socket. Startup fails explicitly otherwise.
- New UDP tests failed on the original script, then passed after implementation. A stale readiness event after flow expiration reproduced a crash; the regression test now passes.
- Ruling: reuse an unconnected outer UDP socket per application source endpoint, instead of a connected socket per destination. This preserves the source mapping when a server changes its reply port; a regression test failed before this change and passes after it. Active unconnected endpoints can receive arbitrary senders, consistent with UDP; no new source-policy enforcement is introduced.
- Final local verification: Bash syntax passes; 14 portable tests pass and 4 Linux integration tests are explicitly skipped. Generated integration child programs compile. No Linux routing/raw-socket success is claimed.
- Final self-review: descriptor ownership, target descriptor closure, packet addresses/checksum, bypass marks, bounded UDP lifetime, and target cleanup paths reviewed. The target Linux runtime is the remaining verification step.
