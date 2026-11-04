#!/usr/bin/env python3
"""Run portable tests with python3 tests/test_secguard.py -v.

Add --integration on Linux to exercise the actual unprivileged launcher.
Only the standard library is required.
"""

import contextlib
import ctypes
import errno
import io
import json
import os
from pathlib import Path
import queue
import selectors
import shlex
import signal
import socket
import socketserver
import struct
import subprocess
import sys
import threading
import time
import types
import unittest
from unittest.mock import Mock, patch


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "secguard.sh"
INTEGRATION = "--integration" in sys.argv
if INTEGRATION:
    sys.argv.remove("--integration")


def load_script():
    text = SCRIPT.read_text()
    source = text.split("<<'SECGUARD_PYTHON' || :\n", 1)[1].split(
        "\nSECGUARD_PYTHON\n", 1
    )[0]
    module = types.ModuleType("secguard_under_test")
    exec(compile(source, str(SCRIPT), "exec"), module.__dict__)
    return module


secguard = load_script()


@contextlib.contextmanager
def udp_server(host="127.0.0.1", handler=None):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 262144)
        server.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 262144)
        server.bind((host, 0))
        server.settimeout(0.05)
        stop = threading.Event()

        def serve():
            while not stop.is_set():
                try:
                    data, source = server.recvfrom(65535)
                except socket.timeout:
                    continue
                if handler is not None:
                    server.sendto(handler(data), source)
                elif data == b"multiple-replies":
                    server.sendto(b"first", source)
                    server.sendto(b"second", source)
                elif data == b"source-port":
                    server.sendto(str(source[1]).encode(), source)
                else:
                    server.sendto(data, source)

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            yield server.getsockname()
        finally:
            stop.set()
            thread.join(2)


@contextlib.contextmanager
def tcp_server(handler, host="127.0.0.1", port=0):
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.settimeout(5)
            handler(self.request)

    with socketserver.ThreadingTCPServer((host, port), Handler) as server:
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        thread.start()
        try:
            yield server.server_address
        finally:
            server.shutdown()
            thread.join(2)


def tcp_echo_after_eof(connection):
    chunks = []
    while True:
        chunk = connection.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    connection.sendall(b"reply:" + b"".join(chunks))


DNS_QUESTION = b"\x05probe\x07invalid\0" + struct.pack("!HH", 1, 1)
DNS_QUERY = struct.pack("!HHHHHH", 0x1234, 0x0100, 1, 0, 0, 0) + DNS_QUESTION
DNS_REPLY = (struct.pack("!HHHHHH", 0x1234, 0x8180, 1, 1, 0, 0) + DNS_QUESTION
             + b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + socket.inet_aton("192.0.2.42"))
DNS_TRUNCATED = struct.pack("!HHHHHH", 0x1234, 0x8380, 1, 0, 0, 0) + DNS_QUESTION


def read_exact(connection, length):
    result = b""
    while len(result) < length:
        chunk = connection.recv(length - len(result))
        if not chunk:
            raise EOFError("short DNS/TCP frame")
        result += chunk
    return result


def dns_tcp_reply(connection):
    length = struct.unpack("!H", read_exact(connection, 2))[0]
    query = read_exact(connection, length)
    if query != DNS_QUERY:
        raise ValueError("unexpected DNS fixture query")
    connection.sendall(struct.pack("!H", len(DNS_REPLY)) + DNS_REPLY)


@contextlib.contextmanager
def outer_udp_relay(**kwargs):
    replies = queue.Queue()
    with secguard.UdpRelay(lambda key, payload: replies.put((key, payload)), **kwargs) as relay:
        def receive():
            deadline = time.monotonic() + 2
            while replies.empty() and time.monotonic() < deadline:
                relay.poll(0.02)
            return replies.get_nowait()

        yield relay, receive


