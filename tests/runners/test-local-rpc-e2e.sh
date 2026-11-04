#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_anvil="${ANVIL:-anvil}"
task_cargo="${CARGO:-cargo}"
task_selected="${1:-full}"
if [[ "$#" -gt 1 || "$task_selected" != "full" && "$task_selected" != "preparation" ]]; then
  echo "usage: $0 [full|preparation]" >&2; exit 2
fi
"$task_anvil" --version | python3 -c 'import sys; sys.exit(0 if "Version: 1.8.4" in sys.stdin.read() else "Anvil 1.8.4 required")'
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
task_work="$(mktemp -d "${TMPDIR:-/tmp}/barterlend-rpc-e2e.XXXXXX")"
task_pid=""
cleanup() {
  if [[ -n "$task_pid" ]]; then kill "$task_pid" 2>/dev/null || true; wait "$task_pid" 2>/dev/null || true; fi
  rm -rf "$task_work"
}
trap cleanup EXIT INT TERM
task_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
"$task_anvil" --host 127.0.0.1 --port "$task_port" --network monad --chain-id 31337 \
  --load-state "$task_repo/tests/fixtures/solidity/generated/anvil-state.json" --block-time 0.3 --silent > "$task_work/anvil.log" 2>&1 &
task_pid=$!
export BARTERLEND_LOCAL_RPC_URL="http://127.0.0.1:$task_port"
export BARTERLEND_LOCAL_MANIFEST="$task_repo/tests/fixtures/solidity/generated/manifest.json"
python3 - <<'PY'
import os,json,time,urllib.request
url=os.environ['BARTERLEND_LOCAL_RPC_URL']
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
for _ in range(100):
    try:
        request=urllib.request.Request(url,data=b'{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}',headers={'Content-Type':'application/json'})
        response=json.load(opener.open(request,timeout=1))
        if response.get('result')!='0x7a69': raise SystemExit('unexpected chain; isolated 31337 required')
        break
    except OSError: time.sleep(.05)
else: raise SystemExit('isolated Anvil did not start')
PY
if [[ "$task_selected" == "full" ]]; then
  task_test="app::rate_tests::full_bot_public_rpc_budget_finalizes_two_markets_and_restarts_without_duplicate_send"
else
  task_test="app::rate_tests::rate_example_full_preparation_survives_public_rate"
fi
python3 "$task_repo/scripts/bounded-test.py" "$task_cargo" +"${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" test --offline --locked --manifest-path "$task_repo/Cargo.toml" --lib "$task_test" -- --ignored --exact --nocapture
