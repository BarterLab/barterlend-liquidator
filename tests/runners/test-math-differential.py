#!/usr/bin/env python3
"""Local Solidity fuzz to Rust differential checks, without RPC, downloads or pin changes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def verify_protocol_inputs(repo, provenance):
    fixture = repo / 'tests/fixtures/solidity'
    manifest = fixture / 'PROTOCOL_INPUTS.json'
    require(digest(manifest) == provenance['source_input_manifest_sha256'],
            'pinned protocol manifest changed')
    protocol = fixture / 'protocol-v5'
    inputs = json.loads(manifest.read_text())['files']
    for entry in inputs:
        path = protocol / entry['path']
        require(path.is_file(), 'Private integration sources required: use '
                'scripts/with-protocol-fixtures.py; missing ' + entry['path'])
        require(digest(path) == entry['sha256'], 'pinned protocol input changed: ' + entry['path'])
    return protocol, manifest, inputs


def verify_rust_comparator_log(contents, expected_runs):
    name = 'actual_solidity_fuzz_corpus_matches_rust'
    require(len(re.findall(r'^test ' + name + r' \.\.\. ok$', contents, re.MULTILINE)) == 1,
            'the exact Rust comparator must execute and pass exactly once')
    require(len(re.findall(r'^test result: ok\. 1 passed; 0 failed; 0 ignored;', contents, re.MULTILINE)) == 1,
            'Rust must report exactly one passed, non-ignored test')
    marker = f'Compared {expected_runs} actual Solidity fuzz executions with Rust repayment/seizure/remainders/bad-debt/error selectors'
    require(contents.splitlines().count(marker) == 1,
            'Rust must explicitly confirm comparison of every generated fuzz execution')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', required=True, type=Path)
    parser.add_argument('--runs', default=4096, type=int)
    parser.add_argument('--seed', default='0x20261007')
    args = parser.parse_args()
    if not 1 <= args.runs <= 16_384:
        parser.error('--runs must be 1..16384')
    if not args.seed.startswith('0x') or not 1 <= len(args.seed[2:]) <= 64:
        parser.error('--seed must be a hexadecimal uint256')
    try:
        int(args.seed, 16)
    except ValueError:
        parser.error('--seed must be a hexadecimal uint256')

    repo = Path(__file__).resolve().parents[2]
    fixture = repo / 'tests/fixtures/math-v5'
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    corpus = output / 'math-fuzz-actual.jsonl'
    if corpus.exists():
        parser.error('use a fresh output directory; existing evidence is never overwritten')
    forge = os.environ.get('FORGE', 'forge')
    solc = os.environ.get('SOLC', 'solc')
    cargo = os.environ.get('CARGO', 'cargo')
    require('Version: 1.8.4' in subprocess.check_output([forge, '--version'], text=True), 'Foundry 1.8.4 required')
    require('Version: 0.8.31+' in subprocess.check_output([solc, '--version'], text=True), 'solc 0.8.31 required')
    provenance = json.loads((fixture / 'PROVENANCE.json').read_text())
    generator = fixture / 'solidity/LiquidationMathFixtures.t.sol'
    require(digest(generator) == provenance['generator_sha256'], 'pinned math generator changed')
    require(digest(fixture / 'generated/actual-liquidations.json') == provenance['goldens_sha256'], 'pinned math goldens changed')
    protocol, _, inputs = verify_protocol_inputs(repo, provenance)

    report = {'scope': 'offline-actual-solidity-fuzz-to-rust', 'runs': args.runs,
              'seed': args.seed, 'public_rpc_used': False, 'pins_modified': False,
              'pinned_protocol_inputs': len(inputs), 'commands': [],
              'source_sha256': {'generator': digest(generator),
                                'rust_comparator': digest(repo / 'tests/math_v5.rs'),
                                'rust_math': digest(repo / 'src/liquidation_math.rs'),
                                'runner': digest(Path(__file__))}}
    report_path = output / 'math-fuzz-acceptance.json'

    def save():
        report_path.write_text(json.dumps(report, indent=2) + '\n')

    def run(name, command, cwd, env=None):
        log = output / (name + '.log')
        started = time.monotonic()
        with log.open('w') as stream:
            result = subprocess.run(command, cwd=cwd, env=env,
                                    stdout=stream, stderr=subprocess.STDOUT)
        record = {'command': command, 'cwd': str(cwd), 'exit_code': result.returncode,
                  'seconds': round(time.monotonic() - started, 3),
                  'log': str(log), 'sha256': digest(log)}
        report['commands'].append(record)
        save()
        print(json.dumps(record), flush=True)
        if result.returncode:
            raise SystemExit(result.returncode)

    with tempfile.TemporaryDirectory(prefix='barterlend-math-differential-') as directory:
        stage = Path(directory)
        (stage / 'solidity').mkdir()
        (stage / 'generated').mkdir()
        config = (fixture / 'foundry.toml').read_text().replace(
            '../solidity/protocol-v5', str(protocol.resolve()))
        (stage / 'foundry.toml').write_text(config)
        source = generator.read_text()
        original = '_case("fuzz", numerator, denominator, requested, minimum, 250e18);'
        require(source.count(original) == 1, 'instrumentation anchor changed')
        instrumented = source.replace(original,
            'string memory record = _case("fuzz", numerator, denominator, requested, minimum, 250e18);\n'
            '        vm.writeLine("./generated/fuzz-liquidations.jsonl", record);')
        staged_generator = stage / 'solidity/LiquidationMathFixtures.t.sol'
        staged_generator.write_text(instrumented)
        report['instrumented_generator_sha256'] = digest(staged_generator)
        report['instrumentation'] = 'Only append observed _case return JSON after its actual liquidate/conservation checks; original pinned generator is unchanged.'
        run('math-fuzz-solidity', [forge, 'test', '--root', str(stage), '--use', solc,
            '--offline', '--network', 'monad', '--match-contract', 'LiquidationMathFixtures',
            '--match-test', 'test_fuzz_real_liquidation_limits', '--fuzz-runs', str(args.runs),
            '--fuzz-seed', args.seed, '-vv'], stage)
        generated = stage / 'generated/fuzz-liquidations.jsonl'
        records = generated.read_text().splitlines()
        require(len(records) == args.runs, f'expected {args.runs} actual runs, observed {len(records)}')
        observed = [json.loads(line) for line in records]
        shutil.copyfile(generated, corpus)
        report['corpus'] = {'path': str(corpus), 'sha256': digest(corpus),
                            'records': len(observed),
                            'success': sum(row['ok'] for row in observed),
                            'revert': sum(not row['ok'] for row in observed)}
        save()
        env = os.environ.copy()
        env['BARTERLEND_MATH_FUZZ_CORPUS'] = str(corpus)
        env['BARTERLEND_MATH_FUZZ_RUNS'] = str(args.runs)
        env['CARGO_TERM_COLOR'] = 'never'
        run('math-fuzz-rust', [cargo, '+'+os.environ.get('BARTERLEND_RUST_TOOLCHAIN','1.98.0'), 'test', '--offline', '--locked',
            '--manifest-path', str(repo / 'Cargo.toml'), '--test', 'math_v5',
            'actual_solidity_fuzz_corpus_matches_rust', '--', '--ignored', '--exact', '--nocapture'], repo, env)
        verify_rust_comparator_log((output / 'math-fuzz-rust.log').read_text(), args.runs)
        require(digest(generator) == provenance['generator_sha256'], 'pinned math generator changed during run')
        require(digest(fixture / 'generated/actual-liquidations.json') == provenance['goldens_sha256'], 'pinned math goldens changed during run')
        require(digest(repo / 'tests/math_v5.rs') == report['source_sha256']['rust_comparator'], 'Rust comparator changed during run')
        require(digest(repo / 'src/liquidation_math.rs') == report['source_sha256']['rust_math'], 'Rust math changed during run')
        require(digest(Path(__file__)) == report['source_sha256']['runner'], 'runner changed during run')
        verify_protocol_inputs(repo, provenance)
        report['rust_comparator_verified'] = {'tests_passed': 1, 'compared_records': args.runs}
        report['passed'] = True
        save()
        print(f'PASS: {len(observed)} actual Solidity fuzz executions match Rust; pins unchanged', flush=True)


if __name__ == '__main__':
    main()
