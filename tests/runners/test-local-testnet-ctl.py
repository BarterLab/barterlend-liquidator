#!/usr/bin/env python3
"""Isolated testnet_ctl acceptance using only a development key and loopback Anvil."""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def rpc(url, method, params=()):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    request = urllib.request.Request(url, data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}).encode(), headers={"Content-Type": "application/json"})
    with opener.open(request, timeout=5) as response:
        value = json.load(response)
    require("error" not in value, "local RPC method failed: " + method)
    return value["result"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--anvil", required=True)
    parser.add_argument("--ctl", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output).absolute()
    output.mkdir(parents=True, exist_ok=False)
    report = {"network": "isolated-loopback-only", "passed": False, "scenarios": [], "commands": []}
    processes = []
    proxy_server = None
    mining_active = threading.Event()
    mining_allowed = threading.Event()
    try:
        with tempfile.TemporaryDirectory(prefix="bl-testnet-ctl-") as temporary:
            work = Path(temporary).resolve()
            work.chmod(0o700)
            key = work / "development-key.txt"
            key.write_text("0000000000000000000000000000000000000000000000000000000000000001\n")
            key.chmod(0o600)
            owner = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
            origin = "http://127.0.0.1:" + str(port())
            node_log = (output / "anvil.log").open("w")
            node = subprocess.Popen([args.anvil, "--host", "127.0.0.1", "--port", origin.rsplit(":", 1)[1], "--network", "monad", "--chain-id", "31337", "--silent"], stdout=node_log, stderr=subprocess.STDOUT)
            processes.append(node)
            for _ in range(100):
                try:
                    require(rpc(origin, "eth_chainId") == "0x7a69", "Anvil chain mismatch")
                    break
                except (OSError, urllib.error.URLError):
                    time.sleep(0.05)
            else:
                raise RuntimeError("Anvil startup failed")
            rpc(origin, "anvil_setBalance", [owner, hex(20 * 10**18)])
            # Only the isolated node is advanced; public finality is never guessed this way.
            mining_active.set()
            mining_allowed.set()
            def advance_local_finality():
                previous_nonce = 0
                while mining_active.is_set():
                    try:
                        nonce = int(rpc(origin, "eth_getTransactionCount", [owner, "latest"]), 16)
                        if nonce < previous_nonce:
                            previous_nonce=nonce
                        if mining_allowed.is_set() and nonce > previous_nonce:
                            rpc(origin, "anvil_mine", ["0x40", "0x0"])
                            previous_nonce = nonce
                    except (OSError, urllib.error.URLError):
                        pass
                    time.sleep(0.05)
            threading.Thread(target=advance_local_finality, daemon=True).start()

            class Proxy(http.server.BaseHTTPRequestHandler):
                drop_next = False
                reject_next = False
                raw_sends = []
                def log_message(self, *unused):
                    pass
                def do_POST(self):
                    length = int(self.headers.get("Content-Length", "0"))
                    require(length < 2 * 1024 * 1024, "oversized local request")
                    body = self.rfile.read(length)
                    value = json.loads(body)
                    if value["method"] == "eth_sendRawTransaction" and Proxy.reject_next:
                        Proxy.reject_next = False
                        self.send_response(503)
                        self.end_headers()
                        return
                    try:
                        result = rpc(origin, value["method"], value["params"])
                    except RuntimeError:
                        data=json.dumps({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32000,"message":"injected/local RPC failure"}}).encode()
                        self.send_response(200)
                        self.send_header("Content-Length",str(len(data)))
                        self.end_headers()
                        self.wfile.write(data)
                        return
                    if value["method"] == "eth_sendRawTransaction":
                        Proxy.raw_sends.append(value["params"][0])
                        if Proxy.drop_next:
                            Proxy.drop_next = False
                            self.send_response(503)
                            self.end_headers()
                            return
                    data = json.dumps({"jsonrpc": "2.0", "id": value["id"], "result": result}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)

            proxy_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
            threading.Thread(target=proxy_server.serve_forever, daemon=True).start()
            proxy = "http://127.0.0.1:" + str(proxy_server.server_port)
            journal = work / "journal.json"
            common = [args.ctl, "--local", "--chain-id", "31337", "--rpc", proxy, "--owner", owner, "--journal", str(journal), "--key-file", str(key), "--budget-wei", hex(15 * 10**18), "--reserve-wei", "0x0", "--wait-seconds", "3"]

            def ctl(command, success=True, wait=None):
                argv = list(common)
                if wait is not None:
                    argv[argv.index("--wait-seconds") + 1] = str(wait)
                argv += command
                completed = subprocess.run(argv, capture_output=True, text=True, timeout=30)
                # No raw signed bytes or private keys enter retained evidence.
                require(key.read_text().strip() not in completed.stdout + completed.stderr, "development secret leaked")
                record = {"argv": argv, "exit_code": completed.returncode, "stdout": completed.stdout, "stderr": completed.stderr}
                report["commands"].append(record)
                require((completed.returncode == 0) == success, "unexpected CLI outcome: " + " ".join(command) + ": " + completed.stderr)
                return json.loads(completed.stdout) if success else completed

            def mark(name):
                report["scenarios"].append({"name": name, "passed": True})

            def plan(name, to, data="0x", value="0x0", **extra):
                path = work / (name + ".json")
                value = {"id": name, "to": to, "input": data, "value": value, **extra}
                path.write_text(json.dumps(value))
                return path

            inspected = ctl(["inspect"])
            require(inspected["chain_id"] == 31337 and inspected["key_matches"] and inspected["latest_nonce"] == 0, "inspect mismatch")
            mark("inspect_matches_key_chain_nonce")
            original_common=list(common)
            common.extend(["--genesis","0x"+"00"*32])
            ctl(["inspect"],success=False)
            common[:]=original_common
            rpc(origin,"anvil_setCode",[owner,"0xef0100"+"11"*20])
            ctl(["inspect"],success=False)
            rpc(origin,"anvil_setCode",[owner,"0x"])
            mark("wrong_genesis_and_delegated_EOA_reject_before_send")
            generation = ctl(["generate-key-file", "--output", str(work / "generated.key")])
            require(len(generation["address"]) == 42 and (work / "generated.key").stat().st_mode & 0o077 == 0, "generated key permissions")
            regenerated=ctl(["generate-key-file", "--output", str(work / "generated.key")])
            recovered_address=ctl(["key-address", "--input", str(work / "generated.key")])
            require(regenerated["created"] is False and regenerated["address"] == generation["address"] == recovered_address["address"], "generated actor recovery changed key")
            mark("generated_actor_private_file_idempotent_crash_recovery")

            # Actual EVM creation returning the fixed bytes32 value42; no state injection.
            creation = plan("creation", None, "0x600a600c600039600a6000f3602a60005260206000f3")
            simulated = ctl(["simulate", "--plan", str(creation)])
            require(simulated["sent"] is False and rpc(origin, "eth_getTransactionCount", [owner, "latest"]) == "0x0" and not journal.exists(), "simulation unexpectedly mutated")
            mark("simulate_create_estimate_no_signature_or_send")
            created = ctl(["send", "--plan", str(creation)])
            require(created["finalized"] and created["contract_address"] == simulated["expected_contract"], "creation finality/address mismatch")
            require(journal.stat().st_mode & 0o077 == 0, "signed journal permissions")
            repeated = ctl(["send", "--plan", str(creation)])
            require(repeated["hash"] == created["hash"] and rpc(origin, "eth_getTransactionCount", [owner, "latest"]) == "0x1" and len(Proxy.raw_sends) == 1, "idempotent deployment duplicated send")
            require(ctl(["call", "--to", created["contract_address"], "--data", "0x"])["result"] == "0x" + "0" * 62 + "2a", "actual getter result")
            mark("actual_creation_finalized_runtime_getter_and_idempotency")
            changed = json.loads(creation.read_text()); changed["input"] += "00"; creation.write_text(json.dumps(changed))
            ctl(["send", "--plan", str(creation)], success=False)
            mark("same_id_mutated_creation_rejected")

            bad = plan("reverting", None, "0x60006000fd")
            ctl(["send", "--plan", str(bad)], success=False)
            require(len(json.loads(journal.read_text())["journal"]["entries"]) == 1, "failed simulation reserved nonce")
            excessive = plan("overspend", owner, value=hex(15 * 10**18))
            ctl(["send", "--plan", str(excessive)], success=False)
            require(rpc(origin, "eth_getTransactionCount", [owner, "latest"]) == "0x1", "no-send guard advanced nonce")
            mark("revert_and_budget_guards_leave_nonce_unchanged")

            constructor=work/"constructor.hex"
            constructor.write_text("0x\n")
            for style in ("string","object"):
                artifact=work/("artifact-"+style+".json")
                creation_code="0x600a600c600039600a6000f3602a60005260206000f3"
                artifact.write_text(json.dumps({"bytecode":creation_code if style=="string" else {"object":creation_code}}))
                cast=str(Path(args.anvil).resolve().parent/"cast")
                artifact_hash=subprocess.run([cast,"keccak","0x"+artifact.read_bytes().hex()],capture_output=True,text=True,check=True).stdout.strip()
                command=["deploy","--id","artifact-"+style,"--artifact",str(artifact),"--artifact-keccak",artifact_hash,"--constructor-file",str(constructor),"--expected-runtime-hash",created["runtime_code_hash"]]
                deployed=ctl(command)
                repeated=ctl(command)
                require(deployed["hash"]==repeated["hash"] and deployed["runtime_code_hash"]==created["runtime_code_hash"],"artifact deployment/idempotency mismatch")
            artifact_command=list(command)
            artifact_command[artifact_command.index("--id")+1]="tampered-artifact"
            artifact_command[artifact_command.index("--artifact-keccak")+1]="0x"+"00"*32
            before_sends=len(Proxy.raw_sends)
            ctl(artifact_command,success=False)
            require(len(Proxy.raw_sends)==before_sends,"artifact hash mismatch sent transaction")
            mark("Foundry_and_V3_artifact_styles_deploy_and_hash_guard")

            action = plan("uncertain", created["contract_address"], expected_runtime_hash=created["runtime_code_hash"], after=[{"to": created["contract_address"], "data": "0x", "expected_result": "0x" + "0" * 62 + "2a"}])
            Proxy.drop_next = True
            ctl(["send", "--plan", str(action)], success=False)
            retained = json.loads(journal.read_text())["journal"]["entries"][-1]
            original_raw, original_hash = retained["raw"], retained["hash"]
            recovered = ctl(["resume"])
            require(recovered["hash"] == original_hash and json.loads(journal.read_text())["journal"]["entries"][-1]["raw"] == original_raw and len(Proxy.raw_sends) == 4, "uncertain recovery replaced/resent mined intent")
            mark("accepted_send_response_lost_restart_recovers_same_hash")

            mining_allowed.clear()
            rpc(origin, "anvil_setAutomine", [False])
            pending = plan("pending", created["contract_address"])
            ctl(["send", "--plan", str(pending)], success=False, wait=0)
            retained = json.loads(journal.read_text())["journal"]["entries"][-1]
            pending_hash = retained["hash"]
            before_sends = len(Proxy.raw_sends)
            ctl(["resume"], success=False, wait=0)
            require(len(Proxy.raw_sends) == before_sends, "known pending raw transaction blindly resent")
            rpc(origin, "evm_mine")
            mining_allowed.set()
            recovered = ctl(["resume"])
            require(recovered["hash"] == pending_hash and recovered["finalized"], "pending restart did not finalize original")
            rpc(origin, "anvil_setAutomine", [True])
            mark("pending_timeout_restart_keeps_signed_bytes_no_blind_resend")

            before_accept = plan("uncertain-before-accept",created["contract_address"])
            Proxy.reject_next=True
            ctl(["send","--plan",str(before_accept)],success=False)
            retained=json.loads(journal.read_text())["journal"]["entries"][-1]
            prior_raw,prior_hash=retained["raw"],retained["hash"]
            recovered=ctl(["resume"])
            require(recovered["hash"]==prior_hash and json.loads(journal.read_text())["journal"]["entries"][-1]["raw"]==prior_raw,"rejected send restart changed signed bytes")
            mark("send_rejected_before_accept_resume_rebroadcasts_exact_raw")

            # A real independently signed transaction consumes the original next nonce.
            snapshot=rpc(origin,"evm_snapshot")
            common[common.index("--journal")+1]=str(work/"external-owner-journal.json")
            foreign=plan("external-signer-action",owner)
            ctl(["send","--plan",str(foreign)])
            common[:]=original_common
            before_sends=len(Proxy.raw_sends)
            skipped=plan("external-nonce-blocked",owner)
            ctl(["send","--plan",str(skipped)],success=False)
            require(len(Proxy.raw_sends)==before_sends,"external nonce consumption allowed another send")
            require(rpc(origin,"evm_revert",[snapshot]),"isolated branch revert failed")
            mark("actual_external_nonce_consumption_stops_new_action")

            common[common.index("--journal")+1]=str(work/"gas-cap-journal.json")
            common.extend(["--max-gas-price-wei","0x1"])
            before_sends=len(Proxy.raw_sends)
            cap_plan=plan("gas-cap-blocked",owner)
            ctl(["send","--plan",str(cap_plan)],success=False)
            common[:]=original_common
            common[common.index("--journal")+1]=str(work/"reserve-journal.json")
            common[common.index("--reserve-wei")+1]=hex(20*10**18)
            ctl(["send","--plan",str(cap_plan)],success=False)
            common[:]=original_common
            require(len(Proxy.raw_sends)==before_sends,"gas cap/reserve guard sent a transaction")
            mark("gas_price_cap_and_native_reserve_prevent_send")

            missing_event = plan("missing-event", created["contract_address"], events=[{"address": created["contract_address"], "topic0": "0x" + "00" * 32, "min_count": 1}])
            ctl(["send", "--plan", str(missing_event)], success=False)
            retained = json.loads(journal.read_text())["journal"]["entries"][-1]
            require(retained["receipt"] is not None and retained["validated"] is False, "failed assertion lost finalized receipt")
            blocked = plan("blocked", owner)
            before_sends = len(Proxy.raw_sends)
            ctl(["send", "--plan", str(blocked)], success=False)
            ctl(["resume"], success=False)
            require(len(Proxy.raw_sends) == before_sends, "failed assertion allowed new/broadcast transaction")
            mark("missing_event_retains_receipt_and_blocks_new_transactions")

            report["passed"] = True
            report["actual_signed_transactions"] = len(Proxy.raw_sends)
            report["source_sha256"] = {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in [Path(args.ctl).resolve(), Path(__file__).resolve(), Path(__file__).resolve().parents[2] / "src/bin/testnet_ctl.rs"]}
    finally:
        mining_active.clear()
        if proxy_server:
            proxy_server.shutdown()
            proxy_server.server_close()
        for process in processes:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        (output / "acceptance.json").write_text(json.dumps(report, indent=2) + "\n")
    require(report["passed"], "testnet control CLI local acceptance failed")
    print(json.dumps({"passed": True, "scenarios": len(report["scenarios"]), "actual_signed_transactions": report["actual_signed_transactions"], "report": str(output / "acceptance.json")}))


if __name__ == "__main__":
    main()
