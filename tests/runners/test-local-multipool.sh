#!/usr/bin/env bash
# Autonomous two-pool fixture and whole-app acceptance; no fork/public RPC.
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
exec python3 - "$task_repo" "$@" <<'PY'
from pathlib import Path
import hashlib
import http.server
import threading
import collections
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


require(len(sys.argv) == 3, 'usage: bash tests/runners/test-local-multipool.sh <fresh-evidence-directory>')
repo = Path(sys.argv[1]).resolve()
output = Path(sys.argv[2]).resolve()
require(not output.is_relative_to(repo.parent / 'BarterLend'), 'BarterLend is read-only')
require(not output.exists(), 'use a fresh evidence directory; old evidence is never overwritten')
output.mkdir(parents=True)
forge = os.environ.get('FORGE', 'forge')
solc = os.environ.get('SOLC', 'solc')
anvil = os.environ.get('ANVIL', 'anvil')
cargo = os.environ.get('CARGO', 'cargo')
bulk = int(os.environ.get('BARTERLEND_BULK_POOLS','0'))
require(bulk in (0,4,8), 'bulk pools must be 4 or 8')
test_target = 'app_bulk_local' if bulk else 'app_multipool_local'
for binary, version in [(forge, 'Version: 1.8.4'), (anvil, 'Version: 1.8.4'), (solc, 'Version: 0.8.31+')]:
    require(version in subprocess.check_output([binary, '--version'], text=True), 'pinned toolchain required: ' + binary)

fixture = repo / 'tests/fixtures/solidity'
generator = repo / 'tests/fixtures/multipool/ExecutorMultiPoolSeedGenerator.sol'
test_source = repo / ('tests/'+test_target+'.rs')
require(test_source.is_file(), 'the actual two-pool integration target is required')
manifest_path = fixture / 'generated/SHA256.json'
pinned = {entry['path']: entry['sha256'] for entry in json.loads(manifest_path.read_text())['files']}
source_files = [
    'tests/solidity/ExecutorFixture.sol', 'tests/fixtures/factory/TestLendingFactory.sol', 'contracts/LiquidationExecutor.sol', 'contracts/IExecutorV4.sol',
    'tests/fixtures/multipool/ExecutorMultiPoolSeedGenerator.sol',
    str(test_source.relative_to(repo)), 'tests/runners/test-local-multipool.sh',
] + [str(path.relative_to(repo)) for path in sorted((repo / 'src').rglob('*.rs'))] + ['Cargo.toml', 'Cargo.lock']
source_pins = {name: digest(repo / name) for name in source_files}
report = {'scope': 'offline-actual-two-credit-pools-whole-app', 'public_rpc_used': False,
          'production_modified': False, 'existing_fixture_pins_modified': False,
          'source_sha256': source_pins, 'commands': []}
report_path = output / 'multipool-acceptance.json'


def save():
    report_path.write_text(json.dumps(report, indent=2) + '\n')


def check_pins():
    for name, expected in pinned.items():
        require(digest(fixture / 'generated' / name) == expected, 'existing generated fixture changed: ' + name)
    for name, expected in source_pins.items():
        require(digest(repo / name) == expected, 'source changed during the run: ' + name)


def run(name, command, cwd):
    log = output / (name + '.log')
    started = time.monotonic()
    with log.open('w') as stream:
        result = subprocess.run([sys.executable,str(repo/'scripts/bounded-test.py'),*command], cwd=cwd, stdout=stream, stderr=subprocess.STDOUT)
    record = {'command': command, 'cwd': str(cwd), 'exit_code': result.returncode,
              'seconds': round(time.monotonic() - started, 3), 'log': str(log), 'sha256': digest(log)}
    report['commands'].append(record)
    save()
    print(json.dumps(record), flush=True)
    require(result.returncode == 0, name + ' failed; inspect ' + str(log))
    return log.read_text()


