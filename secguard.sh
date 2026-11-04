#!/usr/bin/env bash
# Single-file Linux PoC. Python uses -c so stdin remains available to the target.
set -euo pipefail

if [[ $(uname -s) != Linux ]]; then
    echo "Linux is required." >&2
    exit 1
fi
if [[ $EUID -eq 0 ]]; then
    echo "Run as a normal user, without sudo." >&2
    exit 1
fi
for secguard_tool in python3 capsh unshare ip iptables; do
    command -v "$secguard_tool" >/dev/null || {
        echo "Missing command: $secguard_tool" >&2
        exit 1
    }
done

IFS= read -r -d '' _SECGUARD_PYTHON <<'SECGUARD_PYTHON' || :
"""IPv4 TCP/UDP outbound relay PoC, not a filesystem or hostile-code sandbox.

Both modes transparently forward TCP/UDP. Redirect mode also logs five-tuples.
TCP/UDP receiver and UDP reply FDs cross an AF_UNIX socketpair; they retain
the inner netns. Ordinary outgoing TCP/UDP sockets use the outer netns.
DNS uses the same transport paths, including outer loopback resolver addresses.
All destinations, including loopback, refer to the outer network. IPv6,
multicast/broadcast, unsolicited inbound traffic and ICMP forwarding are not implemented.
The mount namespace inherits /proc; process listings still reflect the parent PID namespace.
"""

import argparse
import array
import contextlib
import ctypes
import errno
import json
import os
import selectors
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from collections import OrderedDict

# Keep a copy for the namespace worker, but do not pass this env to the target.
SOURCE = os.environ.pop("_SECGUARD_PYTHON", "")

# Linux constants are not exposed by every Python build.
IP_TRANSPARENT = 19
IP_RECVORIGDSTADDR = 20
SO_MARK = 36
UDP_PORT = 15002
UDP_MAX_DATA = 65507
BYPASS_MARK = 2


def parse_original_destination(raw):
    if len(raw) != 16 or struct.unpack_from("=H", raw)[0] != socket.AF_INET:
        raise ValueError("expected an IPv4 sockaddr_in")
    return socket.inet_ntoa(raw[4:8]), struct.unpack_from("!H", raw, 2)[0]


def send_sockets(control, sockets):
    fds = array.array("i", [sock.fileno() for sock in sockets])
    control.sendmsg([b"S"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])


def receive_sockets(control):
    fds = array.array("i")
    data, ancillary, flags, _ = control.recvmsg(1, socket.CMSG_SPACE(3 * fds.itemsize))
    for level, kind, value in ancillary:
        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
            fds.frombytes(value[:len(value) - len(value) % fds.itemsize])
    if data != b"S" or len(fds) != 3 or flags & socket.MSG_CTRUNC:
        for fd in fds:
            os.close(fd)
        raise RuntimeError("namespace setup failed before transferring the TCP/UDP sockets")
    sockets = []
    try:
        for fd in fds:
            sock = socket.socket(fileno=fd)
            sockets.append(sock)
            sock.set_inheritable(False)
        return sockets
    except Exception:
        for sock in sockets:
            sock.close()
        for fd in fds[len(sockets):]:
            os.close(fd)
        raise


def log_flow(stream, protocol, source, destination):
    if stream is not None:
        # The original sandbox tuple, not the outer outgoing socket tuple.
        stream.write(json.dumps({
            "protocol": protocol, "src_ip": source[0], "src_port": source[1],
            "dst_ip": destination[0], "dst_port": destination[1],
        }) + "\n")
        stream.flush()


def receive_udp(listener):
    payload, ancillary, flags, source = listener.recvmsg(UDP_MAX_DATA, socket.CMSG_SPACE(16))
    if flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC):
        raise RuntimeError("truncated UDP datagram or original-destination metadata")
    for level, kind, value in ancillary:
        if level == socket.IPPROTO_IP and kind == IP_RECVORIGDSTADDR:
            return (source, parse_original_destination(value)), payload
    raise RuntimeError("UDP original destination unavailable; IP_RECVORIGDSTADDR is required")


