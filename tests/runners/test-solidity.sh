#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_forge="${FORGE:-forge}"
task_solc="${SOLC:-solc}"
"$task_forge" --version | python3 -c 'import sys; sys.exit(0 if "Version: 1.8.4" in sys.stdin.read() else "Foundry 1.8.4 required")'
"$task_solc" --version | python3 -c 'import sys; sys.exit(0 if "Version: 0.8.31+" in sys.stdin.read() else "solc 0.8.31 required")'
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
"$task_forge" test --root "$task_repo" --use "$task_solc" --offline --network monad -vv "$@"
