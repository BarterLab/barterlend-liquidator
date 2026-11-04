#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_output="${1:?Choose a fresh evidence directory}"
task_forge="${FORGE:-forge}"
task_solc="${SOLC:-solc}"
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
python3 - "$task_repo" "$task_output" "$task_forge" "$task_solc" <<'PY'
import hashlib,json,pathlib,re,shutil,subprocess,sys,time
repo=pathlib.Path(sys.argv[1]);out=pathlib.Path(sys.argv[2]).resolve();out.mkdir(parents=True,exist_ok=False)
stage=out/'staged';stage.mkdir();fixture=repo/'tests/fixtures/upstream-compat';pins=json.loads((fixture/'SOURCE_PINS.json').read_text())
digest=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for rel,expected in pins['files'].items():
    if digest(fixture/rel)!=expected:raise SystemExit('Copied upstream fingerprint differs: '+rel)
inputs={str(p.relative_to(repo)):digest(p)for p in [repo/'tests/solidity/ExecutorFixture.sol',repo/'tests/fixtures/factory/TestLendingFactory.sol',repo/'tests/solidity/ExecutorUpstreamCompatibility.t.sol',repo/'contracts/KyberRoute.sol',repo/'contracts/LiquidationExecutor.sol',repo/'contracts/IExecutorV4.sol',fixture/'SOURCE_PINS.json']}
inputs.update({str((fixture/rel).relative_to(repo)):digest(fixture/rel)for rel in pins['files']})
inputs.update({str(p.relative_to(repo)):digest(p)for p in (repo/'tests/fixtures/uniswap-v3').glob('*.json')})
for name in ['ExecutorFixture.sol','ExecutorUpstreamCompatibility.t.sol']:
    (stage/'test').mkdir(exist_ok=True);(stage/'test'/name).write_text((repo/'tests/solidity'/name).read_text().replace('../../contracts/','../contracts/'))
for rel in pins['files']:
    if rel=='bots/liquidator-bot/test/UniswapV3LiquidationHelper.t.sol':dest=stage/'test/LegacyUniswapV3LiquidationHelper.t.sol'
    else:dest=stage/rel
    dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(fixture/rel,dest)
(stage/'contracts').mkdir()
for name in ['LiquidationExecutor.sol','IExecutorV4.sol','KyberRoute.sol']:shutil.copyfile(repo/'contracts'/name,stage/'contracts'/name)
shutil.copytree(repo/'tests/fixtures/factory',stage/'fixtures/factory')
shutil.copytree(repo/'tests/fixtures/uniswap-v3',stage/'tests/fixtures/uniswap-v3')
base=repo/'tests/fixtures/solidity/protocol-v5'
remaps=['lib/='+str(base/'lib')+'/','src/='+str(base/'src')+'/','test/mocks/='+str(base/'test/mocks')+'/','forge-std/='+str(base/'lib/forge-std/src')+'/']
(stage/'foundry.toml').write_text('[profile.default]\nsrc="contracts"\ntest="test"\nlibs=[]\noptimizer=true\noptimizer_runs=200\nvia_ir=false\nevm_version="osaka"\ngas_limit="18446744073709551615"\nremappings='+json.dumps(remaps)+'\nfs_permissions=[{access="read",path="tests/fixtures/uniswap-v3"}]\n')
expected=[];counts=[]
for rel in ['test/CreditPool/11_liquidations.t.sol','test/CreditPool/29_liquidation_discount.t.sol','bots/liquidator-bot/test/UniswapV3LiquidationHelper.t.sol']:
    names=re.findall(r'function\s+(test_\w+)\s*\(', (fixture/rel).read_text());expected.extend(names);counts.append(len(names))
new_names=re.findall(r'function\s+(test_\w+)\s*\(', (repo/'tests/solidity/ExecutorUpstreamCompatibility.t.sol').read_text());expected+=new_names
if any(count == 0 for count in counts) or not new_names or len(set(expected)) != len(expected):raise SystemExit("Expected nonempty distinct pinned protocol/helper/executor cases")
command=[sys.argv[3],'test','--root',str(stage),'--use',sys.argv[4],'--offline','--out',str(out/'solidity-out'),'--cache-path',str(out/'solidity-cache'),'--match-contract','^(ExecutorUpstreamCompatibility|TestCreditPool_liquidations|TestCreditPool_collateral|TestCreditPool_liquidation_discount|TestUniswapV3LiquidationHelper)$','--fuzz-runs','1024','--fuzz-seed','0x20261014','-vv']
(out/'inputs-before.json').write_text(json.dumps(inputs,indent=2)+'\n');started=time.monotonic()
with (out/'test.log').open('w')as log:result=subprocess.run(command,cwd=stage,stdout=log,stderr=subprocess.STDOUT)
text=(out/'test.log').read_text();passed=re.findall(r'^\[PASS\]\s+(test_\w+)\(',text,re.M);failed=re.findall(r'^\[FAIL[^\]]*\]',text,re.M)
after={name:digest(repo/name)for name in inputs};(out/'inputs-after.json').write_text(json.dumps(after,indent=2)+'\n');unchanged=after==inputs
report={'protocol_head':pins['snapshot_head'],'legacy_helper_head':pins['legacy_helper_head'],'passed':result.returncode==0 and sorted(passed)==sorted(expected)and not failed and unchanged,'network':'none','command':command,'exit_code':result.returncode,'seconds':round(time.monotonic()-started,3),'expected_count':len(expected),'actual_passed_count':len(passed),'actual_failed_count':len(failed),'current_protocol_count':sum(counts[:2]),'legacy_helper_count':counts[2],'current_executor_count':len(new_names),'new_test_names':new_names,'passed_test_names':passed,'inputs_unchanged':unchanged,'input_sha256':inputs,'log_sha256':digest(out/'test.log'),'limits':['Pinned current protocol tests and retained historical helper tests are separate from current executor runtime tests','Current executor cases execute LiquidationExecutor and genuine CreditPool/V3 in local in-process EVM','No actual node/fork proof in this run; those have separate evidence']}
(out/'acceptance.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n');print(json.dumps({k:report[k]for k in ['passed','exit_code','seconds','actual_passed_count','actual_failed_count','current_executor_count','inputs_unchanged']}))
if not report['passed']:raise SystemExit(1)
PY
python3 "$task_repo/scripts/verify-solidity-fixtures.py"