def udp_reply_packet(key, payload):
    """Build a UDP reply for the inner raw socket; Linux fills the IP checksum.

    A raw reply avoids binding the remote endpoint's UDP port inside the
    namespace: that port may already belong to the application itself.
    """
    if len(payload) > UDP_MAX_DATA:
        raise ValueError("IPv4 UDP payload exceeds 65507 bytes")
    client, remote = key
    source, destination = socket.inet_aton(remote[0]), socket.inet_aton(client[0])
    length = 8 + len(payload)
    udp = struct.pack("!HHHH", remote[1], client[1], length, 0)
    checked = source + destination + struct.pack("!BBH", 0, socket.IPPROTO_UDP, length) + udp + payload
    words = array.array("H", checked + (b"\0" if len(checked) % 2 else b""))
    if sys.byteorder == "little":
        words.byteswap()
    total = sum(words)
    while total >> 16:
        total = (total & 0xffff) + (total >> 16)
    checksum = (~total & 0xffff) or 0xffff
    udp = udp[:6] + struct.pack("!H", checksum)
    ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + length, 0, 0, 64,
                     socket.IPPROTO_UDP, 0, source, destination)
    return ip + udp + payload


def check_udp_path(listener, reply):
    """Exercise capture, original destination and reply routing before exec."""
    destination = ("192.0.2.1", 65534)
    payload = b"secguard-udp-path-check"
    listener.settimeout(2)
    reply.settimeout(2)
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.settimeout(2)
            probe.bind(("127.0.0.1", 0))
            probe.sendto(payload, destination)
            key, captured = receive_udp(listener)
            if key != (probe.getsockname(), destination) or captured != payload:
                raise RuntimeError("UDP capture changed the original tuple or payload")
            reply.sendto(udp_reply_packet(key, captured), (key[0][0], 0))
            if probe.recvfrom(UDP_MAX_DATA) != (payload, destination):
                raise RuntimeError("UDP reply did not preserve the original sender")
    except (OSError, ValueError, RuntimeError) as error:
        raise RuntimeError(f"UDP transparent path self-check failed: {error}") from error
    finally:
        listener.settimeout(None)
        reply.settimeout(None)


class UdpRelay:
    """One outer UDP socket per application source endpoint, with bounded lifetime."""

    def __init__(self, on_reply, listener=None, log_stream=None, idle_timeout=120, max_flows=1024):
        self.on_reply = on_reply
        self.log_stream = log_stream
        self.idle_timeout = idle_timeout
        self.max_flows = max_flows
        self.flows = OrderedDict()
        self.selector = selectors.DefaultSelector()
        if listener is not None:
            listener.setblocking(False)
            self.selector.register(listener, selectors.EVENT_READ, None)

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        for key in list(self.flows):
            self.drop(key)
        self.selector.close()

    def drop(self, key):
        sock, _ = self.flows.pop(key)
        self.selector.unregister(sock)
        sock.close()

    def expire(self):
        cutoff = time.monotonic() - self.idle_timeout
        while self.flows:
            key = next(iter(self.flows))
            if self.flows[key][1] > cutoff:
                break
            self.drop(key)

    def touch(self, key):
        sock, _ = self.flows[key]
        self.flows[key] = (sock, time.monotonic())
        self.flows.move_to_end(key)

    def send_datagram(self, key, payload):
        if len(payload) > UDP_MAX_DATA:
            raise ValueError("IPv4 UDP payload exceeds 65507 bytes")
        self.expire()
        source, destination = key
        if source not in self.flows:
            if len(self.flows) >= self.max_flows:
                raise OSError(errno.ENOBUFS, "UDP session limit reached; datagram dropped")
            upstream = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                upstream.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 262144)
                upstream.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 262144)
                upstream.setblocking(False)
                # Keep one source port across destinations and allow replies
                # from a different remote port (for example a TFTP transfer).
                upstream.bind(("0.0.0.0", 0))
                self.selector.register(upstream, selectors.EVENT_READ, source)
            except Exception:
                upstream.close()
                raise
            self.flows[source] = (upstream, time.monotonic())
        sock, _ = self.flows[source]
        sock.sendto(payload, destination)  # A zero-byte UDP datagram is valid.
        self.touch(source)
        log_flow(self.log_stream, "UDP", source, destination)

    def poll(self, timeout=0.1):
        self.expire()
        for event, _ in self.selector.select(timeout):
            key = event.data
            if key is None:
                try:
                    key, payload = receive_udp(event.fileobj)
                except BlockingIOError:
                    continue
                try:
                    self.send_datagram(key, payload)
                except OSError as error:
                    print(f"secguard: UDP {key}: {error}", file=sys.stderr, flush=True)
            else:
                # An earlier listener event can expire a socket from this
                # readiness batch, or replace a flow with a new socket.
                current = self.flows.get(key)
                if current is None or current[0] is not event.fileobj:
                    continue
                try:
                    payload, _, flags, remote = event.fileobj.recvmsg(UDP_MAX_DATA)
                except BlockingIOError:
                    continue
                except OSError as error:
                    print(f"secguard: UDP {key}: {error}", file=sys.stderr, flush=True)
                    continue
                if flags & socket.MSG_TRUNC:
                    raise RuntimeError("upstream UDP datagram was truncated")
                self.touch(key)
                self.on_reply((key, remote), payload)


