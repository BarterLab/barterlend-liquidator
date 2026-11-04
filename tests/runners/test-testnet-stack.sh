#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_forge="${FORGE:-forge}"
task_solc="${SOLC:-solc}"
task_output="${1:?Choose a fresh output directory}"
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
python3 "$task_repo/tests/runners/build-testnet-artifacts.py" --output-dir "$task_output" --permit-only
task_output="$(cd "$task_output" && pwd)"
export BL_TESTNET_PERMIT_CREATION="$(python3 - "$task_output/Permit2.json" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))['bytecode'])
PY
)"
export FOUNDRY_TEST="$task_repo/tests/testnet"
cd "$task_repo"
python3 - "$task_repo" "$task_output" <<'PY'
import importlib.util,json,pathlib,sys
repo=pathlib.Path(sys.argv[1]); out=pathlib.Path(sys.argv[2])
spec=importlib.util.spec_from_file_location('testnet_artifacts',repo/'tests/runners/build-testnet-artifacts.py')
module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
(out/'inputs-before.json').write_text(json.dumps(module.input_hashes(),indent=2)+'\n')
PY
set +e
"$task_forge" test --root "$task_repo" --use "$task_solc" --offline --network monad --match-contract '^TestnetStackTest$' --out "$task_output/solidity-out" --cache-path "$task_output/solidity-cache" -vv >"$task_output/test.log" 2>&1
task_code=$?
set -e
python3 - "$task_output" "$task_code" "$task_repo" "$task_forge" "$task_solc" <<'PY'
import hashlib,importlib.util,json,pathlib,re,sys
out=pathlib.Path(sys.argv[1]); code=int(sys.argv[2]); repo=pathlib.Path(sys.argv[3])
log=(out/'test.log').read_text()
names=re.findall(r'^\[PASS\] (test_testnet_\S+) \(',log,re.M)
failed=re.findall(r'^\[FAIL[^\]]*\]',log,re.M)
spec=importlib.util.spec_from_file_location('testnet_artifacts',repo/'tests/runners/build-testnet-artifacts.py')
module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
source=module.input_hashes(); before=json.loads((out/'inputs-before.json').read_text())
(out/'inputs-after.json').write_text(json.dumps(source,indent=2)+'\n')
unchanged=source==before
command=[sys.argv[4],'test','--root',str(repo),'--use',sys.argv[5],'--offline','--network','monad','--match-contract','^TestnetStackTest$','--out',str(out/'solidity-out'),'--cache-path',str(out/'solidity-cache'),'-vv']
report={'passed':code==0 and len(names)==7 and not failed and unchanged,'network':'none','target_chain_id':10143,'test_count':len(names),'test_names':names,'exit_code':code,'command':command,'public_env':{'FOUNDRY_TEST':str(repo/'tests/testnet'),'permit_creation_sha256':hashlib.sha256(bytes.fromhex(json.loads((out/'Permit2.json').read_text())['bytecode'][2:])).hexdigest()},'log_sha256':hashlib.sha256((out/'test.log').read_bytes()).hexdigest(),'source_sha256':source,'source_inputs_unchanged':unchanged,'forge':sys.argv[4],'solc':sys.argv[5],'scope':'In-process Monad EVM deployable stack. EOAs/time/native funding use local-only fixture cheatcodes; deployable dependencies use none.'}
(out/'acceptance.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:report[k] for k in ['passed','network','test_count','exit_code']}))
if not report['passed']: raise SystemExit(1)
PY
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
