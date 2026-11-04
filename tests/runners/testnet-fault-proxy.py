#!/usr/bin/env python3
"""Controlled transport faults. Does not read keys or retain raw transactions.

Control: {"mode":"pass|hold-before-send|drop-after-send|reject-before-send|http-failure|rpc-failure|rate-limit|read-timeout",
          "release":false,"generation":0,"methods":["eth_getLogs"]}.
generation changes when hold/drop is rearmed. GET /health returns only
public counters and hashes. Tests use --local-test with a loopback upstream.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import select
import socket
import stat
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

CHAIN_ID = 10143
GENESIS = "0x298034669ee44327d2da9744b9b2782848e2f2a6959756b7b0471b09a404f5c9"
MAX_BYTES = 16 * 1024 * 1024
METHODS = {"eth_chainId", "eth_blockNumber", "eth_getBlockByNumber", "eth_getLogs", "eth_call", "eth_getCode", "eth_getBalance", "eth_getTransactionCount", "eth_gasPrice", "eth_estimateGas", "eth_sendRawTransaction", "eth_getTransactionReceipt", "eth_getTransactionByHash", "eth_getStorageAt"}
MODES = {"pass", "hold-before-send", "drop-after-send", "reject-before-send", "http-failure", "rpc-failure", "rate-limit", "read-timeout"}
READ_METHODS = METHODS - {"eth_sendRawTransaction"}
EXTRA_READ_MODES = {"rate-limit", "read-timeout"}
SDK_TIMEOUT_SECONDS = 15
READ_DELAY_SECONDS = 35


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise RuntimeError("upstream redirect forbidden")


def safe_parent(path):
    require(path.is_absolute(), "control/metrics paths must be absolute")
    require(not any((p / ".git").exists() for p in [path.resolve(), *path.resolve().parents]), "upstream repository is read-only")
    for parent in path.parent.parents:
        if parent.exists():
            require(not parent.is_symlink() and parent.is_dir(), "path parent must be a real directory")
    if path.parent.exists():
        require(not path.parent.is_symlink() and path.parent.is_dir(), "path parent must be a real directory")
    path.parent.mkdir(parents=True, exist_ok=True)


class State:
    def __init__(self, args):
        self.args = args
        self.control = Path(args.control)
        self.metrics_path = Path(args.metrics)
        safe_parent(self.control)
        safe_parent(self.metrics_path)
        self.lock = threading.RLock()
        self.send_locks = {}
        self.accepted = {}
        self.arm = None
        self.held_fingerprint = None
        self.dropped_arm = None
        self.metrics = {"ready": False, "chain_id": args.chain_id, "genesis": args.expected_genesis, "requests": 0, "method_counts": {}, "send_requests": 0, "upstream_send_attempts": 0, "deduplicated_send_responses": 0, "abandoned_held_requests": 0, "injected_failures": 0, "held_requests": 0, "held_payload_sha256": None, "last_tx_hash": None, "events": [], "rate_limit_responses": 0, "read_timeout_started": 0, "read_timeout_clients_expired": 0, "read_timeout_early_disconnects": 0, "read_timeout_windows_expired": 0, "blocked_write_requests": 0}
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def configuration(self):
        try:
            if not self.control.exists():
                return {"mode": "pass", "generation": 0, "release": False, "methods": []}
            descriptor = os.open(self.control, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(descriptor, "rb") as file:
                meta = os.fstat(file.fileno())
                require(stat.S_ISREG(meta.st_mode) and meta.st_size <= 8192, "control must be bounded regular JSON")
                config = json.loads(file.read(8193))
            require(isinstance(config, dict) and set(config) <= {"mode", "generation", "release", "methods", "delay_seconds"}, "control schema rejected")
            require(config.get("mode", "pass") in MODES and isinstance(config.get("generation", 0), int) and 0 <= config.get("generation", 0) < 1000000, "control mode/generation rejected")
            require(isinstance(config.get("release", False), bool) and isinstance(config.get("methods", []), list) and set(config.get("methods", [])) <= METHODS, "control release/methods rejected")
            require(type(config.get("delay_seconds",READ_DELAY_SECONDS)) is int and SDK_TIMEOUT_SECONDS<config.get("delay_seconds",READ_DELAY_SECONDS)<=60,"bounded read delay must exceed SDK timeout")
            if config.get("mode") in EXTRA_READ_MODES:
                require(config.get("methods") and set(config["methods"]) <= READ_METHODS,"extra faults require explicit read-only methods")
            return {"mode": config.get("mode", "pass"), "generation": config.get("generation", 0), "release": config.get("release", False), "methods": config.get("methods", []), "delay_seconds":config.get("delay_seconds",READ_DELAY_SECONDS)}
        except (OSError, ValueError, RuntimeError, TypeError):
            # Invalid operator control blocks traffic instead of silently allowing sends.
            return {"mode": "http-failure", "generation": 0, "release": False, "methods": []}

    def write_metrics(self):
        with self.lock:
            self.metrics["updated_unix"] = time.time()
            data = json.dumps(self.metrics, sort_keys=True, indent=2).encode() + b"\n"
            path = self.metrics_path
            if path.exists():
                require(path.is_file() and not path.is_symlink(), "metrics path must be regular")
            temporary = path.parent / ("." + path.name + "." + str(os.getpid()) + "." + str(time.monotonic_ns()))
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(descriptor, "wb") as file:
                file.write(data)
                file.flush()
                os.fsync(file.fileno())
            os.replace(temporary, path)

    def event(self, name, fingerprint=None, tx_hash=None, **facts):
        with self.lock:
            event = {"kind": name, "unix": time.time()}
            if fingerprint:
                event["payload_sha256"] = fingerprint
            if tx_hash:
                event["tx_hash"] = tx_hash
            require(set(facts)<={"method","elapsed_ms","delay_seconds","client_timeout_seconds","http_status","retry_after_seconds"},"only public fault facts permitted")
            event.update(facts)
            self.metrics["events"] = (self.metrics["events"] + [event])[-100:]
            self.write_metrics()

    def upstream(self, body):
        request = urllib.request.Request(self.args.upstream, data=body, headers={"Content-Type": "application/json"})
        try:
            with self.opener.open(request, timeout=20) as response:
                require(response.status == 200, "upstream HTTP rejected")
                data = response.read(MAX_BYTES + 1)
                require(len(data) <= MAX_BYTES, "upstream response exceeds bound")
                return data
        except (OSError, urllib.error.URLError, RuntimeError) as error:
            raise RuntimeError("upstream transport unavailable") from None

    def identity(self):
        def query(method, params):
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
            value = json.loads(self.upstream(body))
            require(value.get("id") == 1 and "error" not in value, "upstream identity response rejected")
            return value["result"]
        require(int(query("eth_chainId", []), 16) == self.args.chain_id, "upstream chain mismatch")
        require(query("eth_getBlockByNumber", ["0x0", False])["hash"].lower() == self.args.expected_genesis.lower(), "upstream genesis mismatch")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def handle(self):
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError, TimeoutError):
            pass
    def log_message(self, *unused):
        pass
    @property
    def state(self):
        return self.server.state
    def respond(self, status, data, headers=None):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for name,value in (headers or {}).items():
            self.send_header(name,value)
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass
    def disconnected(self):
        try:
            ready, _, _ = select.select([self.connection], [], [], 0)
            return bool(ready) and self.connection.recv(1, socket.MSG_PEEK) == b""
        except (OSError, ValueError):
            return True
    def current_send_control(self, fingerprint):
        control = self.state.configuration()
        if control["mode"] in EXTRA_READ_MODES:
            with self.state.lock:
                self.state.metrics["blocked_write_requests"] += 1
            self.state.event("released-request-blocked-by-read-only-control", fingerprint)
            self.respond(503, b'{}')
            return None
        if control["mode"] in ("http-failure", "rpc-failure", "reject-before-send") and (not control["methods"] or "eth_sendRawTransaction" in control["methods"]):
            self.state.event("released-request-blocked-by-current-control", fingerprint)
            self.respond(503, b'{}')
            return None
        if control["mode"] == "hold-before-send" and not control["release"]:
            self.state.event("request-rearmed-no-forward", fingerprint)
            self.respond(503, b'{}')
            return None
        return control
    def do_GET(self):
        if self.path not in ("/health", "/metrics"):
            self.respond(404, b'{}')
            return
        with self.state.lock:
            self.respond(200, json.dumps(self.state.metrics).encode())
    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
            require(0 < length <= MAX_BYTES, "request size rejected")
            body = self.rfile.read(length)
            require(len(body) == length, "request truncated")
            request = json.loads(body)
            require(isinstance(request, dict) and request.get("jsonrpc") == "2.0" and request.get("method") in METHODS and isinstance(request.get("params"), list), "RPC method/schema rejected")
            method = request["method"]
            received_at = time.monotonic()
            with self.state.lock:
                self.state.metrics["requests"] += 1
                self.state.metrics["method_counts"][method] = self.state.metrics["method_counts"].get(method, 0) + 1
                self.state.write_metrics()
            control = self.state.configuration()
            failure_applies = not control["methods"] or method in control["methods"]
            if control["mode"] in EXTRA_READ_MODES and method=="eth_sendRawTransaction":
                with self.state.lock:
                    self.state.metrics["blocked_write_requests"]+=1
                self.state.event("read-only-mode-write-rejected")
                self.respond(503,b'{}')
                return
            if control["mode"]=="rate-limit" and failure_applies:
                with self.state.lock:
                    self.state.metrics["injected_failures"]+=1
                    self.state.metrics["rate_limit_responses"]+=1
                self.state.event("injected-http429",method=method,http_status=429,retry_after_seconds=1)
                self.respond(429,b'{}',headers={"Retry-After":"1"})
                return
            if control["mode"]=="read-timeout" and failure_applies:
                with self.state.lock:
                    self.state.metrics["injected_failures"]+=1
                    self.state.metrics["read_timeout_started"]+=1
                delay=control["delay_seconds"]
                self.state.event("read-timeout-started-no-response",method=method,delay_seconds=delay,client_timeout_seconds=SDK_TIMEOUT_SECONDS)
                # Metrics/control I/O also consumes the client request deadline.
                started=received_at
                # No headers/body and no upstream forwarding. Switching control cannot
                # silently release the stalled request before actual client expiry.
                while True:
                    elapsed_ms=int((time.monotonic()-started)*1000)
                    if self.disconnected():
                        expired=elapsed_ms>=SDK_TIMEOUT_SECONDS*1000-100
                        counter="read_timeout_clients_expired" if expired else "read_timeout_early_disconnects"
                        with self.state.lock:
                            self.state.metrics[counter]+=1
                        self.state.event("read-timeout-client-expired" if expired else "read-timeout-client-disconnected-early",method=method,elapsed_ms=elapsed_ms,delay_seconds=delay,client_timeout_seconds=SDK_TIMEOUT_SECONDS)
                        self.close_connection=True
                        return
                    if elapsed_ms>=delay*1000:
                        with self.state.lock:
                            self.state.metrics["read_timeout_windows_expired"]+=1
                        self.state.event("read-timeout-window-expired-no-response",method=method,elapsed_ms=elapsed_ms,delay_seconds=delay,client_timeout_seconds=SDK_TIMEOUT_SECONDS)
                        self.close_connection=True
                        try:
                            self.connection.shutdown(socket.SHUT_RDWR)
                        except OSError:
                            pass
                        return
                    time.sleep(0.05)
            if control["mode"] in ("http-failure", "rpc-failure") and failure_applies:
                with self.state.lock:
                    self.state.metrics["injected_failures"] += 1
                self.state.event("injected-" + control["mode"])
                if control["mode"] == "http-failure":
                    self.respond(503, b'{}')
                else:
                    self.respond(200, json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "error": {"code": -32005, "message": "injected testnet transport failure"}}).encode())
                return
            if method != "eth_sendRawTransaction":
                self.respond(200, self.state.upstream(body))
                return
            require(len(request["params"]) == 1 and isinstance(request["params"][0], str) and request["params"][0].startswith("0x"), "send payload rejected")
            raw = bytes.fromhex(request["params"][0][2:])
            require(raw and len(raw) <= MAX_BYTES // 2, "raw payload bounded")
            fingerprint = hashlib.sha256(raw).hexdigest()
            with self.state.lock:
                require(len(self.state.send_locks) < 128 or fingerprint in self.state.send_locks, "send cache capacity reached")
                lock = self.state.send_locks.setdefault(fingerprint, threading.Lock())
                self.state.metrics["send_requests"] += 1
            self.state.event("send-observed", fingerprint)
            with lock:
                if fingerprint in self.state.accepted:
                    with self.state.lock:
                        self.state.metrics["deduplicated_send_responses"] += 1
                    self.state.event("send-deduplicated", fingerprint)
                    self.respond(200, json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "result": self.state.accepted[fingerprint]}).encode())
                    return
                control = self.state.configuration()
                arm = (control["mode"], control["generation"])
                if control["mode"] == "reject-before-send":
                    self.state.event("send-rejected-before-upstream", fingerprint)
                    self.respond(503, b'{}')
                    return
                if control["mode"] == "hold-before-send" and not control["release"]:
                    with self.state.lock:
                        if self.state.arm != arm:
                            self.state.arm = arm
                            self.state.held_fingerprint = None
                        if self.state.held_fingerprint is None:
                            self.state.held_fingerprint = fingerprint
                        require(self.state.held_fingerprint == fingerprint, "different payload blocked while held")
                        self.state.metrics["held_requests"] += 1
                        self.state.metrics["held_payload_sha256"] = fingerprint
                    self.state.event("send-held-before-upstream", fingerprint)
                    started = time.monotonic()
                    try:
                        while True:
                            if self.disconnected():
                                with self.state.lock:
                                    self.state.metrics["abandoned_held_requests"] += 1
                                self.state.event("held-client-disconnected-no-forward", fingerprint)
                                return
                            current = self.state.configuration()
                            if current["mode"] != "hold-before-send" or current["generation"] != control["generation"] or current["release"]:
                                break
                            if time.monotonic() - started >= self.state.args.max_hold_seconds:
                                self.state.event("hold-timeout-no-forward", fingerprint)
                                self.respond(503, b'{}')
                                return
                            time.sleep(0.05)
                    finally:
                        with self.state.lock:
                            self.state.metrics["held_requests"] -= 1
                            self.state.write_metrics()
                # Even a held request revalidates the test network immediately before send.
                control = self.current_send_control(fingerprint)
                if control is None:
                    return
                self.state.identity()
                # Identity RPC reads may be slow. Recheck after them, immediately
                # before forwarding a signed send, including previously held requests.
                control = self.current_send_control(fingerprint)
                if control is None:
                    return
                arm = (control["mode"], control["generation"])
                with self.state.lock:
                    self.state.metrics["upstream_send_attempts"] += 1
                response = self.state.upstream(body)
                value = json.loads(response)
                result = value.get("result")
                if isinstance(result, str) and result.startswith("0x") and len(result) == 66 and "error" not in value:
                    with self.state.lock:
                        self.state.accepted[fingerprint] = result
                        self.state.metrics["last_tx_hash"] = result
                    self.state.event("send-accepted-upstream", fingerprint, result)
                    if control["mode"] == "drop-after-send" and self.state.dropped_arm != arm:
                        self.state.dropped_arm = arm
                        self.state.event("accepted-response-dropped", fingerprint, result)
                        self.respond(503, b'{}')
                        return
                self.respond(200, response)
        except (OSError, ValueError, RuntimeError, TypeError, KeyError):
            self.state.event("request-rejected-or-upstream-unavailable")
            self.respond(503, b'{}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--upstream", default="https://testnet-rpc.monad.xyz")
    parser.add_argument("--listen", default="127.0.0.1", choices=["127.0.0.1"])
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--control", required=True)
    parser.add_argument("--metrics", required=True)
    parser.add_argument("--local-test", action="store_true")
    parser.add_argument("--chain-id", type=int, default=CHAIN_ID)
    parser.add_argument("--expected-genesis", default=GENESIS)
    parser.add_argument("--max-hold-seconds", type=int, default=300)
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.upstream)
    require(not url.username and not url.password and not url.query and not url.fragment, "upstream credentials/query forbidden")
    if args.local_test:
        require(url.scheme == "http" and url.hostname == "127.0.0.1" and args.chain_id == 31337, "local-test requires isolated31337loopback")
    else:
        require(url.scheme == "https" and url.hostname and args.chain_id == CHAIN_ID and args.expected_genesis.lower() == GENESIS, "only pinned Monad Testnet upstream permitted")
    require(0 < args.port < 65536 and 1 <= args.max_hold_seconds <= 1800, "port/hold bounds rejected")
    require(args.metrics != args.control, "control and metrics paths must differ")
    state = State(args)
    state.identity()
    server = http.server.ThreadingHTTPServer((args.listen, args.port), Handler)
    server.daemon_threads = True
    server.state = state
    state.metrics["ready"] = True
    state.write_metrics()
    print(json.dumps({"ready": True, "listen": args.listen, "port": args.port, "chain_id": args.chain_id, "genesis": args.expected_genesis}), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        state.metrics["ready"] = False
        state.write_metrics()


if __name__ == "__main__":
    main()
