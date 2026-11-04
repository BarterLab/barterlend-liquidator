#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export PYTHONDONTWRITEBYTECODE=1
python3 scripts/check-release.py
python3 scripts/verify-solidity-fixtures.py
python3 -m unittest discover -s tests/python
cargo +1.98.0 test --locked
cargo +1.98.0 clippy --locked --all-targets --no-deps -- -D clippy::await_holding_lock -D clippy::suspicious
