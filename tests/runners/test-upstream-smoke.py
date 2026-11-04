#!/usr/bin/env python3
"""Actual current CLI smoke on a fresh Anvil or pinned Monad TESTNET fork.

All signing/mutations are strictly HTTP IP-loopback chain31337. Remote fork
reads require explicit opt-in and a full pinned header/code/storage proof.
No keys or signed bytes are copied into public acceptance evidence.
"""
import argparse
import importlib.util
import json
import os
import pathlib
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request

FORK_ANCHOR_PATH = pathlib.Path(__file__).resolve().parents[2] / 'tests/fixtures/upstream-fork-anchor.json'
FORK_ANCHOR = json.loads(FORK_ANCHOR_PATH.read_text())
FORK_BLOCK = FORK_ANCHOR['block_number']
FORK_HASH = FORK_ANCHOR['block_hash']
FORK_TIME = FORK_ANCHOR['block_timestamp']
GENESIS = FORK_ANCHOR['genesis_hash']
PRIMARY = FORK_ANCHOR['rpc_url']
OLD_POOL = FORK_ANCHOR['pool_address']
OLD_ORACLE = FORK_ANCHOR['oracle_address']
OLD_SLOT_ZERO = '0x' + OLD_ORACLE[2:].rjust(64, '0')

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        raise RuntimeError('RPC redirects forbidden')

OPENER=urllib.request.build_opener(urllib.request.ProxyHandler({}),NoRedirect())
REMOTE_READ={'eth_chainId','eth_getBlockByNumber','eth_getCode','eth_getStorageAt'}
LOCAL_READ=REMOTE_READ|{'anvil_nodeInfo','eth_getTransactionReceipt','eth_getBalance','eth_call'}
LOCAL_ADMIN={'anvil_setBalance','anvil_mine','evm_setNextBlockTimestamp','evm_mine'}

def rpc(url, method, params):
    parsed=urllib.parse.urlsplit(url)
    local=parsed.scheme=='http'and parsed.hostname in ['127.0.0.1','::1']and not parsed.username and not parsed.password and not parsed.query and not parsed.fragment
    require((local and method in LOCAL_READ|LOCAL_ADMIN)or(url==PRIMARY and method in REMOTE_READ),'RPC method/endpoint is outside isolated policy')
    def request(call, arguments):
        req=urllib.request.Request(url,json.dumps({'jsonrpc':'2.0','id':1,'method':call,'params':arguments}).encode(),{'Content-Type':'application/json'})
        value=json.load(OPENER.open(req,timeout=45))
        if 'error'in value:raise RuntimeError(f'{call}: {value["error"]}')
        if 'result'not in value:raise RuntimeError('RPC response omitted result')
        return value['result']
    if method in LOCAL_ADMIN:
        require(int(request('eth_chainId',[]),16)==31337,'local admin chain must be31337')
    return request(method,params)

def require(condition, reason):
    if not condition:
        raise RuntimeError(reason)

