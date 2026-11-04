#!/usr/bin/env bash
# Local state generation only; no public RPC or broadcasting.
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
task_forge="${FORGE:-forge}"
task_solc="${SOLC:-solc}"
"$task_forge" --version | python3 -c 'import sys; sys.exit(0 if "Version: 1.8.4" in sys.stdin.read() else "Foundry 1.8.4 required")'
"$task_solc" --version | python3 -c 'import sys; sys.exit(0 if "Version: 0.8.31+" in sys.stdin.read() else "solc 0.8.31 required")'
python3 "$task_repo/scripts/verify-solidity-fixtures.py" --inputs-only
mkdir -p "$task_repo/tests/fixtures/solidity/generated"
task_build_dir="$(mktemp -d "${TMPDIR:-/tmp}/barterlend-executor-fixture.XXXXXX")"
trap 'rm -rf "$task_build_dir"' EXIT
"$task_forge" test --root "$task_repo" --use "$task_solc" --offline --network monad --out "$task_build_dir/out" --cache-path "$task_build_dir/cache" --match-contract ExecutorSeedGenerator -vv
python3 - "$task_repo" "$task_build_dir/out" <<'PY'
from pathlib import Path
import hashlib, json, sys
repo = Path(sys.argv[1]); fixture = repo / 'tests/fixtures/solidity'
generated = fixture / 'generated'
state = json.loads((fixture / 'anvil-block-template.json').read_text())
state['accounts'] = json.loads((generated / 'forge-state.json').read_text())
for account in state['accounts'].values():
    if isinstance(account.get('nonce'), str):
        account['nonce'] = int(account['nonce'], 16)
(generated / 'anvil-state.json').write_text(json.dumps(state, sort_keys=True, separators=(',', ':')) + '\n')
manifest = json.loads((generated / 'manifest.json').read_text())
(generated / 'deployment.json').write_text(json.dumps(manifest, indent=2) + '\n')
artifact = json.loads((Path(sys.argv[2]) / 'LiquidationExecutor.sol/LiquidationExecutor.json').read_text())
settings = artifact['metadata']['settings']
assert not settings.get('viaIR', False) and settings['evmVersion'] == 'osaka'
assert settings['optimizer'] == {'enabled': True, 'runs': 200}
abi = (json.dumps(artifact['abi'], indent=2) + '\n').encode()
(repo / 'abi').mkdir(exist_ok=True)
(repo / 'abi/LiquidationExecutor.json').write_bytes(abi)
provenance = {'source':'contracts/LiquidationExecutor.sol',
    'source_sha256':hashlib.sha256((repo/'contracts/LiquidationExecutor.sol').read_bytes()).hexdigest(),
    'abi_sha256':hashlib.sha256(abi).hexdigest(), 'solc':'0.8.31', 'via_ir':False,
    'optimizer_runs':200, 'evm_version':'osaka', 'deployment_executed':False}
(repo / 'abi/PROVENANCE.json').write_text(json.dumps(provenance, indent=2) + '\n')
example_path = repo / 'examples/local-manifest.json'
example = json.loads(example_path.read_text())
# Local fixtures opt into fast RPC explicitly; the bot default remains 15rps.
example['rpc'] = {'requests_per_second': 1000}
for field in ['executor', 'owner', 'operator']:
    example[field] = manifest[field]
example['executor_code_hash'] = manifest['runtime_code_hashes']['executor']
configured_pool = example['pools']['local-pool']
configured_pool['spec']['address'] = manifest['pool']
configured_pool['code_hash'] = manifest['runtime_code_hashes']['pool']
for field in ['asset', 'collateral']:
    configured_pool[field]['address'] = manifest[field]
configured_pool['oracle'] = manifest['oracle']
configured_pool['oracle_code_hash'] = manifest['runtime_code_hashes']['oracle']
for field in ['factory', 'router', 'quoter', 'wrapped_native']:
    example['v3'][field] = manifest[field]
    example['v3']['code_hashes'][field] = manifest['runtime_code_hashes'][field]
example['v3']['allowed_tokens'] = [manifest[field] for field in ['asset', 'collateral', 'wrapped_native']]
example['v3']['intermediates'] = [manifest['wrapped_native']]
example_path.write_text(json.dumps(example, indent=2) + '\n')
source_paths = {repo / 'contracts/KyberRoute.sol', repo / 'contracts/IExecutorV4.sol', repo / 'contracts/LiquidationExecutor.sol', repo / 'tests/solidity/ExecutorFixture.sol',
    repo / 'foundry.toml', repo / 'scripts/regenerate-executor-fixture.sh', repo / 'tests/fixtures/factory/TestLendingFactory.sol',
    fixture / 'anvil-block-template.json', repo / 'tests/fixtures/uniswap-v3/MANIFEST.json'}
for name in ['PROTOCOL_INPUTS.json', 'SUPPLEMENTAL_INPUTS.json']:
    source_paths.add(fixture / name)
    source_paths.update(fixture / 'protocol-v5' / item['path']
        for item in json.loads((fixture / name).read_text())['files'])
for package in json.loads((repo / 'tests/fixtures/uniswap-v3/MANIFEST.json').read_text()):
    source_paths.update(repo / 'tests/fixtures/uniswap-v3' / item['file'] for item in package['artifacts'])
source_hashes = {str(path.relative_to(repo)): hashlib.sha256(path.read_bytes()).hexdigest()
    for path in sorted(source_paths)}
files = [{'path':p.name,'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size}
    for p in sorted(generated.iterdir()) if p.is_file() and p.name != 'SHA256.json']
(generated / 'SHA256.json').write_text(json.dumps({'files':files, 'source_sha256':source_hashes},indent=2)+'\n')
print(f'Local fixture ready: {len(state["accounts"])} accounts, {len(manifest["pools"])} real V3 pools; ABI refreshed')
PY
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