def shutdown(sock, how):
    with contextlib.suppress(OSError):
        sock.shutdown(how)


def relay(left, right):
    def pump(source, target):
        try:
            while True:
                data = source.recv(65536)
                if not data:
                    shutdown(target, socket.SHUT_WR)
                    return
                target.sendall(data)
        except OSError:
            shutdown(source, socket.SHUT_RDWR)
            shutdown(target, socket.SHUT_RDWR)

    upstream = threading.Thread(target=pump, args=(left, right), daemon=True)
    upstream.start()
    pump(right, left)
    upstream.join()


def forward_tcp(client, source, destination, log_stream=None):
    with client:
        log_flow(log_stream, "TCP", source, destination)
        try:
            with socket.create_connection(destination, timeout=10) as upstream:
                upstream.settimeout(None)
                relay(client, upstream)
        except OSError as error:
            print(f"secguard: TCP {destination}: {error}", file=sys.stderr, flush=True)


def serve_tcp(listener, log_stream):
    while True:
        try:
            client, source = listener.accept()
        except OSError:
            return
        try:
            # SO_ORIGINAL_DST is Linux option 80, evaluated in the socket's netns.
            destination = parse_original_destination(client.getsockopt(socket.IPPROTO_IP, 80, 16))
        except (OSError, ValueError) as error:
            client.close()
            print(f"secguard: original destination unavailable: {error}", file=sys.stderr, flush=True)
            continue
        threading.Thread(target=forward_tcp,
                         args=(client, source, destination, log_stream), daemon=True).start()


def run_worker(control_fd, parent_netns, lock_path, command):
    # Verify isolation before touching routes or iptables.
    if os.geteuid() != 0 or os.readlink("/proc/self/ns/net") == parent_netns:
        raise RuntimeError("refusing setup without new user/network namespaces")
    os.environ["XTABLES_LOCKFILE"] = lock_path
    for argv in (
        ["ip", "link", "set", "lo", "up", "mtu", "65536"],
        # This route reaches nat/OUTPUT. It is NOT an Internet uplink.
        ["ip", "-4", "route", "add", "default", "dev", "lo", "src", "127.0.0.1"],
        ["iptables", "-t", "nat", "-A", "OUTPUT", "-p", "tcp", "-j", "REDIRECT", "--to-ports", "15001"],
        # Locally generated UDP traverses lo/PREROUTING via policy routing.
        # Only application packets are marked 1; replies carry BYPASS_MARK.
        ["ip", "-4", "rule", "add", "priority", "100", "fwmark", "1", "lookup", "100"],
        ["ip", "-4", "route", "add", "local", "0.0.0.0/0", "dev", "lo", "table", "100"],
        ["iptables", "-t", "mangle", "-A", "OUTPUT", "-p", "udp", "-m", "mark", "!", "--mark", str(BYPASS_MARK),
         "-j", "MARK", "--set-mark", "1"],
        ["iptables", "-t", "mangle", "-A", "PREROUTING", "-p", "udp", "-m", "mark", "--mark", "1",
         "-j", "TPROXY", "--on-ip", "127.0.0.1", "--on-port", str(UDP_PORT), "--tproxy-mark", "1"],
    ):
        subprocess.run(argv, check=True)

    with socket.socket(fileno=int(control_fd)) as control:
        with contextlib.ExitStack() as sockets:
            tcp = sockets.enter_context(socket.socket())
            tcp.bind(("127.0.0.1", 15001))
            tcp.listen(128)
            udp = sockets.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
            udp.setsockopt(socket.IPPROTO_IP, IP_TRANSPARENT, 1)
            udp.setsockopt(socket.IPPROTO_IP, IP_RECVORIGDSTADDR, 1)
            udp.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 262144)
            udp.bind(("127.0.0.1", UDP_PORT))
            try:
                reply = sockets.enter_context(socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_RAW))
                reply.setsockopt(socket.SOL_SOCKET, SO_MARK, BYPASS_MARK)
            except OSError as error:
                raise RuntimeError(f"namespace-local UDP reply socket unavailable (CAP_NET_RAW/seccomp): {error}") from error
            check_udp_path(udp, reply)
            send_sockets(control, (tcp, udp, reply))
        control.settimeout(10)
        if control.recv(1) != b"R":
            raise RuntimeError("forwarder did not become ready")
    # No listener or host-side control descriptors survive into the target.
    return subprocess.run([
        "capsh", "--drop=all", "--noamb", "--caps=", "--", "-c", command,
    ]).returncode


