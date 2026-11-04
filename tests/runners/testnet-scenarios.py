#!/usr/bin/env python3
"""Additional bounded scenarios on an explicitly prepared test deployment.

Uses run-testnet.py's signer, budgets and evidence. Never connects to mainnet.
Runtime keys and reports remain outside this repository.
"""
import argparse
import fcntl
import importlib.util
import json
import pathlib
import sys
import time

spec = importlib.util.spec_from_file_location('testnet_runner', pathlib.Path(__file__).resolve().parents[2] / 'tests/runners/run-testnet.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
WAD = m.WAD


def require(value, message):
    if not value:
        raise RuntimeError(message)


def cast_json(text):
    value = json.loads(text)
    # Foundry versions may wrap block JSON in a result envelope.
    if isinstance(value, dict) and 'schema_version' in value:
        require(value.get('success') is True and not value.get('errors'), 'cast returned a failed JSON envelope')
        value = value.get('data')
    require(isinstance(value, dict), 'cast JSON must contain an object')
    return value


def open_position(r, label, market, scale=1, actor='borrower'):
    pool, borrower = r.context['credit_pool'], r.state['actors'][actor]['address']
    key = (market, borrower)
    if label + '-open-00' not in r.state['steps']:
        require(r.scalar(pool, 'debtOf(uint64,address)', key) == 0, 'scenario starts with an existing debt')
        require(r.scalar(pool, 'collateralOf(uint64,address)', key) == 0, 'scenario starts with existing collateral')
    r.context['market_from_getActiveMarkets'] = market
    for number, original in enumerate(r.template['provided_position_template']):
        action = json.loads(json.dumps(original))
        # Scale only the exact synthetic asset quantities, never selectors/markets.
        if action.get('actor') == 'borrower':
            action['actor'] = actor
        action['args'] = [borrower if value == '${borrower}' else value for value in action['args']]
        action['args'] = [str(int(value) * scale) if value in ['400000000000000000000', '100000000000000000000'] else value for value in action['args']]
        r.action(f'{label}-open-{number:02d}', action)
    debt = r.scalar(pool, 'debtOf(uint64,address)', key)
    collateral = r.scalar(pool, 'collateralOf(uint64,address)', key)
    require(debt == 100 * WAD * scale and collateral == 400 * WAD * scale, 'position setup getter mismatch')
    r.send(label + '-withdraw-loan', pool, 'withdraw(address,address,uint128)',
           [borrower, borrower, debt], actor=actor)
    return {'pool': pool, 'market': market, 'borrower': borrower, 'debt': debt, 'collateral': collateral, 'actor': actor}


def audit(r, label, positions, baseline, live, partial=False, funding=0):
    events = r.bot_events(live['run']['stdout'])
    outcomes = [e['outcome'] for e in events if e.get('event') == 'finalized']
    require(len(outcomes) == len(positions) and all(o['kind'] == 'Liquidated' for o in outcomes), 'unexpected terminal outcome')
    old, new = live['operator_before'], live['operator_after']
    require(new['latest_nonce'] == old['latest_nonce'] + len(positions), 'unexpected paid/repeated nonce')
    charge = sum(int(o['native_charge'], 0) for o in outcomes)
    require(int(old['balance_wei'], 0) + funding - int(new['balance_wei'], 0) == charge, 'actual MON charge differs from signed bounds')
    profit = sum(int(o['asset_gain'], 0) for o in outcomes)
    balance = r.scalar(r.context['asset'], 'balanceOf(address)', [r.context['executor']])
    require(balance - baseline == profit, 'profit balance delta mismatch')
    actual = []
    for position in positions:
        key = (position['market'], position['borrower'])
        matches = [o for o in outcomes if o['attempt']['kind']['Liquidation']['key'] ==
                   {'pool': position['pool'].lower(), 'market': position['market'], 'borrower': position['borrower'].lower()}]
        require(len(matches) == 1, 'outcome does not identify exactly one expected position')
        outcome = matches[0]
        meta = outcome['attempt']['kind']['Liquidation']
        require(int(outcome['asset_gain'], 0) >= int(meta['min_asset_profit'], 0), 'profit guard violated')
        receipt = cast_json(r.cast('receipt', outcome['attempt']['hash'], '--rpc-url', r.args.rpc, '--json'))
        require(int(receipt['status'], 0) == 1 and int(receipt['blockNumber'], 0) == outcome['receipt']['block_number'], 'independent receipt mismatch')
        debt = r.scalar(position['pool'], 'debtOf(uint64,address)', key)
        collateral = r.scalar(position['pool'], 'collateralOf(uint64,address)', key)
        eligible = r.scalar(position['pool'], 'isLiquidatable(uint64,address)', key)
        require((0 < debt < position['debt'] and eligible == 0) if position.get('expect_partial', partial) else debt == 0, 'unexpected remaining debt or eligibility')
        require(r.scalar(r.context['asset'], 'allowance(address,address)', [r.context['executor'], position['pool']]) == 0, 'repayment allowance remains')
        actual.append(dict(position, remaining_debt=debt, remaining_collateral=collateral,
                           receipt=receipt, outcome=outcome))
    require(r.scalar(r.context['collateral'], 'allowance(address,address)', [r.context['executor'], r.context['router']]) == 0, 'swap allowance remains')
    report = {'passed': True, 'scenario': label, 'actual': actual, 'native_charge_wei': str(charge), 'asset_profit_wei': str(profit),
              'live_evidence': str(r.directory / (label + '-result.json'))}
    return report


def maturity(r):
    label = 'maturity-sized'
    require(r.state.get('primary_liquidation_passed'), 'primary HF scenario must finish first')
    case = r.state.setdefault('extra_scenarios', {}).get(label)
    if case and case['stage'] == 'passed':
        print(json.dumps({'scenario': label, 'passed': True, 'already_audited': True}))
        return
    if case and case['stage'] == 'running':
        # A receipt/audit interruption must never repeat opening or liquidation.
        finish_maturity(r, label, case)
        return
    if case is None:
        r.ensure_oracle(label + '-healthy', WAD, WAD)
        now = r.ctl('inspect')['latest_timestamp']
        markets = [n for n in r.active_markets() if now + 180 <= n <= now + 900]
        require(markets, 'no fresh maturity market inside the bounded scenario window')
        market = min(markets)
        case = {'market': market, 'stage': 'opening'}
        r.state['extra_scenarios'][label] = case
        r.save()
    if case['stage'] == 'opening':
        actor = label + '-borrower'
        if actor not in r.state['actors']:
            path = r.args.secret_dir / (actor + '-key.txt')
            generated = r.ctl('generate-key-file', '--output', path)
            r.state['actors'][actor] = {'address': generated['address'], 'key_file': str(path)}
            r.save()
        r.context[actor] = r.state['actors'][actor]['address']
        r.send(label + '-fund', r.state['actors'][actor]['address'], value=3 * WAD // 10)
        case['position'] = open_position(r, label, case['market'], scale=100, actor=actor)
        case['stage'] = 'opened'
        r.save()
    require(case['stage'] == 'opened', 'inspect previous scenario evidence; do not replay setup')
    position = case['position']
    key = (position['market'], position['borrower'])
    now = r.ctl('inspect')['latest_timestamp']
    grace = r.scalar(position['pool'], 'getLiquidationGracePeriod(uint64)', [position['market']])
    hf = r.scalar(position['pool'], 'getHealthFactor(uint64,address)', key)
    require(hf > WAD and now < position['market'] and position['market'] + grace - now < 1000, 'maturity starting condition invalid')
    require(r.scalar(position['pool'], 'isLiquidatable(uint64,address)', key) == 0, 'healthy position already eligible')
    baseline = r.scalar(r.context['asset'], 'balanceOf(address)', [r.context['executor']])
    # No oracle/lending writes after this observation: time alone triggers eligibility.
    case.update(stage='running', initial_hf=hf, grace=grace, observation_timestamp=now, baseline_asset=baseline)
    r.save()
    original = r.args.bot_timeout_seconds
    r.args.bot_timeout_seconds = min(1100, position['market'] + grace - now + 120)
    try:
        live = r.live_until(label)
    finally:
        r.args.bot_timeout_seconds = original
    finish_maturity(r, label, case)


def finish_maturity(r, label, case):
    position = case['position']
    live = json.loads((r.directory / (label + '-result.json')).read_text())
    require(live.get('passed') is True, 'previous live run was not accepted; inspect evidence before retry')
    report = audit(r, label, [position], case['baseline_asset'], live)
    receipt = report['actual'][0]['receipt']
    block = cast_json(r.cast('block', receipt['blockNumber'], '--rpc-url', r.args.rpc, '--json'))
    stamp = int(block['timestamp'], 0)
    require(case['initial_hf'] > WAD and case['observation_timestamp'] < position['market'], 'starting condition must precede maturity with HF > 1')
    require(stamp >= position['market'] + case['grace'], 'liquidation mined before maturity plus grace')
    report.update(initial_health_factor=str(case['initial_hf']), eligibility_time=position['market'] + case['grace'], receipt_timestamp=stamp,
                  no_lending_or_oracle_mutations_while_waiting=True)
    m.atomic_json(r.directory / (label + '-acceptance.json'), report)
    case.update(stage='passed', acceptance=str(r.directory / (label + '-acceptance.json')))
    r.save()
    print(json.dumps({'scenario': label, 'passed': True, 'initial_hf': str(case['initial_hf']), 'receipt_timestamp': stamp}))


def partial(r):
    label = 'partial-existing-pool'
    require(r.state.get('primary_liquidation_passed'), 'primary HF scenario must finish first')
    case = r.state.setdefault('extra_scenarios', {}).get(label)
    if case is None:
        previous = r.state['extra_scenarios'].get('maturity-only', {})
        if previous.get('position'):
            close_position(r, 'cleanup-before-partial', previous['position'])
        r.ensure_oracle(label + '-healthy', WAD, WAD)
        now = r.ctl('inspect')['latest_timestamp']
        markets = [n for n in r.active_markets() if now + 600 <= n <= now + 1800]
        require(markets, 'no sufficiently distant active market')
        case = {'market': min(markets), 'stage': 'opening'}
        r.state['extra_scenarios'][label] = case
        r.save()
    if case['stage'] == 'passed':
        print(json.dumps({'scenario': label, 'passed': True, 'already_audited': True}))
        return
    if case['stage'] == 'opening':
        case['position'] = open_position(r, label, case['market'], scale=10000)
        case['stage'] = 'opened'
        r.save()
    if case['stage'] == 'opened':
        r.ensure_oracle(label + '-unhealthy', 49 * WAD // 100, WAD)
        p = case['position']
        key = (p['market'], p['borrower'])
        require(r.scalar(p['pool'], 'getHealthFactor(uint64,address)', key) < WAD, 'partial target must have HF < 1')
        require(r.scalar(p['pool'], 'isLiquidatable(uint64,address)', key) == 1, 'partial target is not eligible')
        # Even selling ALL collateral cannot repay this full debt. This upper
        # bound is independent of the bot's chosen discount/repay-size maths.
        full_upper_bound = r.quote_input(r.context['collateral'], r.context['asset'], p['collateral'])
        require(full_upper_bound < p['debt'], 'full liquidation is not demonstrably unprofitable')
        case.update(stage='running', baseline_asset=r.scalar(r.context['asset'], 'balanceOf(address)', [r.context['executor']]),
                    full_sale_upper_bound=full_upper_bound)
        r.save()
        r.live_until(label)
    require(case['stage'] == 'running', 'inspect prior scenario state')
    live = json.loads((r.directory / (label + '-result.json')).read_text())
    positions = [dict(case['position'], expect_partial=True)]
    concurrent = False
    if live.get('passed') is not True:
        # Preserve the original observer failure. The first run also closed the
        # explicitly known, earlier small maturity position; audit BOTH receipts.
        require(live.get('reason') == 'more liquidations than the isolated scenario expected', 'previous partial live run failed')
        require(live['run']['exit_code'] == 0 and len(live['stopped']) == 1 and live['stopped'][0]['active_pending'] is False,
                'concurrent result lacks idle successful shutdown')
        previous = r.state['extra_scenarios']['maturity-only']['position']
        positions.append(dict(previous, expect_partial=False))
        concurrent = True
    report = audit(r, label, positions, case['baseline_asset'], live)
    receipt = report['actual'][0]['receipt']
    block = cast_json(r.cast('block', receipt['blockNumber'], '--rpc-url', r.args.rpc, '--json'))
    require(int(block['timestamp'], 0) < case['market'], 'partial proof crossed maturity')
    report['full_collateral_sale_upper_bound'] = str(case['full_sale_upper_bound'])
    report['earlier_matured_position_also_liquidated'] = concurrent
    report['initial_observer_count_mismatch_preserved'] = concurrent
    m.atomic_json(r.directory / (label + '-acceptance.json'), report)
    case.update(stage='passed', acceptance=str(r.directory / (label + '-acceptance.json')))
    r.save()
    print(json.dumps({'scenario': label, 'passed': True, 'remaining_debt': str(report['actual'][0]['remaining_debt'])}))


def close_position(r, label, position):
    p = position
    actor, account = p['actor'], p['borrower']
    key = (p['market'], account)
    values = r.state.setdefault('extra_cleanup', {}).get(label)
    if values and values.get('passed'):
        require(r.scalar(p['pool'], 'debtOf(uint64,address)', key) == 0, 'closed debt reappeared')
        require(r.scalar(p['pool'], 'collateralOf(uint64,address)', key) == 0, 'closed collateral reappeared')
        return
    if values is None:
        values = {'debt': r.scalar(p['pool'], 'debtOf(uint64,address)', key),
                  'collateral': r.scalar(p['pool'], 'collateralOf(uint64,address)', key)}
        r.state['extra_cleanup'][label] = values
        r.save()
    if values['debt']:
        debt = values['debt']
        r.send(label + '-approve', r.context['asset'], 'approve(address,uint256)', [p['pool'], debt], actor=actor)
        r.send(label + '-deposit', p['pool'], 'deposit(address,uint128)', [account, debt], actor=actor)
        r.send(label + '-repay', p['pool'], 'repay(address,address,uint64,uint128)', [account, account, p['market'], debt], actor=actor,
               after=[r.assertion(p['pool'], 'debtOf(uint64,address)', key, 'uint128', 0)])
    if values['collateral']:
        r.send(label + '-collateral', p['pool'], 'withdrawCollateral(address,uint64,address,uint128)',
               [account, p['market'], account, values['collateral']], actor=actor,
               after=[r.assertion(p['pool'], 'collateralOf(uint64,address)', key, 'uint128', 0)])
    require(r.scalar(p['pool'], 'debtOf(uint64,address)', key) == 0, 'cleanup debt remains')
    require(r.scalar(p['pool'], 'collateralOf(uint64,address)', key) == 0, 'cleanup collateral remains')
    values['passed'] = True
    r.save()


def second_pool(r):
    name = 'credit_pool_extra'
    if name in r.state['contracts']:
        return r.state['contracts'][name]
    item = next(item for item in r.template['deployments'] if item['id'] == 'credit_pool')
    encoded = r.encode_constructor('CreditPool', m.resolve(item['constructor'], r.context))
    artifact = r.directory / 'artifacts/credit_pool.json'
    constructor = r.directory / 'artifacts/credit_pool_extra.constructor.hex'
    if constructor.exists():
        require(constructor.read_text().strip() == encoded, 'extra pool constructor changed')
    constructor.write_text(encoded + '\n')
    digest = r.cast('keccak', '0x' + artifact.read_bytes().hex())
    request = {'contract': 'CreditPool', 'artifact_sha256': m.sha(artifact.read_bytes()), 'artifact_keccak': digest, 'constructor': encoded}
    result = r.run_step('deploy-' + name, request, ['deploy', '--id', 'deploy-' + name,
                       '--artifact', artifact, '--artifact-keccak', digest, '--constructor-file', constructor])
    require(result.get('contract_address') and result.get('runtime_code_hash'), 'extra pool identity missing')
    r.state['contracts'][name] = result
    r.context[name] = result['contract_address']
    r.save()
    return result


def restore_synthetic_pair_price(r, label):
    """Reuse the test AMM after a large partial liquidation, at its initial 1:1 price.

    Both tokens are owner-minted fixtures. This helper must never be used for
    production tokens. Persist the setup before signing so replay is idempotent.
    """
    records = r.state.setdefault('amm_resets', {})
    record = records.get(label)
    pool = r.state['v3_pools']['collateral_asset']
    target = 2**96
    if record is None:
        current = int(r.call(pool, 'slot0()')[2:66], 16)
        if current == target:
            return
        liquidity = r.scalar(pool, 'liquidity()')
        token0, token1 = r.address(pool, 'token0()'), r.address(pool, 'token1()')
        require({token0.lower(), token1.lower()} == {r.context['asset'].lower(), r.context['collateral'].lower()}, 'unexpected synthetic pair')
        token_in, token_out = (token0, token1) if current > target else (token1, token0)
        amount = ((liquidity * (current - target) + current - 1) // current if current > target
                  else (liquidity * (target - current) + target - 1) // target)
        amount = (amount * 1000 + 996) // 997 + WAD
        require(liquidity > 0 and 0 < amount <= 1_000_000 * WAD, 'synthetic AMM reset outside bound')
        record = {'token_in': token_in, 'token_out': token_out, 'amount': amount, 'before_sqrt': current}
        records[label] = record
        r.save()
    if record.get('passed'):
        return
    amount, token = record['amount'], record['token_in']
    r.send(label + '-mint', token, 'mint(address,uint256)', [r.context['owner'], amount])
    r.send(label + '-approve', token, 'approve(address,uint256)', [r.context['router'], amount])
    entry = next(item for item in r.contracts['SwapRouter02']['abi'] if item.get('name') == 'exactInputSingle')
    signature = 'exactInputSingle(' + m.abi_type(entry['inputs'][0]) + ')'
    arguments = m.abi_arg(entry['inputs'][0], [token, record['token_out'], 3000, r.context['owner'], amount, 1, target])
    r.send(label + '-swap', r.context['router'], signature, [arguments])
    r.send(label + '-revoke', token, 'approve(address,uint256)', [r.context['router'], 0])
    require(int(r.call(pool, 'slot0()')[2:66], 16) == target, 'synthetic AMM reset target not reached')
    record['passed'] = True
    r.save()


def multipool(r):
    label = 'two-pools-two-markets'
    require(r.state.get('extra_scenarios', {}).get('partial-existing-pool', {}).get('stage') == 'passed', 'partial audit must finish first')
    case = r.state['extra_scenarios'].get(label)
    if case and case['stage'] == 'passed':
        print(json.dumps({'scenario': label, 'passed': True, 'already_audited': True}))
        return
    if case is None:
        # Close prior scenario debt before changing the common oracle.
        for prior in ['partial-existing-pool', 'maturity-only', 'maturity-sized']:
            previous = r.state['extra_scenarios'].get(prior, {})
            if previous.get('position'):
                close_position(r, 'cleanup-extra-' + prior, previous['position'])
        restore_synthetic_pair_price(r, label + '-restore-price')
        r.ensure_oracle(label + '-healthy', WAD, WAD)
        extra = second_pool(r)
        r.send(label + '-register-test-pool', r.context['lending_factory'], 'registerPool(address)',
               [extra['contract_address']])
        for actor, amount in [('borrower', 6 * WAD // 10), ('lender', 4 * WAD // 10)]:
            r.send(label + '-fund-' + actor, r.context[actor], value=amount)
        targets = []
        original = r.context['credit_pool']
        try:
            for pool in [original, extra['contract_address']]:
                r.context['credit_pool'] = pool
                now = r.ctl('inspect')['latest_timestamp']
                markets = sorted(n for n in r.active_markets() if n >= now + 600)
                require(len(markets) >= 2, 'need two future markets per pool')
                targets.extend((pool, market) for market in markets[:2])
        finally:
            r.context['credit_pool'] = original
        case = {'stage': 'opening', 'targets': targets, 'positions': []}
        r.state['extra_scenarios'][label] = case
        r.save()
    if case['stage'] == 'opening':
        original = r.context['credit_pool']
        try:
            for i, (pool, market) in enumerate(case['targets']):
                if i < len(case['positions']):
                    continue
                r.context['credit_pool'] = pool
                p = open_position(r, label + '-' + str(i), market, scale=100)
                case['positions'].append(p)
                r.save()
        finally:
            r.context['credit_pool'] = original
        case['stage'] = 'opened'
        r.save()
    if case['stage'] == 'opened':
        r.ensure_oracle(label + '-unhealthy', 40 * WAD // 100, WAD)
        manifest = json.loads((r.directory / 'bot-manifest.json').read_text())
        extra = r.state['contracts']['credit_pool_extra']
        descriptor = json.loads(json.dumps(next(iter(manifest['pools'].values()))))
        descriptor['spec'].update(address=extra['contract_address'], deployment_block=extra['block_number'], deployment_block_hash=extra['block_hash'])
        descriptor['code_hash'] = extra['runtime_code_hash']
        manifest['pools']['second-test-pool'] = descriptor
        # Leave public RPC capacity for the independent setup/audit controller.
        manifest['rpc']['requests_per_second'] = 8
        for p in case['positions']:
            require(r.scalar(p['pool'], 'isLiquidatable(uint64,address)', [p['market'], p['borrower']]) == 1, 'target not eligible')
        before = r.ctl('inspect', actor='operator')
        balance = int(before['balance_wei'], 0)
        # Require a top-up while this very process is running. No funds are
        # removed; only the explicit reserve is raised for the negative phase.
        manifest['fees']['native_reserve'] = hex(balance + WAD // 20)
        manifest['max_total_native_spend'] = hex(balance + 2 * WAD)
        case['operator_funding_wei'] = 6 * WAD // 10
        def fund_after_block(health):
            if case.get('funding_observed') or not isinstance(health, dict):
                return
            if health.get('native_balance', {}).get('active_funding_sufficient') is not False:
                return
            if sum(p.get('eligible') or 0 for p in health.get('pools', [])) < 4:
                return
            current = r.ctl('inspect', actor='operator')
            require(current['latest_nonce'] == before['latest_nonce'] == current['pending_nonce'], 'bot sent before funding')
            require(current['balance_wei'] == before['balance_wei'], 'operator spent before funding')
            case['funding_blocked_health'] = health
            r.send(label + '-fund-operator-live', r.context['operator'], value=case['operator_funding_wei'])
            case['funding_observed'] = True
            r.save()
        case.update(stage='running', baseline_asset=r.scalar(r.context['asset'], 'balanceOf(address)', [r.context['executor']]))
        r.save()
        manifest_path = r.directory / (label + '-input.json')
        m.atomic_json(manifest_path, manifest)
        r.live_until(label, expected=4, manifest=manifest_path, on_health=fund_after_block)
    require(case['stage'] == 'running', 'inspect previous multi-pool run')
    live = json.loads((r.directory / (label + '-result.json')).read_text())
    require(live.get('passed') is True, 'multi-pool live run failed; do not resend blindly')
    require(case.get('funding_observed') is True, 'live funding recovery was not observed')
    report = audit(r, label, case['positions'], case['baseline_asset'], live, funding=case['operator_funding_wei'])
    report['funding_recovery_without_restart'] = True
    m.atomic_json(r.directory / (label + '-acceptance.json'), report)
    case.update(stage='passed', acceptance=str(r.directory / (label + '-acceptance.json')))
    r.save()
    print(json.dumps({'scenario': label, 'passed': True, 'distinct_positions': len(case['positions'])}))


def cleanup(r):
    """Close extra scenarios before the primary runner removes AMM liquidity."""
    require(r.bot_journal()[1]['active'] is None, 'resolve the last bot run before cleanup')
    positions = {}
    for case in r.state.get('extra_scenarios', {}).values():
        for p in ([case['position']] if case.get('position') else []) + case.get('positions', []):
            positions[(p['pool'].lower(), p['market'], p['borrower'].lower())] = p
    for key, p in sorted(positions.items()):
        label = 'cleanup-extra-' + m.sha(json.dumps(key).encode())[:16]
        close_position(r, label, p)
    primary = (r.context['credit_pool'].lower(), r.state['scenario_market'])
    markets = sorted({(pool, market) for pool, market, _ in positions} - {primary})
    deadline = time.monotonic() + min(r.args.market_wait_seconds, 600)
    while markets:
        now = r.ctl('inspect')['latest_timestamp']
        remaining = max(market - now for _, market in markets)
        if remaining <= 0:
            break
        if not r.args.wait_market or time.monotonic() >= deadline:
            print(json.dumps({'cleanup': 'pending_actual_extra_maturity', 'seconds': remaining,
                              'liquidity_removed': False}))
            return
        time.sleep(min(30, remaining, max(0, deadline - time.monotonic())))
    lender = r.context['lender']
    for pool, market in markets:
        loan = r.scalar(pool, 'loanOf(uint64,address)', [market, lender])
        if loan:
            require(r.scalar(pool, 'getMarketBalance(uint64)', [market]) >= loan, 'extra market loan not fully available')
            label = 'cleanup-claim-' + m.sha(json.dumps([pool, market]).encode())[:16]
            r.send(label, pool, 'claim(address,uint64,uint128)', [lender, market, loan], actor='lender',
                   after=[r.assertion(pool, 'loanOf(uint64,address)', [market, lender], 'uint128', 0)])
    for pool in sorted({pool for pool, _ in markets}):
        balance = r.scalar(pool, 'balanceOf(address)', [lender])
        if balance:
            r.send('cleanup-idle-' + pool[2:18], pool, 'withdraw(address,address,uint128)',
                   [lender, lender, balance], actor='lender',
                   after=[r.assertion(pool, 'balanceOf(address)', [lender], 'uint128', 0)])
    r.cleanup()
    for name in ['asset', 'collateral', 'wrapped_native']:
        if r.scalar(r.context[name], 'allowance(address,address)', [r.context['owner'], r.context['bootstrap']]):
            r.send('cleanup-bootstrap-approval-' + name, r.context[name], 'approve(address,uint256)', [r.context['bootstrap'], 0])
    print(json.dumps({'cleanup': 'passed', 'extra_positions': len(positions), 'extra_markets': len(markets)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--common-args', type=pathlib.Path, required=True)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--scenario', choices=['maturity', 'partial', 'multipool', 'cleanup'], required=True)
    args = parser.parse_args()
    require(args.execute, 'explicit --execute is required')
    common = json.loads(args.common_args.read_text())
    sys.argv = ['runner', *common]
    config = m.parse_args()
    require(config.execute and config.owner_key_file is not None, 'explicit testnet owner inputs required')
    r = m.Runner(config)
    with (r.directory / 'orchestrator.lock').open('a+') as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        {'maturity': maturity, 'partial': partial, 'multipool': multipool, 'cleanup': cleanup}[args.scenario](r)


if __name__ == '__main__':
    main()