check_pins()
run('verify-inputs', [sys.executable, str(repo / 'scripts/verify-solidity-fixtures.py')], repo)
with tempfile.TemporaryDirectory(prefix='barterlend-local-multipool-') as directory:
    work = Path(directory)
    stage = work / 'stage'
    for name in ['contracts', 'tests/fixtures/factory', 'tests/fixtures/uniswap-v3', 'tests/fixtures/solidity/protocol-v5']:
        shutil.copytree(repo / name, stage / name)
    (stage / 'tests/solidity').mkdir(parents=True)
    (stage / 'generated').mkdir()
    shutil.copy2(repo / 'tests/solidity/ExecutorFixture.sol', stage / 'tests/solidity/ExecutorFixture.sol')
    shutil.copy2(generator, stage / 'tests/solidity/ExecutorMultiPoolSeedGenerator.sol')
    config, replacements = re.subn(r'^fs_permissions =.*$',
        'fs_permissions = [{access="read",path="tests/fixtures/uniswap-v3"},{access="read-write",path="generated"}]',
        (repo / 'foundry.toml').read_text(), flags=re.MULTILINE)
    require(replacements == 1, 'fixture fs-permissions config changed')
    (stage / 'foundry.toml').write_text(config)
    os.environ['BARTERLEND_MULTIPOOL_OUTPUT_DIR'] = str(stage / 'generated')
    seed_log = run('multipool-seed', [forge, 'test', '--root', str(stage), '--use', solc,
        '--offline', '--network', 'monad', '--match-contract', 'ExecutorMultiPoolSeedGenerator', '-vv'], stage)
    require('[PASS] test_generate_isolated_two_pool_seed()' in seed_log and '1 tests passed, 0 failed' in seed_log,
            'exact two-pool seed generator must pass')
    manifest = json.loads((stage / 'generated/manifest.json').read_text())
    require(manifest['pool'] != manifest['pool_b'] and manifest['oracle'] != manifest['oracle_b'],
            'two distinct lending pools and independent oracles required')
    state = json.loads((fixture / 'anvil-block-template.json').read_text())
    state['accounts'] = json.loads((stage / 'generated/forge-state.json').read_text())
    for account in state['accounts'].values():
        if isinstance(account.get('nonce'), str):
            account['nonce'] = int(account['nonce'], 16)
    state_path = stage / 'generated/anvil-state.json'
    state_path.write_text(json.dumps(state, sort_keys=True, separators=(',', ':')) + '\n')
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    if os.environ.get('BARTERLEND_EXPORT_SEED'):
        exported = Path(os.environ['BARTERLEND_EXPORT_SEED']).resolve()
        require(not exported.exists() and repo not in exported.parents,
                'seed export requires a fresh external directory')
        exported.mkdir(parents=True)
        shutil.copy2(state_path, exported / 'anvil-state.json')
        shutil.copy2(output / 'manifest.json', exported / 'manifest.json')
    report['generated'] = {'accounts': len(state['accounts']), 'anvil_state_sha256': digest(state_path),
                           'manifest_sha256': digest(output / 'manifest.json'),
                           'pool_a': manifest['pool'], 'pool_b': manifest['pool_b'],
                           'oracle_a': manifest['oracle'], 'oracle_b': manifest['oracle_b']}
    save()

    listener = socket.socket()
    listener.bind(('127.0.0.1', 0))
    port = listener.getsockname()[1]
    listener.close()
    url = f'http://127.0.0.1:{port}'
    os.environ['BARTERLEND_LOCAL_RPC_URL'] = url
    os.environ['BARTERLEND_LOCAL_MANIFEST'] = str(stage / 'generated/manifest.json')
    node_command = [anvil, '--host', '127.0.0.1', '--port', str(port), '--network', 'monad',
                    '--chain-id', '31337', '--load-state', str(state_path), '--silent']
    if bulk: node_command+=['--block-time','0.3','--slots-in-an-epoch','1']
    report['anvil_command'] = node_command
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def rpc(method, params):
        request = urllib.request.Request(url, data=json.dumps({'jsonrpc': '2.0', 'id': 1,
            'method': method, 'params': params}).encode(), headers={'Content-Type': 'application/json'})
        with opener.open(request, timeout=1) as response:
            reply = json.load(response)
        require('error' not in reply, 'local node rejected startup read')
        return reply['result']

    calls=[];call_lock=threading.Lock();proxy=None;proxy_thread=None
    if bulk:
        class Proxy(http.server.BaseHTTPRequestHandler):
            def log_message(self,*unused):pass
            def do_POST(self):
                size=int(self.headers.get('Content-Length','0'))
                if not 0<size<2*1024*1024:self.send_error(413);return
                body=self.rfile.read(size);values=json.loads(body);items=values if isinstance(values,list) else [values]
                with call_lock:calls.extend((time.monotonic(),item.get('method','unknown')) for item in items)
                try:
                    request=urllib.request.Request(url,data=body,headers={'Content-Type':'application/json'})
                    with opener.open(request,timeout=30) as response:data=response.read()
                    self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
                except (OSError,ValueError):
                    try:self.send_error(503)
                    except OSError:pass
        proxy=http.server.ThreadingHTTPServer(('127.0.0.1',0),Proxy)
        proxy_thread=threading.Thread(target=proxy.serve_forever,daemon=True);proxy_thread.start()
        os.environ['BARTERLEND_BULK_PROXY_URL']=f'http://127.0.0.1:{proxy.server_port}'
    with (output / 'anvil.log').open('w') as node_log:
        node = subprocess.Popen(node_command, stdout=node_log, stderr=subprocess.STDOUT)
        try:
            for _ in range(100):
                require(node.poll() is None, 'isolated Anvil exited; inspect anvil.log')
                try:
                    height = rpc('eth_blockNumber', [])
                    break
                except OSError:
                    time.sleep(0.05)
            else:
                raise RuntimeError('isolated seeded Anvil did not start')
            require((bulk or height == '0x0') and int(rpc('eth_chainId', []), 16) == 31337, 'fresh local chain31337 required')
            require(rpc('eth_getBlockByNumber', ['0x0', False])['hash'] == manifest['genesis_hash'], 'local genesis does not match generated manifest')
            for address in [manifest['pool'], manifest['pool_b'], manifest['executor'], manifest['router']]:
                require(rpc('eth_getCode', [address, '0x0']) != '0x', 'generated contract code missing: ' + address)
            name = 'actual_bulk_census_and_atomic_liquidations_at_15rps' if bulk else 'actual_two_pools_multiple_debts_finalize_without_files'
            os.environ['BARTERLEND_BULK_REPORT']=str(output/'bulk-result.json')
            contents = run('multipool-app', [cargo, '+'+os.environ.get('BARTERLEND_RUST_TOOLCHAIN','1.98.0'), 'test', '--offline', '--locked',
                '--manifest-path', str(repo / 'Cargo.toml'), '--test', test_target,
                name, '--', '--ignored', '--exact', '--nocapture'], repo)
            require('test ' + name + ' ... ok' in contents and '1 passed; 0 failed; 0 ignored;' in contents,
                    'exact actual two-pool integration test must execute and pass')
        finally:
            if proxy:
                proxy.shutdown();proxy.server_close();proxy_thread.join(timeout=5)
                with call_lock:observed=sorted(calls)
                left=0;peak=0
                for right,(stamp,_) in enumerate(observed):
                    while observed[left][0]<=stamp-1:left+=1
                    peak=max(peak,right-left+1)
                rate={'max_rolling_second_requests':peak,'limit':15,'requests':len(observed),'methods':dict(collections.Counter(m for _,m in observed)),'scope':'all bot RPC items; setup/assertions excluded'}
                (output/'rpc-rate.json').write_text(json.dumps(rate,indent=2)+'\n')
                report['rpc']=rate
            node.terminate()
            try:
                node.wait(timeout=5)
            except subprocess.TimeoutExpired:
                node.kill()
                node.wait(timeout=5)
    if bulk: require(report['rpc']['max_rolling_second_requests']<=15,'shared RPC limit exceeded')
    check_pins()
    run('verify-final-inputs', [sys.executable, str(repo / 'scripts/verify-solidity-fixtures.py')], repo)
    report['passed'] = True
    report['tests'] = {'isolated_seed_generation': 1, 'actual_two_pool_app': 1}
    save()
    print('PASS: actual two-pool app with independent oracles; existing pins unchanged', flush=True)
PY
