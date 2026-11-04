#!/usr/bin/env python3
"""Regression checks for source and generated-state fixture provenance."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


class FixtureProvenanceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="liquidator-fixture-provenance-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        script = Path(__file__).resolve().parents[2] / 'scripts/verify-solidity-fixtures.py'
        (self.repo / "scripts").mkdir()
        shutil.copyfile(script, self.repo / "scripts" / script.name)
        self.fixture = self.repo / "tests/fixtures/solidity"
        (self.fixture / "protocol-v5/src").mkdir(parents=True)
        (self.fixture / "generated").mkdir()
        (self.repo / "tests/fixtures/uniswap-v3").mkdir(parents=True)
        self.source = self.fixture / "protocol-v5/src/Pool.sol"
        self.source.write_text("verified protocol source")
        self.state = self.fixture / "generated/anvil-state.json"
        self.state.write_text('{"accounts":{}}\n')
        self.write_json(self.fixture / "PROTOCOL_INPUTS.json", {"files": [
            {"path": "src/Pool.sol", "sha256": self.digest(self.source)}]})
        self.write_json(self.fixture / "SUPPLEMENTAL_INPUTS.json", {"files": []})
        self.write_json(self.repo / "tests/fixtures/uniswap-v3/MANIFEST.json", [])
        self.write_json(self.repo / "tests/fixtures/multicall3-PROVENANCE.json", {"files": {}})
        self.write_json(self.fixture / "generated/SHA256.json", {
            "files": [{"path": "anvil-state.json", "sha256": self.digest(self.state)}],
            "source_sha256": {str(self.source.relative_to(self.repo)): self.digest(self.source)}})

    @staticmethod
    def digest(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    @staticmethod
    def write_json(path, contents):
        path.write_text(json.dumps(contents) + "\n")

    def run_verifier(self, *args):
        return subprocess.run([sys.executable, self.repo / "scripts/verify-solidity-fixtures.py", *args],
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def test_matching_sources_and_generated_state_pass(self):
        self.assertEqual(self.run_verifier().returncode, 0)

    def test_repinning_protocol_without_regenerating_state_is_rejected(self):
        self.source.write_text("new protocol source")
        self.write_json(self.fixture / "PROTOCOL_INPUTS.json", {"files": [
            {"path": "src/Pool.sol", "sha256": self.digest(self.source)}]})
        result = self.run_verifier()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Generated fixture source changed", result.stdout)
        self.assertEqual(self.run_verifier("--inputs-only").returncode, 0)

    def test_tampered_generated_state_is_rejected(self):
        self.state.write_text('{"accounts":{"unexpected":{}}}\n')
        result = self.run_verifier()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Generated fixture changed", result.stdout)

    def test_private_sources_are_optional_for_bytecode_checks_but_required_for_builds(self):
        self.source.unlink()
        result=self.run_verifier()
        self.assertEqual(result.returncode,0)
        self.assertIn("private source checks unavailable",result.stdout)
        self.assertNotEqual(self.run_verifier("--inputs-only").returncode,0)
        self.write_json(self.fixture / "PROTOCOL_INPUTS.json", {"files": [
            {"path":"src/Pool.sol","sha256":"00"*32}]})
        self.assertIn("Generated fixture source changed",self.run_verifier().stdout)

    def test_missing_source_provenance_requires_regeneration(self):
        self.write_json(self.fixture / "generated/SHA256.json", {"files": []})
        result = self.run_verifier()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source provenance missing", result.stdout)


if __name__ == "__main__":
    unittest.main()