def load_module(repo):
    spec = importlib.util.spec_from_file_location('accepted_testnet_runner', repo/'tests/runners/run-testnet.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

def fork_proof(url, local, cast):
    chain = int(rpc(url,'eth_chainId',[]),16)
    require(chain == (31337 if local else 10143), 'wrong fork identity')
    genesis = rpc(url,'eth_getBlockByNumber',['0x0',False])
    block = rpc(url,'eth_getBlockByNumber',[hex(FORK_BLOCK),False])
    require(genesis and genesis['hash'].lower()==GENESIS, 'wrong remote genesis')
    require(block and block['hash'].lower()==FORK_HASH and int(block['timestamp'],16)==FORK_TIME, 'fixed fork header changed')
    # Anvil1.8.4 exposes forked storage at its current local state after mining;
    # the remote counterpart remains read at the exact immutable fork anchor.
    state_tag='latest'if local else hex(FORK_BLOCK)
    slot = rpc(url,'eth_getStorageAt',[OLD_POOL,'0x0',state_tag])
    code = rpc(url,'eth_getCode',[OLD_POOL,state_tag])
    require(slot.lower()==OLD_SLOT_ZERO and len(code)>100, 'genuine historical pool storage/code absent')
    code_hash = subprocess.check_output([str(cast),'keccak',code],text=True).strip()
    return {'chain_id':chain,'genesis':genesis['hash'],'block':FORK_BLOCK,'block_hash':block['hash'],'timestamp':int(block['timestamp'],16),'old_pool':OLD_POOL,'slot0_oracle':slot,'storage_request_block_tag':state_tag,'runtime_keccak':code_hash,'code_bytes':(len(code)-2)//2}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo',type=pathlib.Path,default=pathlib.Path(__file__).resolve().parents[2])
    parser.add_argument('--mode',choices=['plain','fork'],required=True)
    parser.add_argument('--artifacts',type=pathlib.Path,required=True,help='Offline bytecode bundle from build-testnet-artifacts.py')
    parser.add_argument('--output',type=pathlib.Path,required=True)
    parser.add_argument('--execute-local',action='store_true')
    parser.add_argument('--allow-testnet-read',action='store_true')
    parser.add_argument('--anvil',type=pathlib.Path,required=True)
    parser.add_argument('--cast',type=pathlib.Path,required=True)
    parser.add_argument('--ctl',type=pathlib.Path,required=True)
    parser.add_argument('--bot',type=pathlib.Path,required=True)
    parser.add_argument('--port',type=int,default=18557)
    args = parser.parse_args()
    require(args.execute_local, 'explicit --execute-local is required')
    require(args.mode!='fork' or args.allow_testnet_read, 'fork requires explicit read-only testnet opt-in')
    repo=args.repo.resolve(); output=args.output.resolve()
    require(not output.is_relative_to(repo) and not any((p/'.git').exists() for p in [output,*output.parents]), 'all smoke runtime must remain outside repositories')
    output.mkdir(parents=True,exist_ok=False)
    url=f'http://127.0.0.1:{args.port}'
    parsed=urllib.parse.urlsplit(url)
    require(parsed.scheme=='http' and parsed.hostname in ['127.0.0.1','::1'] and not parsed.username and not parsed.password and not parsed.query and not parsed.fragment, 'mutations require credential-free IP loopback')
    module=load_module(repo)
    proof=None; node=None; started=time.monotonic(); checks=[]
    artifacts=args.artifacts.resolve()
    inputs={str(p):module.sha(p.read_bytes())for p in [repo/'tests/runners/run-testnet.py',repo/'examples/local-manifest.json',repo/'tests/fixtures/testnet/deployment-template.json',artifacts,FORK_ANCHOR_PATH,args.bot,args.ctl,pathlib.Path(__file__).resolve()]}
    if args.mode=='fork':
        proof=fork_proof(PRIMARY,False,args.cast)
        expected=FORK_ANCHOR['pool_runtime_keccak']
        require(proof['runtime_keccak'].lower()==expected.lower(), 'historical pool runtime disagrees with accepted deployment')
        module.atomic_json(output/'remote-fork-proof.json',proof)
    try:
        with socket.socket()as probe:
            require(probe.connect_ex(('127.0.0.1',args.port))!=0,'local port already occupied')
        cmd=[str(args.anvil),'--host','127.0.0.1','--port',str(args.port),'--chain-id','31337','--timestamp',str(FORK_TIME),'--block-time','0.3','--slots-in-an-epoch','1','--network','monad','--quiet']
        if args.mode=='fork':cmd+=['--fork-url',PRIMARY,'--fork-block-number',str(FORK_BLOCK)]
        log=(output/'anvil.log').open('x'); node=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT)
        module.atomic_json(output/'node-command.json',{'argv':cmd,'pid':node.pid,'public_transaction_execution':False})
        for _ in range(100):
            if node.poll()is not None:raise RuntimeError('owned Anvil failed to start')
            try:
                if int(rpc(url,'eth_chainId',[]),16)==31337:break
            except (OSError,ValueError,RuntimeError):pass
            time.sleep(.2)
        else:raise RuntimeError('owned Anvil readiness timed out')
        require(int(rpc(url,'eth_chainId',[]),16)==31337,'local signing chain is not31337')
        node_info=rpc(url,'anvil_nodeInfo',[])
        require(node_info.get('network')=='monad'and node_info.get('hardFork')=='MonadTen','isolated smoke must preserve Monad code-size/billing semantics')
        module.atomic_json(output/'local-node-info.json',node_info)
        genesis=rpc(url,'eth_getBlockByNumber',['0x0',False])['hash']
        if args.mode=='fork':
            local=fork_proof(url,True,args.cast)
            require(local['runtime_keccak']==proof['runtime_keccak'],'local fork differs from remote code')
            module.atomic_json(output/'local-fork-proof.json',local)
            checks.append('remote and local fixed full header/genesis/code/storage match')
        secret=output/'private';secret.mkdir();os.chmod(secret,0o700)
        fresh=json.loads(subprocess.check_output([str(args.ctl),'--local','--rpc',url,'--chain-id','31337','--journal',str(secret/'key-generation-only.json'),'generate-key-file','--output',str(secret/'fresh-owner-key.txt')],text=True))['address']
        rpc(url,'anvil_setBalance',[fresh,hex(1000*module.WAD)])
        common=['--execute','--local-rehearsal','--rpc',url,'--genesis',genesis,'--owner',fresh,'--owner-key-file',str(secret/'fresh-owner-key.txt'),'--artifacts',str(artifacts),'--template',str(repo/'tests/fixtures/testnet/deployment-template.json'),'--evidence-dir',str(output/'deployment'),'--secret-dir',str(secret),'--ctl',str(args.ctl),'--cast',str(args.cast),'--bot',str(args.bot),'--bot-cycles','10','--bot-timeout-seconds','240']
        module.atomic_json(output/'common-args.json',common)
        old_argv=sys.argv;sys.argv=['accepted-local-orchestrator',*common]
        try:runner_args=module.parse_args()
        finally:sys.argv=old_argv
        require(runner_args.local_rehearsal and runner_args.rpc==url and runner_args.genesis==genesis,'runner local identity changed')
        class LocalRunner(module.Runner):
            def tool(self,argv,timeout=240):
                # Real local mining only during controller sends, never while the bot
                # runs. Receipt and finalized-tag validation remain in testnet_ctl.
                mine=(str(argv[0])==str(self.args.ctl)and any(str(x)in ['send','deploy','resume']for x in argv[1:]))
                stopped=threading.Event();errors=[]
                def pump():
                    while not stopped.wait(.2):
                        try:rpc(url,'anvil_mine',['0x40','0x0'])
                        except Exception as error:errors.append(str(error));return
                thread=threading.Thread(target=pump,daemon=True)if mine else None
                if thread:thread.start()
                try:return super().tool(argv,timeout)
                finally:
                    stopped.set()
                    if thread:thread.join(timeout=50)
                    if errors:raise RuntimeError('local finality mining failed: '+errors[0])
        runner=LocalRunner(runner_args)
        runner.deploy()
        for actor in runner.state['actors'].values():
            actor_path=pathlib.Path(actor['key_file'])
            require(actor_path.resolve().is_relative_to(secret)and not actor_path.is_symlink(),'new actor key path escaped fresh workspace')
        manifest=json.loads((runner.directory/'bot-manifest.json').read_text())
        require(manifest['additional_profit_asset']=='0x0','ordinary smoke must use zero additional profit')
        pool=runner.state['contracts']['credit_pool']; floor=manifest['pools']['test-stack']['spec']
        require(floor['deployment_block']==pool['block_number']and floor['deployment_block_hash']==pool['block_hash'],'SDK floor is not actual deployment receipt')
        require(args.mode!='fork'or pool['block_number']>FORK_BLOCK,'fresh pool must be genuinely created after fork anchor')
        require(rpc(url,'eth_getBlockByNumber',[hex(pool['block_number']),False])['hash']==pool['block_hash'],'deployment receipt block not canonical')
        require(runner.address(runner.context['receipt'],'POOL()')==runner.context['underlying'],'receipt vault binding failed')
        checks+=['18 fresh real contract deployments with finalized receipts and code identities','new CreditPool SDK floor matches actual creation receipt','actual Aave receipt binding and vault adapter setup','baseline additional profit zero']
        # Exercise the actual public discovery/check commands against this deployment.
        discovered=runner.directory/'discovered.toml'
        result=subprocess.run([str(args.bot),'--rpc-url',url,'config','init','--network','local',
            '--executor',runner.context['executor'],'--operator',runner.context['operator'],
            '--pool',runner.context['credit_pool'],'--quoter',runner.context['quoter'],'--output',str(discovered)],
            cwd=repo,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=120)
        require(result.returncode==0,'public config discovery failed')
        result=subprocess.run([str(args.bot),'--config',str(discovered),'config','check'],cwd=repo,
            stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=15)
        require(result.returncode==0,'generated external TOML failed offline config check')
        checks.append('public config init discovers deployed identities and generated TOML passes config check')
        runner.positions()
        checks+=['actual Provided matching creates debt100/collateral400','healthy current CLI no-send preserves nonce']
        stdout=runner.directory/'eligible-dry-run.stdout.jsonl';stderr=runner.directory/'eligible-dry-run.stderr.jsonl'
        runner.ensure_oracle('smoke-dryrun-eligible',4*module.WAD//10,module.WAD)
        before=runner.ctl('inspect',actor='operator')
        with stdout.open('x')as out,stderr.open('x')as err:
            result=subprocess.run([str(args.bot),'--manifest',str(runner.directory/'bot-manifest.json'),'--cycles','10'],stdout=out,stderr=err,timeout=240,cwd=repo)
        require(result.returncode==0,'actual unsigned CLI dry-run failed')
        after=runner.ctl('inspect',actor='operator')
        require(before['latest_nonce']==after['latest_nonce']and before['balance_wei']==after['balance_wei'],'dry-run spent gas or nonce')
        module.atomic_json(runner.directory/'DRY_RUN_RESULT.json',{'passed':True,'operator_before':before,'operator_after':after,'stdout_sha256':module.sha(stdout.read_bytes())})
        events=runner.bot_events(stdout)
        require(any(e.get('event')=='dry_run_validated'for e in events),'eligible dry-run did not produce a real prepared plan')
        require(any(e.get('event')=='stopped'and e.get('dry_run_plans',0)>0 and 'active_pending' in e and e['active_pending'] is None for e in events),'dry-run did not stop without signing')
        checks.append('eligible unsigned CLI prepares route without signing, gas spend or nonce change')
        runner.guards()
        require(len(runner.state['guard_cases'])==5 and all(v['passed']for v in runner.state['guard_cases'].values()),'five actual no-send guards incomplete')
        checks+=['actual gas fee cap no-send','actual insufficient native reserve no-send','actual explicit high profit floor no-send','actual stale oracle no-send','actual future oracle no-send']
        native_before=runner.ctl('inspect',actor='operator')
        asset_before=runner.scalar(runner.context['asset'],'balanceOf(address)',[runner.context['executor']])
        module.atomic_json(runner.directory/'LIVE_BASELINE.json',{'operator':native_before,'executor_asset':asset_before,'source':'actual local getters before first signed CLI run'})
        live = runner.live_until('eligible')
        stdout_path=pathlib.Path(live['run']['stdout'])
        events=runner.bot_events(stdout_path)
        original_hash=next(e['outcome']['attempt']['hash'] for e in events if e.get('event')=='finalized')
        checks.append('actual CLI finalizes in RAM, HTTP reports idle, SIGTERM stops cleanly')
        runner.outcome()
        public=json.loads((runner.directory/'LIQUIDATION_OUTCOME.json').read_text())
        require(len(public['outcomes'])==1 and not public['prior_paid_outcomes'],'smoke archive is not exactly one isolated success')
        win=public['outcomes'][0];receipt=win['receipt'];bound=int(win['native_charge'],0)
        require(win['hash']==original_hash,'terminal receipt differs from observed finalized hash')
        receipt_execution_fee=receipt['gas_used']*int(receipt['effective_gas_price'],0)
        native_after=runner.ctl('inspect',actor='operator')
        require(native_after['latest_nonce']==native_before['latest_nonce']+1 and native_after['latest_nonce']==native_after['pending_nonce'],'one finalized nonce not reconciled')
        actual_charge=int(native_before['balance_wei'],0)-int(native_after['balance_wei'],0)
        require(actual_charge==bound and receipt_execution_fee<=actual_charge,'actual Monad native debit differs from signed gasLimit charge')
        require(0<actual_charge<=bound,'actual native charge exceeds signed conservative bound')
        gain=int(win['asset_gain'],0);asset_after=runner.scalar(runner.context['asset'],'balanceOf(address)',[runner.context['executor']])
        require(asset_after-asset_before==gain and gain>=int(win['signed_min_profit'],0),'actual debt-asset profit delta violates signed guard')
        allowances={'asset_pool':runner.scalar(runner.context['asset'],'allowance(address,address)',[runner.context['executor'],runner.context['credit_pool']]),'collateral_router':runner.scalar(runner.context['collateral'],'allowance(address,address)',[runner.context['executor'],runner.context['router']])}
        require(not any(allowances.values()),'executor left repay/router allowances')
        checks+=['current CLI signs and canonical-finalizes exactly one real liquidation','debt zero and executor asset-profit custody verified','asset-profit delta covers signed native cost guard','operator actual Monad native debit equals signed gasLimit*gasPrice; receipt gasUsed does not exceed limit','latest/pending nonce reconciled plus zero repay/router allowances']
        repeat=runner.bot_run('closed-debt-no-repeat',cycles=10)
        repeat_nonce=runner.operator_nonce()
        require(repeat['finalized_liquidations']==0 and repeat['active_pending']is False and repeat_nonce==native_after['latest_nonce'],'closed debt was signed again')
        checks.append('same real CLI restarted after closed debt does not repeat liquidation')
        latest=int(rpc(url,'eth_getBlockByNumber',['latest',False])['timestamp'],16)
        market=runner.state['scenario_market']
        if latest<market:
            rpc(url,'evm_setNextBlockTimestamp',[market]);rpc(url,'evm_mine',[])
        runner.cleanup()
        require(runner.state.get('cleanup_passed')is True and not any(runner.state['cleanup_actual'].values()),'actual isolated custody cleanup incomplete')
        checks.append('actual market maturity permits lender claim; profit/collateral/vault/LP/wrapped custody cleanup closes eight zero checks')
        after_hashes={name:module.sha(pathlib.Path(name).read_bytes())for name in inputs}
        require(after_hashes==inputs,'accepted inputs changed during smoke')
        report={'passed':True,'mode':args.mode,'seconds':round(time.monotonic()-started,3),'winning_hash':win['hash'],'original_mined_hash':original_hash,'checks':checks,'check_count':len(checks),'origin_chain_id':10143 if args.mode=='fork'else None,'local_chain_id':31337,'fork':proof,'actual_deployment_floor':floor,'actual_operator_native_charge_wei':str(actual_charge),'conservative_session_native_charge_wei':str(bound),'profit_asset_wei':str(gain),'allowances':allowances,'cleanup':runner.state['cleanup_actual'],'source_sha256':inputs,'inputs_unchanged':True,'raw_transactions_copied':False,'public_transactions_sent':False,'limitations':['Fresh isolated deployments on actual fork; historical deployment code/storage is proven against the pinned remote block, not used for bot liquidation','Standard synthetic local tokens/oracle/Aave underlying; canonical real V3 deployment bytecode','Anvil MonadTen billing verified from actual native balance debit; Ethereum gasUsed billing is not assumed','No production market/token/oracle or public mainnet deployment acceptance']}
        module.atomic_json(output/'acceptance.json',report)
        print(json.dumps({'passed':True,'mode':args.mode,'check_count':len(checks),'actual_deployment_floor':pool['block_number'],'actual_profit_asset_wei':str(gain)}),flush=True)
    finally:
        if node is not None:
            node.send_signal(__import__('signal').SIGINT)
            try:node.wait(timeout=10)
            except subprocess.TimeoutExpired:node.kill();node.wait()
            log.close()

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps({'passed':False,'reason':str(error),'private_keys_and_signed_bytes':'never printed'}),file=sys.stderr)
        sys.exit(1)
