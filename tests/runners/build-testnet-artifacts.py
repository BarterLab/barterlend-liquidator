#!/usr/bin/env python3
"""Offline reproducible creation-bytecode bundle; never signs, uses RPC, or reads secrets."""
import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess

REPO = pathlib.Path(__file__).resolve().parents[2]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(condition, reason):
    if not condition:
        raise RuntimeError(reason)


def input_hashes():
    paths = set((REPO / "tests/testnet").glob("*.sol"))
    require(len(paths)==3, "Expected all three test-stack Solidity inputs")
    paths.update([REPO / "tests/fixtures/factory/TestLendingFactory.sol", REPO / "contracts/KyberRoute.sol", REPO / "contracts/LiquidationExecutor.sol", REPO / "contracts/IExecutorV4.sol", pathlib.Path(__file__).resolve(), REPO / "foundry.toml"])
    fixtures = REPO / "tests/fixtures/solidity"
    for name in ["PROTOCOL_INPUTS.json", "SUPPLEMENTAL_INPUTS.json"]:
        manifest_path = fixtures / name
        paths.add(manifest_path)
        for item in json.loads(manifest_path.read_text())["files"]:
            path = fixtures / "protocol-v5" / item["path"]
            require(digest(path) == item["sha256"], "Pinned input changed: " + str(path))
            paths.add(path)
    v3_manifest = REPO / "tests/fixtures/uniswap-v3/MANIFEST.json"
    paths.add(v3_manifest)
    for package in json.loads(v3_manifest.read_text()):
        for item in package["artifacts"]:
            path = v3_manifest.parent / item["file"]
            require(digest(path) == item["sha256"], "Pinned V3 changed: " + str(path))
            paths.add(path)
    return {str(path.relative_to(REPO)): digest(path) for path in sorted(paths)}


