#!/usr/bin/env python3
"""Sequential reproducible testnet deployment. Only testnet_ctl reads keys.

Plan makes no network requests. State-changing phases require --execute; outcome reads RPC
without a signer. Signed journals stay outside the repository; evidence contains only public
plans, receipt summaries and fingerprints, never raw transactions.
"""
import argparse
import fcntl
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import urllib.parse

REPO = pathlib.Path(__file__).resolve().parents[2]
WAD = 10**18
ZERO = "0x" + "00" * 20
POOL_INIT_HASH = "e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54"


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def sha(value):
    return hashlib.sha256(value).hexdigest()


def finalized_health(health, expected):
    # /ready returns JSON null before the application's first health frame.
    if not isinstance(health, dict):
        return False
    count = health.get("counters", {}).get("finalized_liquidations")
    if not isinstance(count, int):
        return False
    if count > expected:
        raise RuntimeError("more liquidations than the isolated scenario expected")
    return count == expected and health.get("tx", {}).get("phase") == "idle"


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_symlink():
        raise RuntimeError("output must not be a symlink")
    temp = path.with_name(path.name + f".tmp-{os.getpid()}")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(json.dumps(value, indent=2, ensure_ascii=False).encode() + b"\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if temp.exists():
            temp.unlink()


def abi_type(item):
    typ = item["type"]
    if typ.startswith("tuple"):
        return "(" + ",".join(abi_type(part) for part in item["components"]) + ")" + typ[5:]
    return typ


def abi_arg(item, value):
    typ = item["type"]
    if typ.endswith("]"):
        base = dict(item, type=typ[:typ.rfind("[")])
        if not isinstance(value, list):
            raise RuntimeError("ABI array requires list")
        return "[" + ",".join(abi_arg(base, part) for part in value) + "]"
    if typ == "tuple":
        if not isinstance(value, list) or len(value) != len(item["components"]):
            raise RuntimeError("ABI tuple arity mismatch")
        return "(" + ",".join(abi_arg(part, arg) for part, arg in zip(item["components"], value)) + ")"
    if typ == "bool":
        if not isinstance(value, bool):
            raise RuntimeError("ABI bool requires bool")
        return "true" if value else "false"
    return str(value)


def resolve(value, context):
    if isinstance(value, list):
        return [resolve(part, context) for part in value]
    if not isinstance(value, str) or not value.startswith("${"):
        return value
    if not value.endswith("}"):
        raise RuntimeError("malformed template reference")
    name = value[2:-1]
    if name == "receipt_bytes32_left_padded":
        return "0x" + context["receipt"][2:].lower().rjust(64, "0")
    if name.startswith("asset_lt_wrapped ? "):
        options = name.removeprefix("asset_lt_wrapped ? ").split(" : ")
        if len(options) != 2 or not all(re.fullmatch(r"[0-9]+", part) for part in options):
            raise RuntimeError("invalid known native price expression")
        return options[0 if int(context["asset"], 16) < int(context["wrapped_native"], 16) else 1]
    if name not in context:
        raise RuntimeError(f"unresolved public deployment reference: {name}")
    return context[name]


class Runner:
    def __init__(self, args):
        self.args = args
        self.index_path = args.artifacts.resolve()
        self.index = json.loads(self.index_path.read_text())
        self.template_path = args.template.resolve()
        self.template = json.loads(self.template_path.read_text())
        if self.index.get("build_exit_code") != 0 or self.index.get("target_chain_id") != 10143:
            raise RuntimeError("artifact build/target is not accepted")
        for name, expected in self.index["source_sha256"].items():
            if sha((REPO / name).read_bytes()) != expected:
                raise RuntimeError(f"artifact source fingerprint changed: {name}")
        self.contracts = self.index["contracts"]
        self.directory = args.evidence_dir.resolve()
        self.directory.mkdir(parents=True, exist_ok=True)
        self.path = self.directory / "DEPLOYMENT_STATUS.json"
        self.binding = {"chain_id": 31337 if args.local_rehearsal else 10143,
                        "genesis": args.genesis.lower(), "owner": args.owner.lower(), "rpc": args.rpc,
                        "owner_start_nonce": getattr(args, "owner_start_nonce", 0),
                        "artifact_index_sha256": sha(self.index_path.read_bytes()),
                        "template_sha256": sha(self.template_path.read_bytes()),
                        "budget_wei": str(args.budget_wei), "max_gas_price_wei": str(args.max_gas_price_wei),
                        "secret_dir": str(args.secret_dir.absolute()), "local_rehearsal": args.local_rehearsal,
                        "testnet_proxy": args.testnet_proxy}
        self.state = json.loads(self.path.read_text()) if self.path.exists() else {
            "schema": 1, "binding": self.binding, "actors": {}, "steps": {}, "contracts": {},
            "phase": "prepared", "synthetic_dependencies": ["asset", "collateral", "oracle", "underlying", "receipt"],
            "production_ready": False}
        if self.state.get("binding") != self.binding:
            raise RuntimeError("deployment binding changed; existing journals/evidence cannot be reused")
        self.context = {"owner": args.owner}
        self.context.update({name: data["address"] for name, data in self.state["actors"].items()})
        self.context["provider"] = self.context.get("lender", ZERO)
        self.context.update({name: data["contract_address"] for name, data in self.state["contracts"].items()})

    def save(self):
        self.state["updated_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        atomic_json(self.path, self.state)

    def tool(self, argv, timeout=240):
        completed = subprocess.run([str(part) for part in argv], cwd=REPO, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, timeout=timeout)
        if completed.returncode != 0:
            # Only trusted tools with file-path key input are invoked; never collect environment or secret files.
            raise RuntimeError(f"{pathlib.Path(str(argv[0])).name} failed ({completed.returncode}): {completed.stderr[-1500:].strip()}")
        if len(completed.stdout) > 16 * 1024 * 1024:
            raise RuntimeError("tool output exceeds limit")
        return completed.stdout.strip()

    def cast(self, *args):
        return self.tool([self.args.cast, *args])

    def encode_constructor(self, contract, values):
        constructor = next((a for a in self.contracts[contract]["abi"] if a["type"] == "constructor"), {"inputs": []})
        inputs = constructor["inputs"]
        if len(inputs) != len(values):
            raise RuntimeError(f"constructor arity mismatch: {contract}")
        signature = "constructor(" + ",".join(abi_type(part) for part in inputs) + ")"
        return self.cast("abi-encode", signature, *(abi_arg(part, value) for part, value in zip(inputs, values)))

    def calldata(self, method, args):
        # Signatures are exact ABI canonical types; nested constructor arrays are handled separately.
        return self.cast("calldata", method, *("true" if arg is True else "false" if arg is False else str(arg) for arg in args))

    def common(self, actor="owner", key=False):
        address = self.args.owner if actor == "owner" else self.state["actors"][actor]["address"]
        journal = self.args.secret_dir / f"{actor}-control-journal.json"
        reserve = self.args.owner_reserve_wei if actor == "owner" else self.args.actor_reserve_wei
        argv = [self.args.ctl, "--rpc", self.args.rpc, "--chain-id", self.binding["chain_id"],
                "--genesis", self.args.genesis, "--owner", address, "--journal", journal,
                "--budget-wei", hex(self.args.budget_wei), "--reserve-wei", hex(reserve),
                "--max-gas-price-wei", hex(self.args.max_gas_price_wei), "--wait-seconds", "180"]
        if self.args.local_rehearsal:
            argv.append("--local")
        elif self.args.testnet_proxy:
            argv.append("--testnet-proxy")
        if key:
            argv += ["--key-file", self.args.owner_key_file if actor == "owner" else self.state["actors"][actor]["key_file"]]
        return argv

    def ctl(self, *command, actor="owner", key=False):
        # Controller observations can overlap the live bot. Retry only known
        # read-only HTTP failures; signed mutations have their own journal.
        for attempt in range(3):
            try:
                value = json.loads(self.tool(self.common(actor, key) + list(command)))
                break
            except RuntimeError as error:
                if (attempt == 2 or key or not command or command[0] not in {"inspect", "call"}
                        or "RPC HTTP status/size rejected" not in str(error)):
                    raise
                time.sleep(attempt + 1)
        if not isinstance(value, dict):
            raise RuntimeError("control tool returned non-object")
        return value

    def call(self, to, method, args=()):
        return self.ctl("call", "--to", to, "--data", self.calldata(method, args))["result"]

    def scalar(self, to, method, args=()):
        value = self.call(to, method, args)
        if not re.fullmatch(r"0x[0-9a-fA-F]{64}", value):
            raise RuntimeError("getter did not return one ABI word")
        return int(value, 16)

    def address(self, to, method, args=()):
        return "0x" + format(self.scalar(to, method, args), "064x")[-40:]

    def assertion(self, to, method, args, expected_type, expected):
        return {"to": to, "data": self.calldata(method, args),
                "expected_result": self.cast("abi-encode", f"result({expected_type})", str(expected).lower() if isinstance(expected, bool) else str(expected))}

    def run_step(self, step_id, request, command, actor="owner"):
        fingerprint = sha(canonical(request))
        old = self.state["steps"].get(step_id)
        if old and (old["request_sha256"] != fingerprint or old["actor"] != actor):
            raise RuntimeError(f"step request changed: {step_id}")
        self.state["steps"][step_id] = {"actor": actor, "request_sha256": fingerprint,
                                       "status": old["status"] if old else "prepared"}
        self.save()
        if old and old["status"] in ["uncertain", "passed"]:
            retained = self.ctl("status", actor=actor).get("entries", [])
            if old["status"] == "passed" and not any(entry["id"] == step_id and entry["validated"] for entry in retained):
                raise RuntimeError("completed public step lacks its private signed journal; restore journal before continuing")
            if old["status"] == "uncertain" and retained and (retained[-1]["id"] == step_id or not retained[-1]["validated"]):
                self.ctl("resume", actor=actor, key=True)
        self.state["steps"][step_id]["status"] = "uncertain"
        self.save()
        result = self.ctl(*command, actor=actor, key=True)
        if result.get("id") != step_id or result.get("finalized") is not True or result.get("assertions_passed") is not True:
            raise RuntimeError("finalized control result does not match requested step")
        # No private journal/raw bytes are copied. Historical receipt validation is repeated by ctl on replay.
        self.state["steps"][step_id].update(status="passed", result=result)
        self.save()
        print(json.dumps({"step": step_id, "status": "passed", "hash": result["hash"], "gas_used": result["gas_used"]}), flush=True)
        return result

    def send(self, step_id, to, method=None, args=(), value=0, actor="owner", before=(), after=(), events=()):
        plan = {"id": step_id, "to": to, "input": self.calldata(method, args) if method else "0x",
                "value": hex(value), "max_gas": 30_000_000, "before": list(before), "after": list(after), "events": list(events)}
        path = self.directory / "plans" / f"{step_id}.json"
        if path.exists() and json.loads(path.read_text()) != plan:
            raise RuntimeError(f"persisted plan changed: {step_id}")
        atomic_json(path, plan)
        if step_id not in self.state["steps"]:
            simulation = self.ctl("simulate", "--plan", path, actor=actor)
            if simulation.get("simulation_passed") is not True or simulation.get("sent") is not False:
                raise RuntimeError("pre-send simulation missing")
        return self.run_step(step_id, plan, ["send", "--plan", path], actor)

    def actors(self):
        secret = self.args.secret_dir.absolute()
        if secret.is_relative_to(REPO) or any((p / ".git").exists() for p in [secret.resolve(), *secret.resolve().parents]):
            raise RuntimeError("generated keys and signed journals must remain outside repositories")
        for parent in [secret, *secret.parents]:
            if parent.exists() and parent.is_symlink():
                raise RuntimeError("secret directory ancestors must not be symlinks")
        secret.mkdir(parents=True, exist_ok=True)
        os.chmod(secret, 0o700)
        for role in ["operator", "lender", "borrower"]:
            path = secret / f"{role}-key.txt"
            if role not in self.state["actors"]:
                result = self.ctl("generate-key-file", "--output", path)
                self.state["actors"][role] = {"address": result["address"], "key_file": str(path)}
                self.save()
            self.context[role] = self.state["actors"][role]["address"]
        self.context["provider"] = self.context["lender"]

    def deploy(self):
        self.state["phase"] = "deploying"
        self.save()
        inspected = self.ctl("inspect", key=True)
        if inspected["chain_id"] != self.binding["chain_id"] or inspected["genesis"].lower() != self.args.genesis.lower():
            raise RuntimeError("inspection identity changed")
        if not self.state["steps"]:
            expected = getattr(self.args, "owner_start_nonce", 0)
            if inspected["latest_nonce"] != expected or inspected["pending_nonce"] != expected:
                raise RuntimeError("fresh experiment requires the explicitly pinned owner nonce and no pending transaction")
        self.state.setdefault("initial_inspection", inspected)
        self.save()
        self.actors()
        for item in self.template["deployments"]:
            if item["id"] == "credit_pool":
                for number, action in enumerate(self.template["before_credit_pool"]):
                    self.action(f"bind-receipt-{number}", action)
            contract = item["contract"]
            arguments = resolve(item["constructor"], self.context)
            encoded = self.encode_constructor(contract, arguments)
            artifact = self.contracts[contract]
            artifact_file = self.directory / "artifacts" / f"{item['id']}.json"
            artifact_value = {"abi": artifact["abi"], "bytecode": {"object": artifact["bytecode"]}}
            if artifact_file.exists() and json.loads(artifact_file.read_text()) != artifact_value:
                raise RuntimeError("deployment artifact changed")
            atomic_json(artifact_file, artifact_value)
            constructor_file = self.directory / "artifacts" / f"{item['id']}.constructor.hex"
            if constructor_file.exists() and constructor_file.read_text().strip() != encoded:
                raise RuntimeError("persisted constructor encoding changed")
            constructor_file.write_text(encoded + "\n")
            artifact_keccak = self.cast("keccak", "0x" + artifact_file.read_bytes().hex())
            request = {"contract": contract, "artifact_sha256": sha(artifact_file.read_bytes()),
                       "artifact_keccak": artifact_keccak, "constructor": encoded}
            result = self.run_step("deploy-" + item["id"], request,
                                   ["deploy", "--id", "deploy-" + item["id"], "--artifact", artifact_file,
                                    "--artifact-keccak", artifact_keccak, "--constructor-file", constructor_file])
            if not result.get("contract_address") or not result.get("runtime_code_hash"):
                raise RuntimeError("creation result lacks deployed identity")
            self.state["contracts"][item["id"]] = result
            self.context[item["id"]] = result["contract_address"]
            self.save()
        for number, action in enumerate(self.template["owner_configuration"]):
            self.action(f"owner-config-{number:02d}", action)
        for role, amount in [("operator", WAD), ("lender", 4 * WAD // 10), ("borrower", 4 * WAD // 10)]:
            self.send("fund-" + role, self.context[role], value=amount)
        self.write_manifest()
        self.state["phase"] = "deployed"
        self.save()

    def pool_address(self, a, b, fee):
        token0, token1 = sorted([a, b], key=lambda value: int(value, 16))
        salt = self.cast("keccak", self.cast("abi-encode", "salt(address,address,uint24)", token0, token1, fee))
        return "0x" + self.cast("keccak", "0xff" + self.context["factory"][2:] + salt[2:] + POOL_INIT_HASH)[-40:]

    def action(self, step_id, action):
        to = resolve(action["to"], self.context)
        args = resolve(action.get("args", []), self.context)
        method = action["method"]
        after = []
        before = []
        if method == "setPool(address)":
            after.append(self.assertion(to, "POOL()", [], "address", args[0]))
        elif method == "registerPool(address)":
            after.append(self.assertion(to, "isPool(address)", args, "bool", True))
            after.append(self.assertion(self.context["executor"], "isPool(address)", args, "bool", True))
        elif method == "approve(address,uint256)":
            actor = action.get("actor", "owner")
            address = self.context[actor]
            after.append(self.assertion(to, "allowance(address,address)", [address, args[0]], "uint256", args[1]))
        elif method in ["setVerifiedToken(address,bool)", "setIntermediate(address,bool)", "setFee(uint24,bool)"]:
            getter = {"setVerifiedToken(address,bool)": "verifiedTokens(address)",
                      "setIntermediate(address,bool)": "intermediates(address)", "setFee(uint24,bool)": "allowedFees(uint24)"}[method]
            after.append(self.assertion(to, getter, [args[0]], "bool", args[1]))
        elif method.startswith("seedPool("):
            pool = self.pool_address(args[0], args[1], args[2])
            before.append(self.assertion(self.context["factory"], "getPool(address,address,uint24)", args[:3], "address", ZERO))
            after.append(self.assertion(self.context["factory"], "getPool(address,address,uint24)", args[:3], "address", pool))
            after.append(self.assertion(pool, "liquidity()", [], "uint128", args[6]))
        return self.send(step_id, to, method, args, int(action.get("value_wei", 0)),
                         actor=action.get("actor", "owner").replace("provider", "lender"), before=before, after=after)

    def write_manifest(self):
        manifest = json.loads((REPO / "examples/local-manifest.json").read_text())
        manifest["profile"] = "local" if self.args.local_rehearsal else "monad-testnet-proxy" if self.args.testnet_proxy else "monad-testnet"
        manifest["rpc_url"] = self.args.rpc
        manifest["rpc_fallback_urls"] = list(getattr(self.args, "rpc_fallback_urls", []))
        if not self.args.local_rehearsal:
            # Public profiles must not inherit the fast Anvil template budget.
            manifest["rpc"] = {"requests_per_second": 15}
        manifest["recovery"] = {"max_head_age_seconds": 120, "max_no_progress_seconds": 30,
                                "max_future_seconds": 15, "retry_initial_ms": 500, "retry_max_ms": 30000}
        manifest["cancellation_retry_seconds"] = 30
        manifest["sdk_history_chunks_per_step"] = 8
        manifest["chain"] = {"chain_id": self.binding["chain_id"], "genesis_hash": self.args.genesis}
        manifest["executor"] = self.context["executor"]
        manifest["executor_code_hash"] = self.state["contracts"]["executor"]["runtime_code_hash"]
        manifest["owner"] = self.args.owner
        manifest["operator"] = self.context["operator"]
        pool = self.state["contracts"]["credit_pool"]
        descriptor = {"spec": {"address": pool["contract_address"], "deployment_block": pool["block_number"],
                                "deployment_block_hash": pool["block_hash"], "expected_version": 5},
                      "code_hash": pool["runtime_code_hash"], "asset": {"address": self.context["asset"], "decimals": 18},
                      "collateral": {"address": self.context["collateral"], "decimals": 18}, "oracle": self.context["oracle"],
                      "oracle_code_hash": self.state["contracts"]["oracle"]["runtime_code_hash"]}
        manifest["pools"] = {"test-stack": descriptor}
        manifest["v3"].update(chain=manifest["chain"], factory=self.context["factory"], router=self.context["router"],
                              quoter=self.context["quoter"], wrapped_native=self.context["wrapped_native"],
                              allowed_tokens=[self.context[key] for key in ["asset", "collateral", "wrapped_native"]],
                              intermediates=[self.context["wrapped_native"]], fee_tiers=[3000], pool_code_hashes={})
        manifest["v3"]["code_hashes"] = {name: self.state["contracts"][name]["runtime_code_hash"] for name in ["factory", "router", "quoter", "wrapped_native"]}
        # Use the same bounded route ages as public configuration. The block-age,
        # oracle expiry, current native-cost and final send checks still bound every plan.
        # Do not regenerate a deployed experiment's historical manifest to change policy.
        ttl_seconds = 60
        max_age_blocks = 200
        manifest["v3"]["limits"].update(quote_ttl_seconds=ttl_seconds, max_age_blocks=max_age_blocks, max_quote_calls=16)
        manifest["planner"]["ttl_seconds"] = ttl_seconds
        manifest["fees"].update(max_gas_price=hex(self.args.max_gas_price_wei), max_fee_native=hex(2_000_000 * self.args.max_gas_price_wei), native_reserve=hex(WAD // 100))
        # New plans permit break-even after estimated gas. Do not rewrite historical manifests.
        manifest.update(additional_profit_asset="0x0", max_total_native_spend=hex(8 * WAD // 10), poll_ms=1000, cooldown_seconds=2)
        atomic_json(self.directory / "bot-manifest.json", manifest)
        self.state["manifest_sha256"] = sha((self.directory / "bot-manifest.json").read_bytes())
        self.state["v3_pools"] = {name: self.address(self.context["factory"], "getPool(address,address,uint24)", [self.context[a], self.context[b], 3000])
                                  for name, a, b in [("collateral_asset", "collateral", "asset"), ("asset_native", "asset", "wrapped_native")]}
        if any(value == ZERO for value in self.state["v3_pools"].values()):
            raise RuntimeError("configured native/unwind V3 pool is absent")
        self.save()

    def preview(self):
        context = {"owner": self.args.owner, "operator": "0x" + "11" * 20}
        items = []
        for number, item in enumerate(self.template["deployments"], 1):
            values = resolve(item["constructor"], context)
            encoded = self.encode_constructor(item["contract"], values)
            items.append({"id": item["id"], "contract": item["contract"], "constructor_bytes": (len(encoded) - 2) // 2,
                          "initcode_bytes": (len(self.contracts[item["contract"]]["bytecode"]) - 2) // 2 + (len(encoded) - 2) // 2})
            context[item["id"]] = "0x" + format(number, "040x")
        atomic_json(self.directory / "DEPLOYMENT_PLAN.json", {"scope": "local_encoding_only", "deployments": items,
                    "artifact_index_sha256": self.binding["artifact_index_sha256"], "template_sha256": self.binding["template_sha256"],
                    "key_read": False, "network": "none", "actor_funding_wei": str(18 * WAD // 10),
                    "honest_wrapped_native_deposit_wei": str(WAD), "public_execution": False})
        print(json.dumps({"phase": "plan", "deployments": len(items), "network": "none", "keys_read": False}))

    def active_markets(self):
        raw = bytes.fromhex(self.call(self.context["credit_pool"], "getActiveMarkets()")[2:])
        if len(raw) < 64 or len(raw) % 32 or int.from_bytes(raw[:32], "big") != 32:
            raise RuntimeError("malformed actual active-market array")
        count = int.from_bytes(raw[32:64], "big")
        if count > 64 or len(raw) != 64 + 32 * count:
            raise RuntimeError("active-market array outside bounded ABI shape")
        values = [int.from_bytes(raw[64 + i * 32:96 + i * 32], "big") for i in range(count)]
        if any(value >= 2**64 for value in values) or len(set(values)) != len(values):
            raise RuntimeError("invalid active-market values")
        return values

    def operator_nonce(self):
        data = self.ctl("inspect", actor="operator")
        if data["latest_nonce"] != data["pending_nonce"]:
            raise RuntimeError("operator has an unresolved transaction")
        return data["latest_nonce"]

    def oracle_observation(self):
        data = self.call(self.context["oracle"], "getPrice()")
        if not re.fullmatch(r"0x[0-9a-fA-F]{192}", data):
            raise RuntimeError("oracle getter did not return the exact three-word Price tuple")
        return tuple(int(data[2 + i * 64:66 + i * 64], 16) for i in range(3))

    def ensure_oracle(self, label, numerator, denominator, updated_at=None):
        # A completed receipt describes history, never the current mutable oracle. Resolve every
        # durable uncertain owner request first; a new nonce must not bypass an earlier signed step.
        for step_id, entry in list(self.state["steps"].items()):
            if entry["actor"] == "owner" and entry["status"] == "uncertain":
                path = self.directory / "plans" / f"{step_id}.json"
                if path.is_symlink() or not path.is_file():
                    raise RuntimeError("uncertain owner step needs its original plan before oracle control can continue")
                plan = json.loads(path.read_text())
                self.run_step(step_id, plan, ["send", "--plan", path])
        numerator, denominator = int(numerator), int(denominator)
        head = self.ctl("inspect")["latest_timestamp"]
        current = self.oracle_observation()
        fresh = updated_at is None
        desired_at = head if fresh else int(updated_at)
        matches = current[:2] == (numerator, denominator) and (0 <= head - current[2] <= 60 if fresh else current[2] == desired_at)
        if not matches:
            revision = self.state.get("oracle_control_revision", 0) + 1
            self.state["oracle_control_revision"] = revision
            self.save()
            step_id = f"oracle-{label}-{revision:06d}"
            target = (numerator, denominator, desired_at)
            if fresh:
                # Contract stamps its actual execution block; do not guess that timestamp before
                # the controller's simulation/signing/broadcast/finality calls complete.
                self.send(step_id, self.context["oracle"], "setPriceFraction(uint256,uint256)", [numerator, denominator])
            else:
                expected = "(" + ",".join(str(value) for value in target) + ")"
                self.send(step_id, self.context["oracle"], "setObservation(uint256,uint256,uint256)", target,
                          after=[self.assertion(self.context["oracle"], "getPrice()", [], "(uint256,uint256,uint256)", expected)])
            current = self.oracle_observation()
            if current[:2] != target[:2] or (not fresh and current != target):
                raise RuntimeError("actual oracle observation differs from the exact controlled target after finalized send")
            if fresh and (current[2] < head or not 0 <= self.ctl("inspect")["latest_timestamp"] - current[2] <= 60):
                raise RuntimeError("controlled fresh observation expired while awaiting confirmation; retry with a new durable step")
        self.state.setdefault("oracle_controls", []).append({"label": label, "fresh": fresh, "observation": [str(value) for value in current],
                                                               "observed_head_timestamp": head, "sent_new_step": not matches})
        self.save()
        return current

    def oracle_refresh(self):
        if not self.state.get("positions_passed") or not self.state.get("guards_passed"):
            raise RuntimeError("controlled oracle refresh requires accepted positions and no-send guards")
        _, journal = self.bot_journal()
        if journal["active"] is not None:
            raise RuntimeError("finish the original signed nonce before changing the controlled oracle")
        numerator, denominator = self.args.oracle_numerator, self.args.oracle_denominator
        observation = self.ensure_oracle("fault-replay-fresh-price", numerator, denominator)
        print(json.dumps({"phase": "oracle-refresh", "observation": list(observation),
                          "purpose": "new plans only; previous signed bytes/deadline retained"}))

    @staticmethod
    def bot_events(path):
        events=[]
        for line in pathlib.Path(path).read_text().splitlines():
            if not line.strip(): continue
            # Accept the current text envelope and earlier JSON event logs.
            if line.lstrip().startswith("{"):
                event=json.loads(line)
                if isinstance(event.get("details"),str): event=json.loads(event["details"])
            else:
                _,separator,details=line.partition(" details=")
                if not separator: continue  # Native tracing/span-close messages have no event payload.
                event=json.loads(details)
            if not isinstance(event,dict): raise ValueError("bot event must be an object")
            events.append(event)
        return events

    def bot_run(self, label, manifest=None, cycles=None):
        path = manifest or self.directory / "bot-manifest.json"
        argv = [str(self.args.bot), "--manifest", str(path),
                "--local-live" if self.args.local_rehearsal else "--testnet-live",
                "--private-key-file", self.state["actors"]["operator"]["key_file"],
                "--cycles", str(cycles or self.args.bot_cycles)]
        stdout_path = self.directory / f"{label}.stdout.jsonl"
        stderr_path = self.directory / f"{label}.stderr.jsonl"
        if stdout_path.exists() or stderr_path.exists():
            # Never overwrite a previous failed public run while recording a retry.
            suffix = time.time_ns()
            stdout_path = self.directory / f"{label}-{suffix}.stdout.jsonl"
            stderr_path = self.directory / f"{label}-{suffix}.stderr.jsonl"
        with stdout_path.open("x") as stdout, stderr_path.open("x") as stderr:
            process = subprocess.Popen(argv, cwd=REPO, stdout=stdout, stderr=stderr, env=dict(os.environ, LOG_FILTER="debug"))
            try:
                code = process.wait(timeout=self.args.bot_timeout_seconds)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                process.send_signal(__import__("signal").SIGINT)
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                raise RuntimeError("bounded bot interrupted; inspect operator nonce/receipts before restarting")
        record = {"argv": argv, "exit_code": code, "stdout": str(stdout_path), "stderr": str(stderr_path),
                  "stdout_sha256": sha(stdout_path.read_bytes()), "stderr_sha256": sha(stderr_path.read_bytes())}
        self.state.setdefault("bot_runs", []).append(record)
        self.save()
        if code != 0:
            raise RuntimeError("bounded bot failed; inspect preserved logs and on-chain nonce/receipts")
        stopped = self.bot_events(stdout_path)
        stopped = [event for event in stopped if event.get("event") == "stopped"]
        if len(stopped) != 1:
            raise RuntimeError("bot lacks exactly one bounded stopped outcome")
        return stopped[0]

    def positions(self):
        if len(self.state["contracts"]) != len(self.template["deployments"]) or not (self.directory / "bot-manifest.json").is_file():
            raise RuntimeError("positions require completed deployment and actual manifest")
        if self.state.get("positions_passed"):
            print(json.dumps({"phase": "positions", "status": "already_passed", "market": self.state["scenario_market"]}))
            return
        self.ensure_oracle("positions-healthy-price", WAD, WAD)
        inspection = self.ctl("inspect")
        markets = self.active_markets()
        if "scenario_market" not in self.state:
            fresh = [market for market in markets if market >= inspection["latest_timestamp"] + 120]
            if not fresh:
                raise RuntimeError("no active market with sufficient real-time setup margin")
            self.state["scenario_market"] = max(fresh)
            self.state["market_selected_at"] = inspection["latest_timestamp"]
            self.save()
        market = self.state["scenario_market"]
        if market not in markets or market < inspection["latest_timestamp"] + 60:
            raise RuntimeError("selected position market expired during setup; preserve partially completed actions")
        self.context["market_from_getActiveMarkets"] = market
        for number, action in enumerate(self.template["provided_position_template"]):
            self.action(f"provided-position-{number:02d}", action)
        debt = self.scalar(self.context["credit_pool"], "debtOf(uint64,address)", [market, self.context["borrower"]])
        collateral = self.scalar(self.context["credit_pool"], "collateralOf(uint64,address)", [market, self.context["borrower"]])
        if debt != 100 * WAD or collateral != 400 * WAD:
            raise RuntimeError("actual matching debt/collateral differs from position template")
        self.send("borrower-withdraw-loan", self.context["credit_pool"], "withdraw(address,address,uint128)",
                  [self.context["borrower"], self.context["borrower"], 100 * WAD], actor="borrower")
        before = self.operator_nonce()
        report = self.bot_run("healthy-no-send")
        after = self.operator_nonce()
        if after != before or report.get("finalized_liquidations") != 0 or report.get("active_pending") is not False:
            raise RuntimeError("healthy no-send changed operator nonce or produced liquidation")
        self.state["positions_passed"] = True
        self.state["position_baseline"] = {"market": market, "debt": hex(debt), "collateral": hex(collateral),
                                           "healthy_operator_nonce": before, "healthy_bot_report": report}
        self.state["phase"] = "positions_healthy_passed"
        self.save()
        print(json.dumps({"phase": "positions", "status": "passed", "market": market, "healthy_no_send": True}))

    def live_until(self, label, expected=1, manifest=None, on_health=None):
        """Observe the real RAM bot, stop on finalization, preserve public evidence."""
        import socket
        import signal
        import urllib.request
        import urllib.error
        if not re.fullmatch(r"[a-z0-9-]{1,80}", label) or not 1 <= expected <= 32:
            raise RuntimeError("invalid bounded live scenario")
        result_path = self.directory / (label + "-result.json")
        if result_path.exists():
            raise RuntimeError("live scenario already attempted; inspect retained evidence before repeating")
        source = manifest or self.directory / "bot-manifest.json"
        config = json.loads(source.read_text())
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        config["health_listen"] = f"127.0.0.1:{port}"
        path = self.directory / (label + "-config.json")
        atomic_json(path, config)
        before = self.ctl("inspect", actor="operator")
        if before["latest_nonce"] != before["pending_nonce"]:
            raise RuntimeError("untracked pending operator nonce before bounded live scenario")
        argv = [str(self.args.bot), "--manifest", str(path), "--live", "--private-key-file",
                self.state["actors"]["operator"]["key_file"]]
        stdout_path = self.directory / (label + ".stdout.jsonl")
        stderr_path = self.directory / (label + ".stderr.jsonl")
        record = {"passed": False, "operator_before": before, "expected": expected,
                  "config_sha256": sha(path.read_bytes())}
        atomic_json(result_path, record)
        observed = None
        failure = None
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with stdout_path.open("x") as stdout, stderr_path.open("x") as stderr:
            process = subprocess.Popen(argv, cwd=REPO, stdout=stdout, stderr=stderr,
                                       env=dict(os.environ, LOG_FILTER="debug"))
            try:
                deadline = time.monotonic() + min(self.args.bot_timeout_seconds, 1100)
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError("live bot exited before expected finalization")
                    try:
                        try:
                            response = opener.open(f"http://127.0.0.1:{port}/ready", timeout=2)
                        except urllib.error.HTTPError as error:
                            response = error
                        with response:
                            health = json.load(response)
                        if on_health is not None:
                            on_health(health)
                        if finalized_health(health, expected):
                            observed = health
                            break
                    except (OSError, ValueError):
                        pass
                    time.sleep(.5)
                if observed is None:
                    raise RuntimeError("live scenario did not finalize within its time budget")
            except BaseException as error:
                failure = error
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                try:
                    code = process.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    process.kill(); process.wait()
                    code = process.returncode
                    failure = RuntimeError("live bot exceeded shutdown grace")
        run = {"argv": argv, "exit_code": code, "stdout": str(stdout_path), "stderr": str(stderr_path),
               "stdout_sha256": sha(stdout_path.read_bytes()), "stderr_sha256": sha(stderr_path.read_bytes())}
        self.state.setdefault("bot_runs", []).append(run)
        self.save()
        events = self.bot_events(stdout_path)
        stopped = [event for event in events if event.get("event") == "stopped"]
        after = self.ctl("inspect", actor="operator")
        valid = (failure is None and code == 0 and len(stopped) == 1
                 and stopped[0].get("active_pending") is False
                 and stopped[0].get("finalized_liquidations") == expected
                 and after["latest_nonce"] == after["pending_nonce"])
        record.update(passed=valid, operator_after=after, health=observed, run=run,
                      stopped=stopped, reason=None if failure is None else str(failure))
        atomic_json(result_path, record)
        if failure is not None:
            raise failure
        if not valid:
            raise RuntimeError("live scenario finalization/shutdown/nonce assertions failed")
        return record

    def guard_case(self, label, manifest, oracle_status, eligible, reason=None):
        cases = self.state.setdefault("guard_cases", {})
        old = cases.get(label, {})
        if old.get("passed"):
            if not reason or any(event.get("reason") == reason for event in old.get("skip_events", [])):
                return
            self.state.setdefault("guard_history", {}).setdefault(label, []).append(old)
            del cases[label]
            self.save()
        manifest = json.loads(json.dumps(manifest))
        # One genuine full candidate makes financial diagnostics attributable, avoiding a final dust quote hiding the tested guard.
        manifest["planner"].update(max_candidates=1, partial_divisors=[])
        # This fixture has one collateral/asset pool. Isolate the financial
        # guard from later unavailable two-hop hints; route fallback is tested
        # independently with multiple real pools.
        manifest["v3"]["intermediates"] = []
        path = self.directory / f"guard-{label}-manifest.json"
        atomic_json(path, manifest)
        before = self.ctl("inspect", actor="operator")
        report = self.bot_run("guard-" + label, manifest=path)
        after = self.ctl("inspect", actor="operator")
        health = report["health"]
        observed = health["pools"]
        if len(observed) != 1 or observed[0].get("verified") is not True or (observed[0].get("debt_positive") or 0) < 1:
            raise RuntimeError("guard lacks an actual verified nonempty debt census")
        eligible_seen = (observed[0].get("eligible") or 0) > 0
        checkpoint_evidence = None
        if oracle_status == "future_timestamp":
            eligible_seen = bool(self.scalar(self.context["credit_pool"], "isLiquidatable(uint64,address)",
                                             [self.state["scenario_market"], self.context["borrower"]]))
        if oracle_status == "stale":
            if observed[0].get("oracle_failure") != "stale":
                raise RuntimeError("stale guard lacks exact SDK stale failure")
            if observed[0].get("eligible") is not None or observed[0].get("oracle_status") != "unavailable":
                raise RuntimeError("stale guard lacks unavailable oracle/risk evidence")
        checkpoint_evidence = {"as_of": observed[0].get("as_of"), "oracle_status": observed[0].get("oracle_status"),
                               "oracle_failure": observed[0].get("oracle_failure"), "source": "bounded RAM health plus actual protocol getters"}
        expected_health_status = "unavailable" if oracle_status == "stale" else oracle_status
        if observed[0]["oracle_status"] != expected_health_status or (eligible and not eligible_seen):
            raise RuntimeError("guard oracle/eligibility precondition was not observed")
        if before["latest_nonce"] != after["latest_nonce"] or before["pending_nonce"] != after["pending_nonce"] or before["balance_wei"] != after["balance_wei"]:
            raise RuntimeError("no-send guard changed operator nonce/native balance")
        if report.get("finalized_liquidations") != 0 or report.get("active_pending") is not False:
            raise RuntimeError("no-send guard produced a signed pending/terminal outcome")
        log = pathlib.Path(self.state["bot_runs"][-1]["stdout"])
        events = self.bot_events(log)
        skips = [event for event in events if event.get("event") == "position_skipped"]
        if eligible and not skips:
            raise RuntimeError("eligible guard did not reach the actual preparation/skip path")
        if reason and not any(event.get("reason") == reason for event in skips):
            raise RuntimeError("guard did not emit the expected concrete rejection reason")
        if not eligible and oracle_status != "future_timestamp" and (observed[0].get("risk_unavailable") or 0) < 1:
            raise RuntimeError("oracle guard lacks an actual unavailable-risk observation")
        cases[label] = {"passed": True, "manifest_sha256": sha(path.read_bytes()), "operator_nonce": after["latest_nonce"],
                        "operator_balance_wei": after["balance_wei"], "health": health, "stopped": report, "skip_events": skips,
                        "state_observation": checkpoint_evidence}
        self.save()
        print(json.dumps({"guard": label, "passed": True, "operator_nonce": after["latest_nonce"]}), flush=True)

    def guards(self):
        if not self.state.get("positions_passed"):
            raise RuntimeError("guards require actual Provided matching and healthy no-send")
        fee_evidence = self.state.get("guard_cases", {}).get("fee-cap", {})
        strict_fee = any(event.get("reason") == "gas_price_cap_exceeded" for event in fee_evidence.get("skip_events", []))
        if self.state.get("guards_passed") and strict_fee:
            print(json.dumps({"phase": "guards", "status": "already_passed"}))
            return
        manifest = json.loads((self.directory / "bot-manifest.json").read_text())
        price = [str(4 * WAD // 10), str(WAD)]
        self.ensure_oracle("guards-eligible-price", *price)
        variants = [("fee-cap", {"max_gas_price": "0x1"}, None, "gas_price_cap_exceeded"),
                    ("native-reserve-insufficient", {"native_reserve": hex(2 * WAD)}, None, "insufficient_native"),
                    ("unprofitable", {}, hex(10**6 * WAD), "unprofitable_route")]
        for label, fees, profit, reason in variants:
            variant = json.loads(json.dumps(manifest))
            variant["fees"].update(fees)
            if profit:
                variant["additional_profit_asset"] = profit
            self.guard_case(label, variant, "ready", True, reason)
        if "guard_future_timestamp" not in self.state:
            self.state["guard_future_timestamp"] = self.ctl("inspect")["latest_timestamp"] + 10000
            self.save()
        observations = [("stale-oracle", price + ["0"], "stale", False),
                        ("future-oracle", price + [str(self.state["guard_future_timestamp"])], "future_timestamp", True)]
        for label, values, expected, eligible in observations:
            if self.state.get("guard_cases", {}).get(label, {}).get("passed"):
                continue
            self.ensure_oracle("guards-" + label, *values)
            self.guard_case(label, manifest, expected, eligible)
        self.ensure_oracle("guards-fresh-eligible-price", *price)
        self.state["guards_passed"] = True
        self.state["phase"] = "guards_passed_ready_for_fault_liquidation"
        self.save()
        print(json.dumps({"phase": "guards", "passed": len(self.state["guard_cases"]), "primary_liquidation": "not_started"}))

    def router_swap(self, step_id, token_in, token_out, amount):
        entry = next(item for item in self.contracts["SwapRouter02"]["abi"] if item.get("name") == "exactInputSingle" and len(item["inputs"]) == 1)
        arguments = [token_in, token_out, 3000, self.args.owner, amount, 0, 0]
        signature = "exactInputSingle(" + abi_type(entry["inputs"][0]) + ")"
        encoded = self.cast("calldata", signature, abi_arg(entry["inputs"][0], arguments))
        plan = {"id": step_id, "to": self.context["router"], "input": encoded, "value": "0x0", "max_gas": 30_000_000,
                "before": [], "after": [], "events": []}
        path = self.directory / "plans" / f"{step_id}.json"
        if path.exists() and json.loads(path.read_text()) != plan:
            raise RuntimeError("persisted AMM swap changed")
        atomic_json(path, plan)
        if step_id not in self.state["steps"]:
            simulated = self.ctl("simulate", "--plan", path)
            if simulated.get("simulation_passed") is not True:
                raise RuntimeError("AMM swap simulation failed")
        return self.run_step(step_id, plan, ["send", "--plan", path])

    def quote_input(self, token_in, token_out, amount):
        path = token_in + format(3000, "06x") + token_out[2:]
        data = self.call(self.context["quoter"], "quoteExactInput(bytes,uint256)", [path, amount])
        if not re.fullmatch(r"0x[0-9a-fA-F]+", data) or len(data) < 66:
            raise RuntimeError("actual V3 quote is malformed")
        return int(data[2:66], 16)

    def amm_guard(self):
        if not self.state.get("guards_passed"):
            raise RuntimeError("AMM guard requires completed no-send guards")
        negative_case = self.state.get("guard_cases", {}).get("actual-amm-unprofitable", {})
        strict_negative = any(event.get("reason") == "unprofitable_route" for event in negative_case.get("skip_events", []))
        if self.state.get("amm_guard_passed"):
            if not strict_negative:
                raise RuntimeError("completed restored AMM trial lacks a strict economic rejection reason")
            print(json.dumps({"phase": "amm-guard", "status": "already_passed"}))
            return
        if negative_case.get("passed") and not strict_negative and self.state["steps"].get("amm-recovery-swap", {}).get("status") in ["passed", "uncertain"]:
            raise RuntimeError("cannot repeat a no-send loss guard after liquidity was restored; preserve trial and use a separate scenario")
        manifest = json.loads((self.directory / "bot-manifest.json").read_text())
        self.ensure_oracle("amm-initial-fresh-eligible-price", 4 * WAD // 10, WAD)
        record = self.state.setdefault("amm_guard", {})
        if "asset_balance_before" not in record:
            record["asset_balance_before"] = self.scalar(self.context["asset"], "balanceOf(address)", [self.args.owner])
            record["original_quote_all_collateral"] = self.quote_input(self.context["collateral"], self.context["asset"], 400 * WAD)
            self.save()
        self.send("amm-loss-collateral-approve", self.context["collateral"], "approve(address,uint256)", [self.context["router"], 10**6 * WAD])
        self.router_swap("amm-loss-swap", self.context["collateral"], self.context["asset"], 10**6 * WAD)
        if "restore_amount_asset" not in record:
            record["restore_amount_asset"] = self.scalar(self.context["asset"], "balanceOf(address)", [self.args.owner]) - record["asset_balance_before"]
            self.save()
        if record["restore_amount_asset"] <= 0:
            raise RuntimeError("actual AMM loss swap produced no observed asset delta")
        negative_passed = negative_case.get("passed") and strict_negative
        if not negative_passed:
            debt = self.scalar(self.context["credit_pool"], "debtOf(uint64,address)", [self.state["scenario_market"], self.context["borrower"]])
            quoted = self.quote_input(self.context["collateral"], self.context["asset"], 400 * WAD)
            if debt != 100 * WAD or quoted >= debt:
                raise RuntimeError("AMM guard is not genuinely unprofitable before gas even for all collateral")
            record["loss_quote_all_collateral"] = quoted
            record["actual_full_debt"] = debt
            self.save()
            self.guard_case("actual-amm-unprofitable", manifest, "ready", True, "unprofitable_route")
        elif not 0 < record.get("loss_quote_all_collateral", 0) < record.get("actual_full_debt", 0):
            raise RuntimeError("completed AMM guard lacks its immutable actual loss quote/debt evidence")
        debt = record["actual_full_debt"]
        self.send("amm-recovery-asset-approve", self.context["asset"], "approve(address,uint256)", [self.context["router"], record["restore_amount_asset"]])
        self.router_swap("amm-recovery-swap", self.context["asset"], self.context["collateral"], record["restore_amount_asset"])
        restored = self.quote_input(self.context["collateral"], self.context["asset"], 400 * WAD)
        if restored <= debt:
            raise RuntimeError("actual reverse AMM swap did not restore usable unwind liquidity")
        record["restored_quote_all_collateral"] = restored
        self.ensure_oracle("amm-final-fresh-eligible-price", 4 * WAD // 10, WAD)
        self.state["amm_guard_passed"] = True
        self.save()
        print(json.dumps({"phase": "amm-guard", "status": "passed", "economic_loss_before_gas": True, "primary_liquidation": "not_started"}))

    def bot_journal(self):
        # Test-runner evidence only. The production bot has no filesystem journal.
        manifest=json.loads((self.directory/"bot-manifest.json").read_text())
        outcomes={};last=None
        for run in self.state.get("bot_runs",[]):
            if sha(pathlib.Path(run["stdout"]).read_bytes()) != run["stdout_sha256"]:
                raise RuntimeError("bot stdout evidence changed after run")
            events=self.bot_events(run["stdout"])
            for event in events:
                if event.get("event")=="stopped": last=event
                if event.get("event")!="finalized": continue
                outcome=event.get("outcome")
                if not outcome: raise RuntimeError("terminal event lacks public financial evidence")
                identity=outcome["identity"]
                if (identity["chain"]!=manifest["chain"] or identity["signer"].lower()!=self.context["operator"].lower()
                    or identity["executor"].lower()!=self.context["executor"].lower()
                    or identity["executor_code_hash"].lower()!=manifest["executor_code_hash"].lower()):
                    raise RuntimeError("terminal evidence identity differs from deployment")
                hash_=outcome["attempt"]["hash"]
                if hash_ in outcomes and outcomes[hash_]!=outcome: raise RuntimeError("conflicting terminal evidence")
                outcomes[hash_]=outcome
        if last is None or last.get("active_pending") is not False:
            raise RuntimeError("last run has no confirmed idle shutdown; inspect on-chain pending nonce")
        journal={"active":None,"outcomes":sorted(outcomes.values(),key=lambda o:o["attempt"]["envelope"]["nonce"])}
        path=self.directory/"BOT_OUTCOMES.json"
        atomic_json(path,journal)
        return path,journal

    def conservative_native_cost(self, amount):
        reverse_path = self.context["wrapped_native"] + format(3000, "06x") + self.context["asset"][2:]
        data = self.call(self.context["quoter"], "quoteExactOutput(bytes,uint256)", [reverse_path, amount])
        if not re.fullmatch(r"0x[0-9a-fA-F]+", data) or len(data) < 66:
            raise RuntimeError("actual native-cost quote is malformed")
        quoted = int(data[2:66], 16)
        if quoted <= 0:
            raise RuntimeError("actual native-cost quote must be positive")
        margin = (quoted * 10100 + 9999) // 10000
        return (margin * 10100 + 9999) // 10000

    def paid_cancelled_outcome(self, outcome, matching_metas):
        attempt, receipt = outcome["attempt"], outcome["receipt"]
        envelope = attempt["envelope"]
        originals = [item for item in outcome.get("attempts", []) if isinstance(item["kind"], dict) and "Liquidation" in item["kind"]]
        if (outcome["kind"] != "Cancelled" or attempt["kind"] != "Cancel" or not originals
                or any(item["kind"]["Liquidation"] not in matching_metas for item in originals)
                or any(item["envelope"]["nonce"] != envelope["nonce"] for item in outcome.get("attempts", []))
                or envelope["to"].lower() != self.context["operator"].lower() or envelope["input_hash"] != "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
                or envelope["chain_id"] != self.binding["chain_id"] or envelope["gas_limit"] != 21000
                or not receipt["succeeded"] or receipt["hash"].lower() != attempt["hash"].lower()
                or int(receipt["effective_gas_price"], 0) != int(envelope["gas_price"], 0)
                or receipt["gas_used"] != 21000 or receipt["logs"] or int(receipt["block_hash"], 16) == 0
                or outcome["asset_gain"] is not None or outcome["net_asset"] is not None):
            raise RuntimeError("paid prior outcome is not a valid same-nonce primary self cancellation")
        amount = int(outcome["native_charge"], 0)
        if amount <= 0 or amount != envelope["gas_limit"] * int(envelope["gas_price"], 0):
            raise RuntimeError("cancelled native charge differs from signed gasLimit bound")
        return {"kind": "Cancelled", "hash": attempt["hash"], "nonce": envelope["nonce"],
                "to": envelope["to"], "value": "0x0", "gas_limit": envelope["gas_limit"], "gas_price": envelope["gas_price"],
                "native_charge": hex(amount), "receipt": receipt,
                "related_liquidation": {"pool": self.context["credit_pool"], "market": self.state["scenario_market"], "borrower": self.context["borrower"]},
                "original_attempt_hashes": [item["hash"] for item in originals]}

    def outcome(self):
        path, journal = self.bot_journal()
        if journal["active"] is not None:
            raise RuntimeError("signed bot transaction is unresolved; finish reconciliation first")
        matching, prior_paid = [], []
        for outcome in journal["outcomes"]:
            kinds = [attempt["kind"] for attempt in [outcome["attempt"], *outcome.get("attempts", [])]]
            metas = [kind["Liquidation"] for kind in kinds if isinstance(kind, dict) and "Liquidation" in kind]
            matching_metas = [meta for meta in metas if meta["key"]["pool"].lower() == self.context["credit_pool"].lower()
                              and meta["key"]["borrower"].lower() == self.context["borrower"].lower()
                              and meta["key"]["market"] == self.state["scenario_market"] and meta["asset"].lower() == self.context["asset"].lower()]
            if not matching_metas:
                raise RuntimeError("isolated operator archive contains an unrelated paid outcome")
            chosen_kind = outcome["attempt"]["kind"]
            if outcome["kind"] == "Cancelled":
                if matching:
                    raise RuntimeError("paid cancellation follows the primary liquidation")
                prior_paid.append(self.paid_cancelled_outcome(outcome, matching_metas))
                continue
            if outcome["kind"] != "Liquidated" or not isinstance(chosen_kind, dict) or "Liquidation" not in chosen_kind:
                raise RuntimeError("primary borrower has an unsupported paid non-liquidated outcome")
            meta = chosen_kind["Liquidation"]
            if meta not in matching_metas:
                raise RuntimeError("chosen archived liquidation differs from the primary actual pool/asset")
            amount = int(outcome["native_charge"], 0)
            envelope = outcome["attempt"]["envelope"]
            if amount <= 0 or amount != envelope["gas_limit"] * int(envelope["gas_price"], 0):
                raise RuntimeError("actual archived native charge differs from signed gasLimit bound")
            conservative_cost = self.conservative_native_cost(amount)
            gain = int(outcome["asset_gain"], 0)
            guard = int(meta["min_asset_profit"], 0)
            if gain < guard or guard < conservative_cost:
                raise RuntimeError("actual gained asset does not cover signed guard/current conservative native route")
            matching.append({"hash": outcome["attempt"]["hash"], "nonce": envelope["nonce"], "receipt": outcome["receipt"],
                             "asset_gain": hex(gain), "signed_min_profit": hex(guard), "native_charge": hex(amount),
                             "current_conservative_native_cost_asset": hex(conservative_cost),
                             "asset_net_at_current_native_route": hex(gain - conservative_cost), "plan_meta": meta})
        if (len(matching) != 1 or any(item["nonce"] != index for index, item in enumerate(prior_paid))
                or matching[0]["nonce"] != len(prior_paid)
                or any(item["receipt"]["block_number"] > matching[0]["receipt"]["block_number"] for item in prior_paid)):
            raise RuntimeError("isolated operator requires prior cancel nonces 0..N-1 and exactly one liquidation nonce N")
        debt = self.scalar(self.context["credit_pool"], "debtOf(uint64,address)", [self.state["scenario_market"], self.context["borrower"]])
        balance = self.scalar(self.context["asset"], "balanceOf(address)", [self.context["executor"]])
        gain = sum(int(item["asset_gain"], 0) for item in matching)
        native_charge = sum(int(item["native_charge"], 0) for item in [*prior_paid, *matching])
        aggregate_cost = self.conservative_native_cost(native_charge) if prior_paid else int(matching[0]["current_conservative_native_cost_asset"], 0)
        if debt != 0 or balance < gain or gain < aggregate_cost:
            raise RuntimeError("actual debt/profit custody or aggregate paid native economics does not match finalized bot archive")
        report = {"passed": True, "journal_path": str(path), "journal_sha256": sha(path.read_bytes()), "outcomes": matching,
                  "prior_paid_outcomes": prior_paid, "aggregate_actual_native_charge": hex(native_charge),
                  "current_conservative_all_native_cost_asset": hex(aggregate_cost),
                  "asset_net_after_all_bot_costs": hex(gain - aggregate_cost),
                  "actual_debt": "0x0", "executor_asset_balance": hex(balance), "raw_transactions_copied": False,
                  "native_cost_basis": "actual current V3 quote for ALL paid native charges; signed success guard retained separately",
                  "synthetic_asset_not_mainnet_pnl": True}
        atomic_json(self.directory / "LIQUIDATION_OUTCOME.json", report)
        self.state["primary_liquidation_passed"] = True
        self.state["outcome_report_sha256"] = sha((self.directory / "LIQUIDATION_OUTCOME.json").read_bytes())
        self.save()
        print(json.dumps({"phase": "outcome", "status": "passed", "finalized_outcomes": len(matching),
                          "prior_paid_cancellations": len(prior_paid), "actual_debt": "0x0"}))

    def wait_for_market(self):
        deadline = time.monotonic() + self.args.market_wait_seconds
        market = self.state["scenario_market"]
        while True:
            now = self.ctl("inspect")["latest_timestamp"]
            if now >= market:
                return True
            print(json.dumps({"cleanup": "lender_claim_pending", "actual_head_timestamp": now, "market": market,
                              "seconds_until_market": market - now}), flush=True)
            if not self.args.wait_market or time.monotonic() >= deadline:
                return False
            time.sleep(min(30, market - now, max(0, deadline - time.monotonic())))

    def cleanup(self):
        if not self.state.get("primary_liquidation_passed"):
            raise RuntimeError("cleanup requires verified finalized liquidation/profit before removing AMM liquidity")
        report = self.directory / "LIQUIDATION_OUTCOME.json"
        if not report.is_file() or sha(report.read_bytes()) != self.state.get("outcome_report_sha256"):
            raise RuntimeError("accepted pre-cleanup liquidation report changed")
        if self.bot_journal()[1]["active"] is not None:
            raise RuntimeError("cleanup cannot run while signed bot reconciliation is unresolved")
        if self.state.get("cleanup_passed"):
            print(json.dumps({"phase": "cleanup", "status": "already_passed"}))
            return
        market = self.state["scenario_market"]
        pool, owner, borrower, lender = self.context["credit_pool"], self.args.owner, self.context["borrower"], self.context["lender"]
        if self.scalar(pool, "debtOf(uint64,address)", [market, borrower]) != 0:
            raise RuntimeError("cleanup primary borrower debt is not zero")
        amounts = self.state.setdefault("cleanup_amounts", {})
        if "profit_asset" not in amounts:
            amounts["profit_asset"] = self.scalar(self.context["asset"], "balanceOf(address)", [self.context["executor"]])
            amounts["borrower_collateral"] = self.scalar(pool, "collateralOf(uint64,address)", [market, borrower])
            self.save()
        if amounts["profit_asset"]:
            self.send("cleanup-owner-profit", self.context["executor"], "withdraw(address,address,uint256)",
                      [self.context["asset"], owner, amounts["profit_asset"]],
                      after=[self.assertion(self.context["asset"], "balanceOf(address)", [self.context["executor"]], "uint256", 0)])
        if amounts["borrower_collateral"]:
            self.send("cleanup-borrower-collateral", pool, "withdrawCollateral(address,uint64,address,uint128)",
                      [borrower, market, borrower, amounts["borrower_collateral"]], actor="borrower",
                      after=[self.assertion(pool, "collateralOf(uint64,address)", [market, borrower], "uint128", 0)])
        if not self.wait_for_market():
            self.state["phase"] = "cleanup_pending_actual_market_maturity"
            self.save()
            print(json.dumps({"phase": "cleanup", "status": "pending_market", "market": market, "no_early_claim_sent": True}))
            return
        if "lender_loan" not in amounts:
            amounts["lender_loan"] = self.scalar(pool, "loanOf(uint64,address)", [market, lender])
            amounts["lender_idle_balance"] = self.scalar(pool, "balanceOf(address)", [lender])
            self.save()
        if amounts["lender_loan"]:
            if "cleanup-lender-claim" not in self.state["steps"] and self.scalar(pool, "getMarketBalance(uint64)", [market]) < amounts["lender_loan"]:
                raise RuntimeError("finalized market lacks actual available lender loan; no blind claim sent")
            self.send("cleanup-lender-claim", pool, "claim(address,uint64,uint128)", [lender, market, amounts["lender_loan"]], actor="lender",
                      after=[self.assertion(pool, "loanOf(uint64,address)", [market, lender], "uint128", 0)])
        lender_withdraw = amounts["lender_loan"] + amounts["lender_idle_balance"]
        if lender_withdraw:
            self.send("cleanup-lender-withdraw", pool, "withdraw(address,address,uint128)", [lender, lender, lender_withdraw], actor="lender",
                      after=[self.assertion(pool, "balanceOf(address)", [lender], "uint128", 0)])
        for label, liquidity in [("collateral_asset", 10**6 * WAD), ("asset_native", 32 * WAD)]:
            self.send("cleanup-lp-" + label, self.context["bootstrap"], "removeLiquidity(address,int24,int24,uint128)",
                      [self.state["v3_pools"][label], -887220, 887220, liquidity],
                      after=[self.assertion(self.state["v3_pools"][label], "liquidity()", [], "uint128", 0)])
        if "wrapped_native" not in amounts:
            amounts["wrapped_native"] = self.scalar(self.context["wrapped_native"], "balanceOf(address)", [owner])
            self.save()
        if amounts["wrapped_native"]:
            self.send("cleanup-native-unwrap", self.context["wrapped_native"], "withdraw(uint256)", [amounts["wrapped_native"]],
                      after=[self.assertion(self.context["wrapped_native"], "balanceOf(address)", [owner], "uint256", 0)])
        actual = {"borrower_debt": self.scalar(pool, "debtOf(uint64,address)", [market, borrower]),
                  "borrower_collateral": self.scalar(pool, "collateralOf(uint64,address)", [market, borrower]),
                  "lender_loan": self.scalar(pool, "loanOf(uint64,address)", [market, lender]),
                  "lender_idle_balance": self.scalar(pool, "balanceOf(address)", [lender]),
                  "executor_profit_asset": self.scalar(self.context["asset"], "balanceOf(address)", [self.context["executor"]]),
                  "owner_wrapped_native": self.scalar(self.context["wrapped_native"], "balanceOf(address)", [owner]),
                  "collateral_asset_lp_liquidity": self.scalar(self.state["v3_pools"]["collateral_asset"], "liquidity()"),
                  "asset_native_lp_liquidity": self.scalar(self.state["v3_pools"]["asset_native"], "liquidity()")}
        if any(actual.values()):
            raise RuntimeError("actual post-cleanup debt/custody balances are not closed")
        self.state["cleanup_passed"] = True
        self.state["cleanup_actual"] = actual
        self.state["phase"] = "cleanup_passed"
        self.save()
        print(json.dumps({"phase": "cleanup", "status": "passed", "claimed_after_actual_market": market,
                          "actor_native_balances": "retained in isolated wallets; no automatic refund"}))


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=["plan", "deploy", "status", "positions", "guards", "amm-guard", "oracle-refresh", "liquidate", "outcome", "cleanup"], default="plan")
    parser.add_argument("--execute", action="store_true", help="Explicit public testnet deployment opt-in; chain 10143 only")
    parser.add_argument("--rpc", default="https://testnet-rpc.monad.xyz")
    parser.add_argument("--rpc-fallback-url", dest="rpc_fallback_urls", action="append", default=[])
    parser.add_argument("--genesis", required=True)
    parser.add_argument("--owner", required=True)
    parser.add_argument("--owner-start-nonce", type=int, default=0, help="Observed latest/pending nonce for this fresh experiment")
    parser.add_argument("--owner-key-file", type=pathlib.Path)
    parser.add_argument("--artifacts", type=pathlib.Path, required=True, help="Offline bytecode bundle from build-testnet-artifacts.py")
    parser.add_argument("--template", type=pathlib.Path, default=REPO / "tests/fixtures/testnet/deployment-template.json")
    parser.add_argument("--evidence-dir", type=pathlib.Path, required=True)
    parser.add_argument("--secret-dir", type=pathlib.Path, required=True)
    parser.add_argument("--ctl", type=pathlib.Path, default=REPO / "target/debug/testnet_ctl")
    parser.add_argument("--cast", type=pathlib.Path, required=True)
    parser.add_argument("--bot", type=pathlib.Path, default=REPO / "target/debug/barterlend-liquidator")
    parser.add_argument("--bot-cycles", type=int, default=5)
    parser.add_argument("--bot-timeout-seconds", type=int, default=600)
    parser.add_argument("--live-label", default="primary-liquidation", help="Fresh evidence label after inspecting a previous failed attempt")
    parser.add_argument("--oracle-numerator", type=lambda value: int(value, 0), default=41 * WAD // 100)
    parser.add_argument("--oracle-denominator", type=lambda value: int(value, 0), default=WAD)
    parser.add_argument("--wait-market", action="store_true")
    parser.add_argument("--market-wait-seconds", type=int, default=600)
    parser.add_argument("--budget-wei", type=lambda value: int(value, 0), default=15 * WAD)
    parser.add_argument("--owner-reserve-wei", type=lambda value: int(value, 0), default=WAD // 5)
    parser.add_argument("--actor-reserve-wei", type=lambda value: int(value, 0), default=WAD // 1000)
    parser.add_argument("--max-gas-price-wei", type=lambda value: int(value, 0), default=150_000_000_000)
    parser.add_argument("--local-rehearsal", action="store_true")
    parser.add_argument("--testnet-proxy", action="store_true")
    args = parser.parse_args()
    if len(args.rpc_fallback_urls) > 7 or len(set([args.rpc, *args.rpc_fallback_urls])) != 1 + len(args.rpc_fallback_urls):
        parser.error("at most seven distinct fallback URLs are supported")
    if not re.fullmatch(r"0x[0-9a-fA-F]{64}", args.genesis) or int(args.genesis, 16) == 0 or not re.fullmatch(r"0x[0-9a-fA-F]{40}", args.owner):
        parser.error("nonzero pinned genesis and EVM owner required")
    if args.local_rehearsal and args.testnet_proxy:
        parser.error("local-rehearsal and testnet-proxy are mutually exclusive")
    url = urllib.parse.urlsplit(args.rpc)
    loopback = url.scheme == "http" and url.hostname in ["127.0.0.1", "::1"]
    public = url.scheme == "https" and url.hostname is not None and "." in url.hostname and not url.hostname.endswith((".local", ".localhost"))
    if url.username or url.password or url.query or url.fragment or not (loopback if args.local_rehearsal or args.testnet_proxy else public):
        parser.error("credential-free HTTPS or explicit IP-loopback profile required")
    if not 0 < args.budget_wei <= 15 * WAD or not 0 < args.max_gas_price_wei <= 150_000_000_000:
        parser.error("test budget/gas cap outside accepted bounds")
    if not 0 < args.oracle_numerator < 2**256 or not 0 < args.oracle_denominator < 2**256:
        parser.error("controlled oracle fraction must contain two positive uint256 values")
    if args.phase in ["deploy", "positions", "guards", "amm-guard", "oracle-refresh", "liquidate", "cleanup"] and (not args.execute or args.owner_key_file is None or not args.owner_key_file.is_absolute()):
        parser.error("mutating phases require --execute and absolute owner-key-file")
    if not 1 <= args.bot_cycles <= 1000 or not 1 <= args.bot_timeout_seconds <= 1200:
        parser.error("bounded bot cycles/time required")
    if not 0 <= args.market_wait_seconds <= 1200:
        parser.error("market wait must be bounded by 1200 real seconds")
    if not 0 <= args.owner_start_nonce < 2**64:
        parser.error("owner-start-nonce must be uint64")
    args.secret_dir = args.secret_dir.absolute()
    return args


def main():
    args = parse_args()
    runner = Runner(args)
    lock_path = runner.directory / "orchestrator.lock"
    with lock_path.open("a+") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        if args.phase == "plan":
            runner.preview()
        elif args.phase == "status":
            print(json.dumps(runner.state, ensure_ascii=False))
        elif args.phase == "positions":
            runner.positions()
        elif args.phase == "guards":
            runner.guards()
        elif args.phase == "amm-guard":
            runner.amm_guard()
        elif args.phase == "oracle-refresh":
            runner.oracle_refresh()
        elif args.phase == "liquidate":
            if not runner.state.get("guards_passed"):
                raise RuntimeError("primary live liquidation requires completed no-send guards")
            runner.oracle_refresh()
            runner.live_until(args.live_label)
            runner.outcome()
        elif args.phase == "outcome":
            runner.outcome()
        elif args.phase == "cleanup":
            runner.cleanup()
        else:
            runner.deploy()
            print(json.dumps({"phase": "deployed", "manifest": str(runner.directory / "bot-manifest.json"), "mainnet": "forbidden"}))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"status": "blocked", "reason": str(error), "signed_journals": "retained outside repository"}), file=sys.stderr)
        sys.exit(1)
