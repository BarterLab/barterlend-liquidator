#!/usr/bin/env python3
"""Differential-runner attestation regressions, also run with python -O."""
from pathlib import Path
import hashlib
import json
import runpy
import tempfile
import unittest


runner = runpy.run_path(str(Path(__file__).resolve().parents[2] / 'tests/runners/test-math-differential.py'))
verify = runner['verify_rust_comparator_log']


def successful_log(runs=4096):
    return (
        'running 1 test\n'
        f'Compared {runs} actual Solidity fuzz executions with Rust repayment/seizure/remainders/bad-debt/error selectors\n'
        'test actual_solidity_fuzz_corpus_matches_rust ... ok\n'
        'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out; finished in 0.07s\n'
    )


class DifferentialRunnerAttestation(unittest.TestCase):
    def test_exact_successful_comparator_and_count_are_accepted(self):
        verify(successful_log(), 4096)

    def test_zero_matched_or_ignored_test_cannot_be_a_success(self):
        logs = [
            'running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 15 filtered out\n',
            successful_log().replace(' ... ok', ' ... ignored'),
            successful_log().replace('1 passed; 0 failed; 0 ignored', '0 passed; 0 failed; 1 ignored'),
        ]
        for contents in logs:
            with self.subTest(contents=contents), self.assertRaises(RuntimeError):
                verify(contents, 4096)

    def test_missing_duplicate_or_wrong_record_confirmation_is_rejected(self):
        for contents in [successful_log(4095), successful_log()+successful_log(),
                         '\n'.join(line for line in successful_log().splitlines() if not line.startswith('Compared '))]:
            with self.subTest(contents=contents), self.assertRaises(RuntimeError):
                verify(contents, 4096)

    def test_integrity_failure_is_an_exception_even_under_python_optimization(self):
        with self.assertRaisesRegex(RuntimeError, 'pinned input changed'):
            runner['require'](False, 'pinned input changed')


class ProtocolFixtureResolution(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='liquidator-math-inputs-')
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name) / 'bot'
        fixture = self.repo / 'tests/fixtures/solidity'
        self.source = fixture / 'protocol-v5/src/Pool.sol'
        self.source.parent.mkdir(parents=True)
        self.source.write_text('pinned source')
        self.manifest = fixture / 'PROTOCOL_INPUTS.json'
        self.manifest.write_text(json.dumps({'files': [{'path': 'src/Pool.sol',
            'sha256': hashlib.sha256(self.source.read_bytes()).hexdigest()}]}))
        self.provenance = {'source_input_manifest_sha256':
            hashlib.sha256(self.manifest.read_bytes()).hexdigest()}

    def test_uses_bot_fixture_without_a_sibling_sdk(self):
        protocol, manifest, inputs = runner['verify_protocol_inputs'](self.repo, self.provenance)
        self.assertEqual(protocol / inputs[0]['path'], self.source)
        self.assertEqual(manifest, self.manifest)
        self.source.write_text('changed source')
        with self.assertRaisesRegex(RuntimeError, 'pinned protocol input changed'):
            runner['verify_protocol_inputs'](self.repo, self.provenance)

    def test_missing_private_inputs_explain_how_to_supply_them(self):
        self.source.unlink()
        with self.assertRaisesRegex(RuntimeError, 'scripts/with-protocol-fixtures.py'):
            runner['verify_protocol_inputs'](self.repo, self.provenance)

    def test_replaced_manifest_is_rejected(self):
        self.manifest.write_text('{"files": []}')
        with self.assertRaisesRegex(RuntimeError, 'pinned protocol manifest changed'):
            runner['verify_protocol_inputs'](self.repo, self.provenance)


if __name__ == '__main__':
    unittest.main()