class UdpRelayTests(unittest.TestCase):
    def exchange(self, client, key, payload):
        relay, receive = client
        relay.send_datagram(key, payload)
        reply_key, reply = receive()
        self.assertEqual(reply_key, key)
        return reply

    def test_one_source_multiple_destinations_and_reused_upstream(self):
        log = io.StringIO()
        with udp_server() as first, udp_server() as second, outer_udp_relay(log_stream=log) as client:
            relay, receive = client
            source = ("127.0.0.1", 41000)
            keys = [(source, first), (source, second)]
            for index, key in enumerate(keys):
                relay.send_datagram(key, bytes([index, 0, 255]))
            replies = dict(receive() for _ in keys)
            self.assertEqual(replies, {key: bytes([index, 0, 255]) for index, key in enumerate(keys)})
            before = self.exchange(client, keys[0], b"source-port")
            after = self.exchange(client, keys[0], b"source-port")
            self.assertEqual(before, after)
        events = [json.loads(line) for line in log.getvalue().splitlines()]
        self.assertEqual(len(events), 4)
        self.assertEqual({event["dst_port"] for event in events}, {first[1], second[1]})
        self.assertTrue(all(event["protocol"] == "UDP" for event in events))

    def test_zero_length_maximum_size_and_multiple_replies(self):
        with udp_server() as destination, outer_udp_relay() as client:
            relay, receive = client
            key = (("127.0.0.1", 41001), destination)
            for payload in (b"", bytes(range(256)) * 255 + b"x" * 227, b"after-large"):
                self.assertEqual(self.exchange(client, key, payload), payload)
            relay.send_datagram(key, b"multiple-replies")
            replies = [receive() for _ in range(2)]
            self.assertEqual(replies, [(key, b"first"), (key, b"second")])

    def test_dns_wire_request_and_response_pass_unchanged(self):
        seen = []

        def answer(query):
            seen.append(query)
            return DNS_REPLY

        with udp_server(handler=answer) as destination, outer_udp_relay() as client:
            key = (("127.0.0.1", 41008), destination)
            self.assertEqual(self.exchange(client, key, DNS_QUERY), DNS_REPLY)
        self.assertEqual(seen, [DNS_QUERY])

    def test_server_can_change_reply_port_without_changing_client_mapping(self):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as first, \
             socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as transfer, outer_udp_relay() as client:
            for sock in (first, transfer):
                sock.bind(("127.0.0.1", 0))
                sock.settimeout(2)
            errors = []

            def serve():
                try:
                    _, source = first.recvfrom(65535)
                    transfer.sendto(b"challenge", source)
                    acknowledgment, actual_source = transfer.recvfrom(65535)
                    transfer.sendto(b"accepted" if acknowledgment == b"ack" and actual_source == source else b"wrong-mapping",
                                    actual_source)
                except Exception as error:
                    errors.append(error)

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            try:
                relay, receive = client
                source = ("127.0.0.1", 41009)
                relay.send_datagram((source, first.getsockname()), b"start")
                self.assertEqual(receive(), ((source, transfer.getsockname()), b"challenge"))
                self.assertEqual(self.exchange(client, (source, transfer.getsockname()), b"ack"), b"accepted")
            finally:
                thread.join(3)
            self.assertEqual(errors, [])

    def test_capacity_does_not_evict_active_flow(self):
        with udp_server() as destination, outer_udp_relay(max_flows=1) as client:
            relay, _ = client
            first = (("127.0.0.1", 41002), destination)
            second = (("127.0.0.1", 41003), destination)
            self.assertEqual(self.exchange(client, first, b"initial"), b"initial")
            with self.assertRaises(OSError) as raised:
                relay.send_datagram(second, b"over-limit")
            self.assertEqual(raised.exception.errno, errno.ENOBUFS)
            self.assertEqual(self.exchange(client, first, b"still-live"), b"still-live")
            self.assertEqual(len(relay.flows), 1)

    def test_idle_flow_is_expired(self):
        with udp_server() as destination, outer_udp_relay(idle_timeout=0.06) as client:
            relay, _ = client
            key = (("127.0.0.1", 41004), destination)
            self.assertEqual(self.exchange(client, key, b"first"), b"first")
            deadline = time.monotonic() + 1
            while relay.flows and time.monotonic() < deadline:
                relay.poll(0.02)
            self.assertEqual(len(relay.flows), 0)
            self.assertEqual(self.exchange(client, key, b"second"), b"second")

    def test_reply_packet_addresses_ports_length_and_checksum(self):
        key = (("127.0.0.1", 41005), ("192.0.2.1", 53))
        for payload in (b"", b"odd", b"even", b"x" * 65507):
            packet = secguard.udp_reply_packet(key, payload)
            self.assertEqual(packet[12:20], socket.inet_aton(key[1][0]) + socket.inet_aton(key[0][0]))
            src, dst, length, checksum = struct.unpack("!HHHH", packet[20:28])
            self.assertEqual((src, dst, length), (53, 41005, len(payload) + 8))
            self.assertNotEqual(checksum, 0)
            self.assertEqual(packet[28:], payload)
            pseudo = packet[12:20] + struct.pack("!BBH", 0, 17, length)
            data = pseudo + packet[20:]
            if len(data) % 2:
                data += b"\0"
            total = sum(struct.unpack("!%dH" % (len(data) // 2), data))
            while total >> 16:
                total = (total & 65535) + (total >> 16)
            self.assertEqual(total, 65535)
        with self.assertRaises(ValueError):
            secguard.udp_reply_packet(key, b"x" * 65508)

    def test_readiness_for_expired_socket_is_ignored(self):
        # A captured readiness batch can contain a socket that was expired
        # while an earlier event in that same batch was being processed.
        with udp_server() as destination, outer_udp_relay() as client:
            relay, _ = client
            key = (("127.0.0.1", 41006), destination)
            self.exchange(client, key, b"before-expiry")
            sock = relay.flows[key[0]][0]
            event = selectors.SelectorKey(sock, sock.fileno(), selectors.EVENT_READ, key[0])
            relay.drop(key[0])
            with patch.object(relay.selector, "select", return_value=[(event, selectors.EVENT_READ)]):
                relay.poll(0)
            self.assertEqual(len(relay.flows), 0)

    def test_original_destination_required_for_every_datagram(self):
        destination = ("192.0.2.17", 5300)
        raw = struct.pack("=H", socket.AF_INET) + struct.pack("!H", destination[1])
        raw += socket.inet_aton(destination[0]) + b"\0" * 8
        source = ("127.0.0.1", 41007)
        listener = Mock()
        listener.recvmsg.return_value = (b"", [(socket.IPPROTO_IP, 20, raw)], 0, source)
        self.assertEqual(secguard.receive_udp(listener), ((source, destination), b""))
        for ancillary, flags in (([], 0), ([(socket.IPPROTO_IP, 20, raw)], socket.MSG_CTRUNC)):
            listener.recvmsg.return_value = (b"data", ancillary, flags, source)
            with self.assertRaises(RuntimeError):
                secguard.receive_udp(listener)


class DescriptorTests(unittest.TestCase):
    def test_three_sockets_remain_usable_after_sender_closes_copies(self):
        with contextlib.ExitStack() as stack:
            left, right = socket.socketpair()
            stack.enter_context(left)
            stack.enter_context(right)
            originals = [socket.socket(socket.AF_INET, socket.SOCK_DGRAM) for _ in range(3)]
            try:
                for original in originals:
                    original.bind(("127.0.0.1", 0))
                addresses = [original.getsockname() for original in originals]
                secguard.send_sockets(left, originals)
            finally:
                for original in originals:
                    original.close()
            received = [stack.enter_context(sock) for sock in secguard.receive_sockets(right)]
            for sock, address in zip(received, addresses):
                self.assertEqual(sock.getsockname(), address)
                self.assertFalse(sock.get_inheritable())
                sock.settimeout(1)
                sock.sendto(b"fd-still-live", address)
                self.assertEqual(sock.recv(64), b"fd-still-live")

    def test_partial_descriptor_transfer_is_closed_and_rejected(self):
        with contextlib.ExitStack() as stack:
            left, right = socket.socketpair()
            stack.enter_context(left)
            stack.enter_context(right)
            original = stack.enter_context(socket.socket())
            secguard.send_sockets(left, (original,))
            with patch.object(os, "close", wraps=os.close) as close:
                with self.assertRaises(RuntimeError):
                    secguard.receive_sockets(right)
                self.assertEqual(close.call_count, 1)


class ExistingBehaviorTests(unittest.TestCase):
    def test_tcp_large_payload_and_half_close(self):
        with socket.socket() as server, socket.socket() as ingress:
            server.bind(("127.0.0.1", 0))
            ingress.bind(("127.0.0.1", 0))
            server.listen()
            ingress.listen()
            payload = bytes(range(256)) * 512
            errors = []

            def serve():
                try:
                    with server.accept()[0] as upstream:
                        upstream.settimeout(3)
                        chunks = []
                        while True:
                            chunk = upstream.recv(65536)
                            if not chunk:
                                break
                            chunks.append(chunk)
                        upstream.sendall(b"reply:" + b"".join(chunks))
                except Exception as error:
                    errors.append(error)

            server_thread = threading.Thread(target=serve, daemon=True)
            server_thread.start()
            with socket.create_connection(ingress.getsockname(), timeout=3) as client:
                accepted, source = ingress.accept()
                forwarder = threading.Thread(target=secguard.forward_tcp,
                                             args=(accepted, source, server.getsockname()), daemon=True)
                forwarder.start()
                client.sendall(payload)
                client.shutdown(socket.SHUT_WR)
                chunks = []
                while True:
                    chunk = client.recv(65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
                self.assertEqual(b"".join(chunks), b"reply:" + payload)
                forwarder.join(3)
                self.assertFalse(forwarder.is_alive())
            server_thread.join(3)
            self.assertEqual(errors, [])

    def test_no_new_privs_failure_prevents_namespace_launch(self):
        libc = Mock()
        libc.prctl.return_value = -1
        with patch.object(ctypes, "CDLL", return_value=libc), \
             patch.object(ctypes, "get_errno", return_value=errno.EPERM), \
             patch.object(subprocess, "Popen") as launch:
            with self.assertRaises(OSError) as raised:
                secguard.run("direct", "true")
            self.assertEqual(raised.exception.errno, errno.EPERM)
            launch.assert_not_called()

    def test_tproxy_setup_failure_prevents_target_execution(self):
        calls = []

        def execute(argv, **kwargs):
            calls.append(argv)
            if "TPROXY" in argv:
                raise subprocess.CalledProcessError(1, argv)
            return subprocess.CompletedProcess(argv, 0)

        with patch.object(os, "geteuid", return_value=0), \
             patch.object(os, "readlink", return_value="new-netns"), \
             patch.dict(os.environ), \
             patch.object(subprocess, "run", side_effect=execute):
            with self.assertRaises(subprocess.CalledProcessError):
                secguard.run_worker("-1", "parent-netns", "/unused/lock", "touch should-not-run")
        self.assertFalse(any(argv[0] == "capsh" for argv in calls))


@unittest.skipUnless(INTEGRATION, "use --integration on a normal-user Linux target")
class LinuxLauncherTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(sys.platform, "linux", "namespace integration requires Linux")
        self.assertNotEqual(os.geteuid(), 0, "run the integration tests without sudo")

    def launch(self, code=None, command=None, mode="redirect", expected=0):
        if command is None:
            command = shlex.join([sys.executable, "-c", code])
        with subprocess.Popen(["bash", str(SCRIPT), "--mode", mode, "-c", command],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, start_new_session=True) as process:
            try:
                stdout, stderr = process.communicate("stdin preserved\n", timeout=35)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    stdout, stderr = process.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    stdout, stderr = process.communicate()
                self.fail(f"secguard timed out\nstdout:\n{stdout}\nstderr:\n{stderr}")
        self.assertEqual(process.returncode, expected, f"stdout:\n{stdout}\nstderr:\n{stderr}")
        return stdout, stderr

    def exercise_network(self, host):
        with tcp_server(tcp_echo_after_eof, host=host) as tcp, \
             udp_server("127.0.0.11") as first, udp_server(host) as second:
            code = f'''
import json, os, socket, stat, sys
from pathlib import Path
assert sys.stdin.read() == "stdin preserved\\n"
assert "_SECGUARD_PYTHON" not in os.environ
status = dict(line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines())
for name in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"):
    assert int(status[name], 16) == 0, (name, status[name])
assert int(status["NoNewPrivs"]) == 1
assert os.readlink("/proc/self/ns/net") != {os.readlink('/proc/self/ns/net')!r}
for name in os.listdir("/proc/self/fd"):
    try:
        mode = os.fstat(int(name)).st_mode
    except OSError:
        continue
    assert not stat.S_ISSOCK(mode), ("inherited socket", name)
with socket.create_connection({tcp!r}, timeout=5) as stream:
    data = bytes(range(256)) * 512
    stream.sendall(data)
    stream.shutdown(socket.SHUT_WR)
    chunks = []
    while True:
        chunk = stream.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    assert b"".join(chunks) == b"reply:" + data
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
    client.settimeout(5)
    client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 262144)
    client.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 262144)
    client.bind(("127.0.0.1", 0))
    destinations = [{first!r}, {second!r}]
    for index, destination in enumerate(destinations):
        client.sendto(bytes([index, 0, 255]), destination)
    replies = {{}}
    for _ in destinations:
        payload, remote = client.recvfrom(65535)
        replies[remote] = payload
    assert replies == {{destination: bytes([index, 0, 255]) for index, destination in enumerate(destinations)}}, replies
    for payload in (b"", bytes(range(256)) * 255 + b"x" * 227, b"second-packet"):
        client.sendto(payload, destinations[0])
        assert client.recvfrom(65535) == (payload, destinations[0])
    client.sendto(b"multiple-replies", destinations[1])
    assert {{client.recvfrom(65535), client.recvfrom(65535)}} == {{(b"first", destinations[1]), (b"second", destinations[1])}}
    client.connect(destinations[0])
    client.send(b"connected-client")
    assert client.recv(65535) == b"connected-client"
# The application's wildcard-bound local port is also the remote port.
# A second ordinary UDP socket cannot always bind that port for replies.
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as same_port:
    same_port.settimeout(5)
    same_port.bind(("0.0.0.0", {first[1]}))
    same_port.sendto(b"same-source-and-destination-port", {first!r})
    assert same_port.recvfrom(65535) == (b"same-source-and-destination-port", {first!r})
print("CHECK_OK tcp udp capabilities descriptors stdin")
'''
            stdout, _ = self.launch(code)
            self.assertIn("CHECK_OK tcp udp capabilities descriptors stdin", stdout)
            events = [json.loads(line) for line in stdout.splitlines() if line.startswith('{"protocol":')]
            self.assertEqual({event["protocol"] for event in events}, {"TCP", "UDP"})

    def test_loopback_tcp_udp_and_capabilities(self):
        self.exercise_network("127.0.0.2")

    def test_nonloopback_policy_routing(self):
        result = subprocess.run(["ip", "-j", "-4", "addr", "show", "scope", "global"],
                                check=True, capture_output=True, text=True)
        addresses = [address["local"] for link in json.loads(result.stdout)
                     for address in link.get("addr_info", []) if address.get("family") == "inet"]
        if not addresses:
            self.skipTest("outer container has no non-loopback IPv4 address to bind a fixture")
        self.exercise_network(addresses[0])

    def test_dns_udp_response_and_tcp_fallback(self):
        with udp_server("127.0.0.11", handler=lambda query: DNS_TRUNCATED) as destination, \
             tcp_server(dns_tcp_reply, host=destination[0], port=destination[1]):
            code = f'''
import socket, struct
query = {DNS_QUERY!r}
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
    udp.settimeout(5)
    udp.connect({destination!r})
    udp.send(query)
    answer = udp.recv(65535)
    assert answer == {DNS_TRUNCATED!r}
    assert struct.unpack("!H", answer[2:4])[0] & 0x0200
with socket.create_connection({destination!r}, timeout=5) as tcp:
    tcp.sendall(struct.pack("!H", len(query)) + query)
    def exact(length):
        data = b""
        while len(data) < length:
            chunk = tcp.recv(length - len(data))
            assert chunk, "short DNS/TCP response"
            data += chunk
        return data
    length = struct.unpack("!H", exact(2))[0]
    assert exact(length) == {DNS_REPLY!r}
print("CHECK_OK dns udp and tcp")
'''
            stdout, _ = self.launch(code, mode="direct")
            self.assertEqual(stdout.strip(), "CHECK_OK dns udp and tcp")

    def test_target_exit_status_and_background_child_cleanup(self):
        start = time.monotonic()
        self.launch(command="sleep 60 & exit 37", mode="direct", expected=37)
        self.assertLess(time.monotonic() - start, 15)


if __name__ == "__main__":
    if INTEGRATION and sys.platform != "linux":
        raise SystemExit("--integration requires a normal-user Linux target")
    unittest.main()