def run(mode, command):
    # Set the kernel flag directly; children inherit it across fork and exec.
    # This avoids depending on capsh support for a no_new_privs option.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int] + [ctypes.c_ulong] * 4
    libc.prctl.restype = ctypes.c_int
    if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
        error = ctypes.get_errno()
        raise OSError(error, "cannot set no_new_privs: " + os.strerror(error))

    # A private lock file avoids needing access to /run/xtables.lock.
    with tempfile.TemporaryDirectory(prefix="secguard-") as directory:
        with contextlib.ExitStack() as stack:
            control, child_control = [stack.enter_context(sock) for sock in socket.socketpair()]
            child = None
            try:
                child = subprocess.Popen([
                    "unshare", "--user", "--map-root-user", "--net",
                    # Keep mount isolation without mounting procfs, which some containers deny.
                    # The PID namespace still provides child cleanup; /proc is inherited.
                    "--pid", "--fork", "--kill-child=SIGKILL", "--mount",
                    sys.executable, "-c", SOURCE, "--worker", str(child_control.fileno()),
                    os.readlink("/proc/self/ns/net"), directory + "/xtables.lock", command,
                ], pass_fds=(child_control.fileno(),))
                child_control.close()
                control.settimeout(10)
                tcp, udp, reply = [stack.enter_context(sock) for sock in receive_sockets(control)]
                reply.setblocking(False)
                log_stream = sys.stdout if mode == "redirect" else None
                threading.Thread(target=serve_tcp, args=(tcp, log_stream), daemon=True).start()

                def send_reply(key, payload):
                    # This FD still belongs to the inner netns. No outer raw
                    # socket or outer CAP_NET_RAW is used or acquired.
                    try:
                        reply.sendto(udp_reply_packet(key, payload), (key[0][0], 0))
                    except BlockingIOError:
                        print(f"secguard: UDP {key}: reply buffer full; datagram dropped", file=sys.stderr, flush=True)

                with UdpRelay(send_reply, listener=udp, log_stream=log_stream) as relay_udp:
                    control.sendall(b"R")
                    control.close()
                    while child.poll() is None:
                        relay_udp.poll()
                    return child.wait()
            finally:
                if child is not None:
                    if child.poll() is None:
                        # Kill this launcher's unshare process; --kill-child then
                        # kills namespace PID 1 and the namespace's descendants.
                        child.kill()
                    child.wait()


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="Transparent outbound IPv4 TCP/UDP relay (including DNS). Requires namespace-local TPROXY, policy routing and raw replies.")
    parser.add_argument("--mode", choices=("direct", "redirect"), default="direct",
                        help="direct: forward silently (default); redirect: also log TCP/UDP five-tuples")
    parser.add_argument("-c", dest="command", required=True, help="command to run as namespace root without capabilities")
    return parser.parse_args(argv)


def main():
    if sys.argv[1:2] == ["--worker"]:
        return run_worker(*sys.argv[2:])
    args = parse_args(sys.argv[1:])
    if sys.platform != "linux" or os.geteuid() == 0:
        raise RuntimeError("run secguard.sh on Linux as a normal user, without sudo")

    def stop(signum, frame):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, stop)
    return run(args.mode, args.command)


if __name__ == "__main__":
    try:
        status = main()
        sys.exit(status if status >= 0 else 128 - status)
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"secguard: {error}", file=sys.stderr, flush=True)
        sys.exit(1)
SECGUARD_PYTHON

# The host forwarder stays unprivileged; only setup in the new userns gains caps.
export _SECGUARD_PYTHON
exec capsh --noamb --caps= -- -c \
    'exec python3 -c "$_SECGUARD_PYTHON" "$@"' secguard "$@"

