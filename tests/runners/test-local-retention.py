#!/usr/bin/env python3
"""Bounded real-time retention run on an isolated, pre-generated 4000-debt Anvil seed.

No public RPC or signer. The actual CLI runs keyless; the harness creates and
closes genuine debts through impersonated local accounts. Output stays external.
"""
import argparse
import json
import os
import pathlib
import signal
import shutil
import hashlib
import socket
import subprocess
import time
import http.client

ROOT = pathlib.Path(__file__).resolve().parents[2]
WAD = 10**18


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def probe_port():
    # Keep the long-lived health listener outside the client's ephemeral range.
    for candidate in range(18090, 18190):
        with socket.socket() as sock:
            try:
                sock.bind(('127.0.0.1', candidate))
                return candidate
            except OSError:
                continue
    raise RuntimeError('no free local probe port')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seed', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--bot', type=pathlib.Path, required=True)
    parser.add_argument('--anvil', type=pathlib.Path, required=True)
    parser.add_argument('--cast', type=pathlib.Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    require(not out.exists() and ROOT not in out.parents, 'fresh external output directory required')
    out.mkdir(parents=True)
    # Never execute a Cargo output that another test/build may replace while
    # this long-running process has its pages mapped (notably on macOS).
    binary = out / 'bot-under-test'
    original_digest = hashlib.sha256(args.bot.read_bytes()).hexdigest()
    seed = json.loads((args.seed / 'manifest.json').read_text())
    require(len(seed['credit_pools']) == 8 and seed['positions_per_market'] == 250, '4000-debt seed required')
    url = f'http://127.0.0.1:{port()}'
    health_url = f'http://127.0.0.1:{probe_port()}'
    health_errors = []
    rpc_connect_retries = []

    def rpc(method, params):
        body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}).encode()
        connection = None
        # Retry only failed TCP establishment. No request bytes have been sent;
        # a setup mutation is never retried after an uncertain POST/response.
        for attempt in range(3):
            connection = http.client.HTTPConnection('127.0.0.1', int(url.rsplit(':', 1)[1]), timeout=5)
            try:
                connection.connect()
                break
            except OSError as error:
                connection.close()
                if len(rpc_connect_retries) < 1000:
                    rpc_connect_retries.append({'method': method, 'reason': str(error)[:200], 'attempt': attempt})
                if attempt == 2:
                    raise
                time.sleep(.1)
        try:
            connection.request('POST', '/', body, {'Content-Type': 'application/json'})
            response = connection.getresponse()
            require(response.status == 200, 'local RPC HTTP failure')
            value = json.loads(response.read())
        finally:
            connection.close()
        require('error' not in value, f'local RPC {method} failed: {value.get("error")}')
        return value['result']

    def cast(*argv):
        return subprocess.check_output([str(args.cast), *map(str, argv)], text=True, timeout=10).strip()

    def send(actor, to, signature, *values):
        rpc('anvil_impersonateAccount', [actor])
        rpc('anvil_setBalance', [actor, hex(10 * WAD)])
        data = cast('calldata', signature, *values)
        tx = rpc('eth_sendTransaction', [{'from': actor, 'to': to, 'data': data, 'gas': hex(2_000_000), 'gasPrice': hex(2_000_000_000)}])
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            receipt = rpc('eth_getTransactionReceipt', [tx])
            if receipt:
                require(int(receipt['status'], 16) == 1, 'local churn transaction reverted: ' + signature)
                return
            time.sleep(.1)
        raise RuntimeError('local churn transaction did not mine')

    def scalar(to, signature, *values):
        return int(rpc('eth_call', [{'to': to, 'data': cast('calldata', signature, *values)}, 'latest']), 16)

    def health():
        connection = http.client.HTTPConnection('127.0.0.1', int(health_url.rsplit(':', 1)[1]), timeout=2)
        stage = 'connect'
        try:
            connection.connect()
            stage = 'headers'
            connection.request('GET', '/ready')
            response = connection.getresponse()
            require(response.status in (200, 503), 'unexpected health status')
            stage = 'body'
            return json.loads(response.read())
        except (OSError, ValueError, http.client.HTTPException) as error:
            if len(health_errors) < 1000:
                health_errors.append({'elapsed_seconds': time.monotonic() - started, 'type': type(error).__name__,
                                      'stage': stage,
                                      'local_reason': str(getattr(error, 'reason', error))[:200]})
            return None
        finally:
            connection.close()

    report = {'passed': False, 'scope': 'actual CLI dry-run and genuine EVM churn', 'positions': 4000,
              'rpc_limit': 15, 'block_ms': 300, 'samples': [], 'task_counts_measured': False,
              'public_rpc': False, 'live_execution': False, 'health_errors': health_errors,
              'rpc_connect_retries': rpc_connect_retries,
              'binary_sha256': original_digest}
    node = bot = None
    started = time.monotonic()
    try:
        shutil.copy2(args.bot, binary)
        require(hashlib.sha256(binary.read_bytes()).hexdigest() == original_digest
                == hashlib.sha256(args.bot.read_bytes()).hexdigest(), 'binary changed while copying')
        with (out / 'anvil.log').open('w') as log:
            node = subprocess.Popen([str(args.anvil), '--host', '127.0.0.1', '--port', url.rsplit(':', 1)[1],
                '--chain-id', '31337', '--network', 'monad', '--block-time', '.3', '--slots-in-an-epoch', '1',
                '--load-state', str(args.seed / 'anvil-state.json'), '--silent'], stdout=log, stderr=log)
        for _ in range(100):
            try:
                if rpc('eth_chainId', []) == '0x7a69':
                    break
            except OSError:
                time.sleep(.1)
        require(rpc('eth_getBlockByNumber', ['0x0', False])['hash'] == seed['genesis_hash'], 'seed genesis mismatch')
        mc = '0x0000000000000000000000000000000000000900'
        code = '0x' + (ROOT / 'tests/fixtures/multicall3-runtime.hex').read_text().strip().removeprefix('0x')
        rpc('anvil_setCode', [mc, code])
        config = json.loads((ROOT / 'examples/local-manifest.json').read_text())
        config.update(rpc_url=url, health_listen=health_url.removeprefix('http://'), poll_ms=1000, queue_capacity=1000,
                      executor=seed['executor'], executor_code_hash=seed['runtime_code_hashes']['executor'], owner=seed['owner'], operator=seed['operator'])
        config['rpc'] = {'requests_per_second': 15, 'multicall3': {'address': mc, 'code_hash': cast('keccak', code),
                         'max_calls': 64, 'gas_limit': 10_000_000, 'max_calldata_bytes': 262_144},
                         'read_optimizations': {'batch_log_hashes': True, 'coalesce_pending_checks': True,
                         'position_batch_size': 128, 'position_batch_gas_limit': 10_000_000, 'position_batch_calldata_limit': 262_144}}
        config['sdk_log_chunk_size'] = 1000
        config['sync'] = {'reconcile_user_page_size': 128}
        template = next(iter(config['pools'].values()))
        config['pools'] = {}
        for i, pool in enumerate(seed['credit_pools']):
            p = json.loads(json.dumps(template))
            p['spec']['address'] = pool['address']
            p.update(code_hash=pool['code_hash'], oracle=pool['oracle'], oracle_code_hash=pool['oracle_code_hash'])
            p['asset']['address'], p['collateral']['address'] = seed['asset'], seed['collateral']
            config['pools'][str(i)] = p
        for name in ['factory', 'router', 'quoter', 'wrapped_native']:
            config['v3'][name] = seed[name]
            config['v3']['code_hashes'][name] = seed['runtime_code_hashes'][name]
        config['v3'].update(allowed_tokens=[seed[n] for n in ['asset', 'collateral', 'wrapped_native']], intermediates=[], fee_tiers=[3000])
        config['v3']['limits'].update(quote_ttl_seconds=60, max_age_blocks=200)
        config['planner']['ttl_seconds'] = 60
        config_path = out / 'config.json'
        config_path.write_text(json.dumps(config, indent=2) + '\n')
        subprocess.run([str(binary), '--config', str(config_path), 'config', 'check'], check=True, timeout=10)
        with (out / 'stdout.jsonl').open('w') as stdout, (out / 'stderr.jsonl').open('w') as stderr:
            bot = subprocess.Popen([str(binary), '--config', str(config_path)], stdout=stdout, stderr=stderr, env=dict(os.environ, LOG_FILTER='warn'))
        ready = None
        while time.monotonic() - started < 100:
            ready = health()
            if ready and len(ready['pools']) == 8 and all(p['verified'] for p in ready['pools']):
                break
            require(bot.poll() is None, 'CLI exited during census')
            time.sleep(.5)
        require(ready and all(p['verified'] for p in ready['pools']), 'census failed')
        report['initial_ready_seconds'] = time.monotonic() - started
        steady_health_errors = len(health_errors)
        old = [f'0x{0x100000 + i * 1000 + 10:040x}' for i in range(8)]
        market = int(seed['market0'])
        rounds = 0
        next_churn = next_sample = time.monotonic()
        phase_start = time.monotonic()
        # 18 real minutes, plus bounded census/drain/shutdown: whole test <20min.
        while time.monotonic() - phase_start < 1080 and time.monotonic() - started < 1140:
            require(bot.poll() is None, 'CLI exited during churn')
            now = time.monotonic()
            if now >= next_churn:
                i = rounds % 8
                p = seed['credit_pools'][i]
                account = f'0x{0x900000 + rounds:040x}'
                send(seed['owner'], p['oracle'], 'setPrice(uint256)', WAD)
                debt = scalar(p['address'], 'debtOf(uint64,address)', market, old[i])
                # A filled order's debt includes its interest; use the actual
                # getter, not the nominal amount provided to the order book.
                send(old[i], seed['asset'], 'mint(uint256)', debt)
                send(old[i], seed['asset'], 'approve(address,uint256)', p['address'], debt)
                send(old[i], p['address'], 'deposit(address,uint128)', old[i], debt)
                send(old[i], p['address'], 'repay(address,address,uint64,uint128)', old[i], old[i], market, debt)
                require(scalar(p['address'], 'debtOf(uint64,address)', market, old[i]) == 0, 'old debt not closed')
                send(old[i], p['address'], 'withdrawCollateral(address,uint64,address,uint128)', old[i], market, old[i], 800 * WAD)
                send(account, seed['collateral'], 'mint(uint256)', 800 * WAD)
                send(account, seed['collateral'], 'approve(address,uint256)', p['address'], 800 * WAD)
                send(account, p['address'], 'depositCollateral(address,uint64,uint128)', account, market, 800 * WAD)
                send(account, p['address'], 'placeBorrowOrder(address,uint64,uint256,uint128)', account, market, 1000, 100 * WAD)
                send(seed['provider_a'], seed['asset'], 'mint(uint256)', 100 * WAD)
                send(seed['provider_a'], seed['asset'], 'approve(address,uint256)', p['address'], 100 * WAD)
                send(seed['provider_a'], p['address'], 'deposit(address,uint128)', seed['provider_a'], 100 * WAD)
                send(seed['provider_a'], p['address'], 'provideExactIn(address,uint64,uint128,uint128,uint16)', seed['provider_a'], market, 100 * WAD, 0, 10)
                send(seed['owner'], p['oracle'], 'setPrice(uint256)', (2 if rounds % 3 == 0 else 4) * WAD // 10)
                old[i] = account
                rounds += 1
                if rounds % 8 == 0:
                    print(json.dumps({'churn_rounds': rounds, 'elapsed_seconds': round(time.monotonic() - started, 1)}), flush=True)
                next_churn = time.monotonic() + 8
            if now >= next_sample:
                h = health()
                rss = int(subprocess.check_output(['ps', '-o', 'rss=', '-p', str(bot.pid)], text=True).strip()) * 1024
                fds = subprocess.check_output(['lsof', '-a', '-p', str(bot.pid), '-Ff'], text=True)
                count = sum(line[1:].isdigit() for line in fds.splitlines() if line.startswith('f'))
                report['samples'].append({'elapsed_seconds': time.monotonic() - started, 'rss_bytes': rss, 'fds': count,
                                          'queue': h['queue_len'] if h else None, 'pools': h['pools'] if h else None})
                require(rss < 800 * 1024 * 1024, 'RSS exceeds 1GiB budget minus20% headroom')
                if h is None:
                    # Preserve a diagnostic follow-up, but do not turn a failed
                    # primary observation into a passing retention result.
                    report['failed_probe_followup'] = health()
                # Keep measuring memory after a transport failure. It still
                # fails overall acceptance below; do not hide it with a retry.
                require(h is None or h['queue_len'] <= 1000, 'queue bound failed')
                next_sample = time.monotonic() + 15
            time.sleep(.1)
        report['churn_rounds'] = rounds
        report['real_churn_seconds'] = time.monotonic() - phase_start
        report['nonce'] = int(rpc('eth_getTransactionCount', [seed['operator'], 'latest']), 16)
        require(report['nonce'] == 0, 'keyless CLI consumed operator nonce')
        samples = report['samples']
        early = [s for s in samples if 120 <= s['elapsed_seconds'] <= 300]
        late = samples[-10:]
        require(early and len(samples) >= 40 and rounds >= 40, 'insufficient real-time observations')
        report['rss_growth_bytes'] = max(s['rss_bytes'] for s in late) - max(s['rss_bytes'] for s in early)
        report['fd_growth'] = max(s['fds'] for s in late) - max(s['fds'] for s in early)
        require(report['rss_growth_bytes'] <= 16 * 1024 * 1024 and report['fd_growth'] <= 16, 'retention growth exceeded fixed bounds')
        report['memory_passed'] = True
        report['health_passed'] = len(health_errors) == steady_health_errors
        report['passed'] = report['health_passed']
        require(report['passed'], 'health transport failed; memory measurements retained separately')
    finally:
        report['bot_exit_before_cleanup'] = bot.poll() if bot is not None else None
        for child in [bot, node]:
            if child is not None and child.poll() is None:
                child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=25 if child is bot else 5)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait()
                    report['passed'] = False
        binary.unlink(missing_ok=True)
        report['elapsed_seconds'] = time.monotonic() - started
        require(report['elapsed_seconds'] <= 1200, 'whole retention test exceeded20min')
        (out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({k: report[k] for k in ['passed', 'positions', 'elapsed_seconds', 'churn_rounds',
                         'rss_growth_bytes', 'fd_growth', 'bot_exit_before_cleanup'] if k in report}))


if __name__ == '__main__':
    main()
