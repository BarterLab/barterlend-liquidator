#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_anvil="${ANVIL:-anvil}"
task_cargo="${CARGO:-cargo}"
task_selected="${1:-all}"
case "$task_selected" in
  partial-current|maturity-current|reorg-current|oracle-guards-current|cache-io-current)
    export BARTERLEND_LOCAL_CURRENT_STATE=1
    task_selected="${task_selected%-current}"
    ;;
esac
if [[ "$#" -gt 1 ]]; then echo "usage: $0 [scenario-tag|all]" >&2; exit 2; fi
task_matched=0
"$task_anvil" --version | python3 -c 'import sys; sys.exit(0 if "Version: 1.8.4" in sys.stdin.read() else "Anvil1.8.4 required")'
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
task_work="$(mktemp -d "${TMPDIR:-/tmp}/barterlend-local-bot.XXXXXX")"
task_pid=""
cleanup() {
  if [[ -n "$task_pid" ]]; then kill "$task_pid" 2>/dev/null || true; wait "$task_pid" 2>/dev/null || true; fi
  rm -rf "$task_work"
}
trap cleanup EXIT INT TERM
export BARTERLEND_LOCAL_MANIFEST="$task_repo/tests/fixtures/solidity/generated/manifest.json"
start_fresh_node() {
  if [[ -n "$task_pid" ]]; then kill "$task_pid"; wait "$task_pid" 2>/dev/null || true; task_pid=""; fi
  task_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
  local task_mining_args=(--silent)
  if [[ -n "${2:-}" ]]; then task_mining_args+=(--block-time "$2"); fi
  "$task_anvil" --host 127.0.0.1 --port "$task_port" --network monad --chain-id 31337 --load-state "$task_repo/tests/fixtures/solidity/generated/anvil-state.json" "${task_mining_args[@]}" > "$task_work/anvil-$1.log" 2>&1 &
  task_pid=$!
  export BARTERLEND_LOCAL_RPC_URL="http://127.0.0.1:$task_port"
  BARTERLEND_INTERVAL_MINING="${2:-}" python3 - <<'PY'
import os,json,time,urllib.request
url=os.environ['BARTERLEND_LOCAL_RPC_URL']
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
for _ in range(100):
    try:
        request=urllib.request.Request(url,data=b'{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}',headers={'Content-Type':'application/json'})
        response=json.load(opener.open(request,timeout=1))
        if not os.environ['BARTERLEND_INTERVAL_MINING'] and response.get('result') != '0x0':
            raise SystemExit('each scenario requires a fresh node')
        break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit('isolated seeded Anvil did not start')
PY
}
run_scenario() {
  if [[ "$task_selected" != "all" && "$task_selected" != "$1" ]]; then return; fi
  task_matched=1
  start_fresh_node "$1"
  if [[ -n "${3:-}" ]]; then
    python3 "$task_repo/scripts/bounded-test.py" "$task_cargo" +"${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" test --offline --locked --manifest-path "$task_repo/Cargo.toml" --test "$2" "$3" -- --ignored --exact --nocapture
  else
    python3 "$task_repo/scripts/bounded-test.py" "$task_cargo" +"${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" test --offline --locked --manifest-path "$task_repo/Cargo.toml" --test "$2" -- --ignored --nocapture
  fi
}
if [[ "$task_selected" == "all" || "$task_selected" == "public-rate" ]]; then
  task_matched=1
  start_fresh_node public-rate 0.3
  python3 "$task_repo/scripts/bounded-test.py" "$task_cargo" +"${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" test --offline --locked --manifest-path "$task_repo/Cargo.toml" --lib \
    app::rate_tests::rate_example_full_preparation_survives_public_rate -- --ignored --exact --nocapture
fi
run_scenario healthy local_bot
run_scenario routes routes_local_v3
run_scenario faults app_faults_local
run_scenario partial app_liquidation_scenarios_local whole_app_rejects_unprofitable_full_and_finalizes_profitable_partial
run_scenario maturity app_liquidation_scenarios_local whole_app_maturity_transition_without_lending_events_finalizes_liquidation
run_scenario oracle-guards app_liquidation_scenarios_local whole_app_healthy_invalid_and_stale_oracle_never_sign_then_fresh_price_recovers
run_scenario financial-guards app_liquidation_scenarios_local whole_app_route_native_profit_and_fee_guards_never_sign_or_spend
run_scenario lender-fill-withdraw app_liquidation_scenarios_local whole_app_borrower_fills_lender_order_then_owner_withdraws_actual_profit
run_scenario cli-signals app_cli_unsigned_shutdown_local
if [[ "$task_matched" == 0 ]]; then echo "unknown local scenario: $task_selected" >&2; exit 2; fi