def permit_artifact():
    source = REPO / "tests/fixtures/solidity/protocol-v5/lib/permit2/test/utils/DeployPermit2.sol"
    matches = re.findall(r'hex"([0-9a-fA-F]+)"', source.read_text())
    require(len(matches) == 1, "Pinned Permit2 runtime must have exactly one hex literal")
    runtime = bytes.fromhex(matches[0])
    require(0 < len(runtime) < 65536, "Permit2 runtime length")
    # PUSH2 length; DUP1; PUSH1 offset10; RETURNDATASIZE; CODECOPY; RETURNDATASIZE; RETURN.
    creation = b"\x61" + len(runtime).to_bytes(2, "big") + bytes.fromhex("80600a3d393df3") + runtime
    return {
        "bytecode": "0x" + creation.hex(), "deployedBytecode": "0x" + runtime.hex(),
        "abi": [{"type": "function", "name": "DOMAIN_SEPARATOR", "inputs": [], "outputs": [{"name": "", "type": "bytes32"}], "stateMutability": "view"}],
        "source": str(source.relative_to(REPO)), "source_sha256": digest(source),
        "provenance": "Exact pinned DeployPermit2 runtime, constructor wrapper only. Cached separator is chain31337/canonicaladdress; chain10143 uses dynamic chain/address separator, covered locally.",
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--forge", default=os.environ.get("FORGE", "forge"))
    parser.add_argument("--solc", default=os.environ.get("SOLC", "solc"))
    parser.add_argument("--permit-only", action="store_true")
    args = parser.parse_args()
    output = pathlib.Path(args.output_dir).expanduser().resolve()
    for immutable in [REPO / "tests/fixtures", REPO / "fixtures"]:
        require(output != immutable and immutable not in output.parents, "Immutable fixture output is forbidden")
    require(output != REPO, "Repository root output is forbidden")
    require(not output.exists(), "Choose a new output directory; overwriting evidence is forbidden")
    inside_git = any((p / ".git").exists() for p in [output, *output.parents])
    require(not inside_git, "build output must be outside Git repositories")
    output.mkdir(parents=True)
    permit = permit_artifact()
    (output / "Permit2.json").write_text(json.dumps(permit, indent=2) + "\n")
    if args.permit_only:
        print("Pinned Permit2 creation wrapper prepared offline")
        return
    before_hashes = input_hashes()
    forge_version = subprocess.run([args.forge, "--version"], check=True, text=True, stdout=subprocess.PIPE).stdout
    solc_version = subprocess.run([args.solc, "--version"], check=True, text=True, stdout=subprocess.PIPE).stdout
    require("Version: 1.8.4" in forge_version and "Version: 0.8.31+" in solc_version, "Pinned toolchain required")
    compile_out = output / "solidity-out"
    compile_cache = output / "solidity-cache"
    command = [args.forge, "build", "tests/testnet/ProtocolDeployables.sol", "--skip", "test", "--skip", "script", "--root", str(REPO), "--use", args.solc, "--offline", "--out", str(compile_out), "--cache-path", str(compile_cache)]
    result = subprocess.run(command, cwd=REPO, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (output / "build.log").write_text(result.stdout)
    require(result.returncode == 0, "Offline build failed; see build.log")
    names = ["TestLendingFactory", "TestnetToken", "TestnetWrappedMON", "TestnetOracle", "TestnetAaveReceipt", "TestnetAavePool", "TestnetV3Bootstrap", "CreditPoolFacet1", "CreditPoolFacet2", "CreditPoolFacet3", "CreditPoolFacet4", "CreditPool", "AaveV3Adapter", "LiquidationExecutor"]
    contracts = {"Permit2": permit}
    for name in names:
        candidates = list(compile_out.glob("*/" + name + ".json"))
        require(len(candidates) == 1, "Artifact ambiguity: " + name)
        path = candidates[0]
        original = json.loads(path.read_text())
        settings = original["metadata"]["settings"]
        require(not settings.get("viaIR", False) and settings["evmVersion"] == "osaka"
                and settings["optimizer"] == {"enabled": True, "runs": 200},
                "Unexpected compiler settings: " + name)
        for kind in ["bytecode", "deployedBytecode"]:
            require(original[kind].get("linkReferences", {}) == {}, "Unlinked artifact: " + name)
        contracts[name] = {"abi": original["abi"], "bytecode": original["bytecode"]["object"], "deployedBytecode": original["deployedBytecode"]["object"], "artifact_sha256": digest(path), "artifact_path": str(path)}
    manifest = json.loads((REPO / "tests/fixtures/uniswap-v3/MANIFEST.json").read_text())
    for name in ["UniswapV3Factory", "SwapRouter02", "QuoterV2", "UniswapV3Pool"]:
        path = REPO / ("tests/fixtures/uniswap-v3/" + name + ".json")
        entries = [item for package in manifest for item in package["artifacts"] if item["contract"] == name]
        require(len(entries) == 1 and digest(path) == entries[0]["sha256"], "V3 pin mismatch: " + name)
        original = json.loads(path.read_text())
        contracts[name] = {"abi": original["abi"], "bytecode": original["bytecode"], "deployedBytecode": original["deployedBytecode"], "artifact_sha256": digest(path), "artifact_path": str(path), "provenance": entries[0]}
    for name, artifact in contracts.items():
        init_size = len(bytes.fromhex(artifact["bytecode"].removeprefix("0x")))
        runtime_size = len(bytes.fromhex(artifact["deployedBytecode"].removeprefix("0x")))
        require(init_size <= 262144 and runtime_size <= 131072, "Monad deployment size limit: " + name)
        artifact["initcode_bytes_without_constructor_args"] = init_size
        artifact["runtime_bytes"] = runtime_size
    source_hashes = input_hashes()
    require(before_hashes == source_hashes, "Source inputs changed during offline build")
    protocol_head = json.loads((REPO / "tests/fixtures/solidity/PROTOCOL_INPUTS.json").read_text())["source_head"]
    bundle = {"network": "none", "target_chain_id": 10143, "protocol_head": protocol_head, "compiler": "solc0.8.31", "optimizer_runs": 200, "via_ir": False, "evm_version": "osaka", "forge_version": forge_version.strip(), "solc_version": solc_version.strip(), "build_command": command, "build_exit_code": result.returncode, "build_log_sha256": digest(output / "build.log"), "source_inputs_unchanged": True, "source_sha256": source_hashes, "contracts": contracts}
    (output / "artifacts.json").write_text(json.dumps(bundle, indent=2) + "\n")
    print("Offline Monad Testnet artifact bundle prepared: " + str(output / "artifacts.json"))


if __name__ == "__main__":
    main()
