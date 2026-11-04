#!/usr/bin/env python3
"""Proxy acceptance only synthetic loopback requests; no real key or external RPC."""
import argparse
import hashlib
import http.server
import importlib.util
import io
import json
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
from types import SimpleNamespace

SOURCE=Path(__file__).resolve().parents[2]/"tests/runners/testnet-fault-proxy.py"
spec=importlib.util.spec_from_file_location("testnet_fault_proxy",SOURCE)
proxy=importlib.util.module_from_spec(spec)
spec.loader.exec_module(proxy)
GENESIS="0x"+"11"*32
TX_HASH="0x"+"22"*32


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self,*unused):
        pass
    def do_POST(self):
        request=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        method=request["method"]
        with self.server.count_lock:
            self.server.counts[method]=self.server.counts.get(method,0)+1
        results={"eth_chainId":"0x7a69","eth_getBlockByNumber":{"hash":GENESIS,"number":"0x0"},"eth_blockNumber":"0x1","eth_call":"0x","eth_sendRawTransaction":TX_HASH,"eth_getLogs":[]}
        data=json.dumps({"jsonrpc":"2.0","id":request["id"],"result":results[method]}).encode()
        self.send_response(200)
        self.send_header("Content-Length",str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Cases(unittest.TestCase):
    def setUp(self):
        self.directory=tempfile.TemporaryDirectory(prefix="bl-fault-proxy-")
        self.work=Path(self.directory.name).resolve()
        self.upstream=http.server.ThreadingHTTPServer(("127.0.0.1",0),Upstream)
        self.upstream.daemon_threads=True
        self.upstream.counts={}
        self.upstream.count_lock=threading.Lock()
        threading.Thread(target=self.upstream.serve_forever,daemon=True).start()
        args=SimpleNamespace(control=str(self.work/"control.json"),metrics=str(self.work/"metrics.json"),upstream="http://127.0.0.1:"+str(self.upstream.server_port),chain_id=31337,expected_genesis=GENESIS,max_hold_seconds=3)
        self.state=proxy.State(args)
        self.state.identity()
        self.server=http.server.ThreadingHTTPServer(("127.0.0.1",0),proxy.Handler)
        self.server.daemon_threads=True
        self.server.state=self.state
        threading.Thread(target=self.server.serve_forever,daemon=True).start()
        self.url="http://127.0.0.1:"+str(self.server.server_port)
        self.opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.configuration("pass")
    def tearDown(self):
        self.configuration("pass")
        self.server.shutdown()
        self.server.server_close()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.directory.cleanup()
    def configuration(self,mode,**extra):
        self.state.control.write_text(json.dumps({"mode":mode,**extra}))
    def query(self,method,params=(),timeout=5):
        request=urllib.request.Request(self.url,data=json.dumps({"jsonrpc":"2.0","id":7,"method":method,"params":list(params)}).encode(),headers={"Content-Type":"application/json"})
        with self.opener.open(request,timeout=timeout) as response:
            return json.load(response)
    def wait_for(self,predicate):
        until=time.monotonic()+4
        while time.monotonic()<until:
            if predicate():
                return
            time.sleep(0.02)
        self.fail("local proxy condition timeout")
    def send_socket(self,raw):
        body=json.dumps({"jsonrpc":"2.0","id":8,"method":"eth_sendRawTransaction","params":[raw]}).encode()
        sock=socket.create_connection(("127.0.0.1",self.server.server_port),timeout=5)
        sock.sendall(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: "+str(len(body)).encode()+b"\r\n\r\n"+body)
        return sock
    def test_identity_and_pass_preserve_rpc_id_without_send(self):
        self.assertEqual(self.query("eth_blockNumber"),{"jsonrpc":"2.0","id":7,"result":"0x1"})
        self.assertEqual(self.state.metrics["upstream_send_attempts"],0)
        self.state.args.expected_genesis="0x"+"00"*32
        with self.assertRaises(RuntimeError):
            self.state.identity()
    def test_selective_http_and_rpc_failures_leave_other_reads_live(self):
        self.configuration("http-failure",methods=["eth_getLogs"])
        with self.assertRaises(urllib.error.HTTPError) as failure:
            self.query("eth_getLogs")
        self.assertEqual(failure.exception.code,503)
        self.assertEqual(self.query("eth_call")["result"],"0x")
        self.configuration("rpc-failure",methods=["eth_getLogs"])
        self.assertEqual(self.query("eth_getLogs")["error"]["code"],-32005)
        self.assertEqual(self.upstream.counts.get("eth_getLogs",0),0)
    def test_reject_before_send_never_forwards_signed_payload(self):
        self.configuration("reject-before-send")
        with self.assertRaises(urllib.error.HTTPError):
            self.query("eth_sendRawTransaction",["0x0102"])
        self.assertEqual(self.upstream.counts.get("eth_sendRawTransaction",0),0)
    def test_drop_after_accept_deduplicates_same_raw_and_only_hash_is_retained(self):
        self.configuration("drop-after-send",generation=1)
        with self.assertRaises(urllib.error.HTTPError):
            self.query("eth_sendRawTransaction",["0x01020304"])
        self.assertEqual(self.query("eth_sendRawTransaction",["0x01020304"])["result"],TX_HASH)
        self.assertEqual(self.upstream.counts["eth_sendRawTransaction"],1)
        metrics=json.loads(self.state.metrics_path.read_text())
        self.assertEqual(metrics["deduplicated_send_responses"],1)
        self.assertNotIn("0x01020304",self.state.metrics_path.read_text())
        self.assertEqual(metrics["last_tx_hash"],TX_HASH)
    def test_killed_held_client_is_abandoned_then_same_raw_restart_releases_once(self):
        self.configuration("hold-before-send",release=False,generation=1)
        original=self.send_socket("0x010203")
        self.wait_for(lambda:self.state.metrics["held_requests"]==1)
        self.assertEqual(self.query("eth_blockNumber")["result"],"0x1")
        self.assertEqual(self.upstream.counts.get("eth_sendRawTransaction",0),0)
        original.close()
        self.wait_for(lambda:self.state.metrics["abandoned_held_requests"]==1)
        self.assertEqual(self.upstream.counts.get("eth_sendRawTransaction",0),0)
        result={}
        def restarted():
            result.update(self.query("eth_sendRawTransaction",["0x010203"]))
        thread=threading.Thread(target=restarted)
        thread.start()
        self.wait_for(lambda:self.state.metrics["held_requests"]==1)
        self.configuration("hold-before-send",release=True,generation=1)
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(result["result"],TX_HASH)
        self.assertEqual(self.upstream.counts["eth_sendRawTransaction"],1)
        self.assertEqual(self.query("eth_sendRawTransaction",["0x010203"])["result"],TX_HASH)
        self.assertEqual(self.upstream.counts["eth_sendRawTransaction"],1)
    def test_invalid_control_during_hold_blocks_release_without_blind_send(self):
        self.configuration("hold-before-send",release=False,generation=1)
        original=self.send_socket("0x030405")
        self.wait_for(lambda:self.state.metrics["held_requests"]==1)
        self.state.control.write_text("{broken")
        response=original.recv(4096)
        original.close()
        self.assertIn(b"503",response)
        self.assertEqual(self.upstream.counts.get("eth_sendRawTransaction",0),0)
    def test_unknown_methods_and_nonregular_control_fail_closed(self):
        with self.assertRaises(urllib.error.HTTPError):
            self.query("anvil_setBalance",["0x123","0x0"])
        self.state.control.unlink()
        self.state.control.symlink_to(self.state.metrics_path)
        self.assertEqual(self.state.configuration()["mode"],"http-failure")
        self.state.control.unlink()
    def test_actual_http429_retry_after_read_only_and_recovery(self):
        self.configuration("rate-limit",methods=["eth_getLogs"])
        with self.assertRaises(urllib.error.HTTPError) as failure:
            self.query("eth_getLogs")
        self.assertEqual(failure.exception.code,429)
        self.assertEqual(failure.exception.headers["Retry-After"],"1")
        self.assertEqual(self.state.metrics["rate_limit_responses"],1)
        self.assertEqual(self.upstream.counts.get("eth_getLogs",0),0)
        self.assertEqual(self.query("eth_blockNumber")["result"],"0x1")
        with self.assertRaises(urllib.error.HTTPError):
            self.query("eth_sendRawTransaction",["0x0102"])
        self.assertEqual(self.state.metrics["blocked_write_requests"],1)
        self.assertEqual(self.state.metrics["upstream_send_attempts"],0)
        self.configuration("pass")
        self.assertEqual(self.query("eth_getLogs")["result"],[])
    def test_real_client_deadline_timeout_no_http503_and_recovery(self):
        self.configuration("read-timeout",methods=["eth_getLogs"],delay_seconds=35)
        record_event=self.state.event
        def slow_start_event(name,*args,**kwargs):
            if name=="read-timeout-started-no-response":
                # Slow metrics storage must not look like an early disconnect.
                time.sleep(0.6)
            return record_event(name,*args,**kwargs)
        started=time.monotonic()
        with patch.object(self.state,"event",side_effect=slow_start_event):
            with self.assertRaises((TimeoutError,socket.timeout)):
                self.query("eth_getLogs",timeout=15.25)
        self.assertGreaterEqual(time.monotonic()-started,15.1)
        # The event follows the counter update; wait for the complete observation.
        self.wait_for(lambda:any(event["kind"]=="read-timeout-client-expired" for event in self.state.metrics["events"]))
        self.assertEqual(self.state.metrics["read_timeout_clients_expired"],1)
        events=[event for event in self.state.metrics["events"] if event["kind"]=="read-timeout-client-expired"]
        self.assertEqual(len(events),1)
        self.assertGreaterEqual(events[0]["elapsed_ms"],14900)
        self.assertEqual(self.state.metrics["read_timeout_windows_expired"],0)
        self.assertEqual(self.state.metrics["read_timeout_early_disconnects"],0)
        self.assertEqual(self.upstream.counts.get("eth_getLogs",0),0)
        self.configuration("pass")
        self.assertEqual(self.query("eth_getLogs")["result"],[])
        self.assertEqual(self.state.metrics["upstream_send_attempts"],0)
    def test_early_disconnect_is_not_timeout_and_invalid_timeout_control_fails_closed(self):
        self.configuration("read-timeout",methods=["eth_getLogs"])
        body=json.dumps({"jsonrpc":"2.0","id":8,"method":"eth_getLogs","params":[]}).encode()
        sock=socket.create_connection(("127.0.0.1",self.server.server_port),timeout=5)
        sock.sendall(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: "+str(len(body)).encode()+b"\r\n\r\n"+body)
        self.wait_for(lambda:self.state.metrics["read_timeout_started"]==1)
        sock.close()
        self.wait_for(lambda:self.state.metrics["read_timeout_early_disconnects"]==1)
        self.assertEqual(self.state.metrics["read_timeout_clients_expired"],0)
        for control in [{"methods":["eth_sendRawTransaction"]},{"methods":[]},{"methods":["eth_getLogs"],"delay_seconds":15},{"methods":["eth_getLogs"],"delay_seconds":61}]:
            self.configuration("read-timeout",**control)
            self.assertEqual(self.state.configuration()["mode"],"http-failure")
            with self.assertRaises(urllib.error.HTTPError) as failure:
                self.query("eth_blockNumber")
            self.assertEqual(failure.exception.code,503)
        self.assertEqual(self.state.metrics["upstream_send_attempts"],0)


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--output",required=True)
    args=parser.parse_args()
    output=Path(args.output).absolute()
    output.mkdir(parents=True,exist_ok=False)
    stream=io.StringIO()
    result=unittest.TextTestRunner(stream=stream,verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(Cases))
    log=output/"proxy-tests.log"
    log.write_text(stream.getvalue())
    report={"passed":result.wasSuccessful(),"tests_run":result.testsRun,"failures":len(result.failures),"errors":len(result.errors),"network":"synthetic-loopback-only","source_sha256":{str(path):hashlib.sha256(path.read_bytes()).hexdigest() for path in [SOURCE,Path(__file__).resolve()]},"log_sha256":hashlib.sha256(log.read_bytes()).hexdigest()}
    (output/"acceptance.json").write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report))
    if not result.wasSuccessful():
        raise SystemExit(1)


if __name__=="__main__":
    main()
