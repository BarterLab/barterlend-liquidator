#!/usr/bin/env python3
"""Real Kyber API/router/tokens on a fresh local Monad fork; upstream is read-only.
The lending callback fixture controls repayment. Genuine lending is tested separately.
Supply addresses/RPC in an external JSON file. No public private key is accepted.
"""
import argparse, collections, http.server, json, os, pathlib, socket, subprocess, tempfile, threading, time, urllib.request, urllib.parse

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config',required=True);parser.add_argument('--output',required=True)
    parser.add_argument('--anvil',required=True);parser.add_argument('--cast',required=True)
    args=parser.parse_args();repo=pathlib.Path(__file__).resolve().parents[2]
    config=json.loads(pathlib.Path(args.config).read_text());output=pathlib.Path(args.output).resolve();output.mkdir(parents=True,exist_ok=False)
    opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def request(url,value=None,headers=None):
        req=urllib.request.Request(url,data=None if value is None else json.dumps(value).encode(),headers={'Content-Type':'application/json','User-Agent':'barterlend-local-fork',**(headers or {})})
        open_url=opener.open if url.startswith('http://127.0.0.1:') else urllib.request.urlopen
        try:
            with open_url(req,timeout=180 if url.startswith("http://127.0.0.1:") else 30) as response:return json.load(response)
        except urllib.error.HTTPError as error:
            if 'kyberswap.com' in url:
                detail=error.read(4096).decode(errors='replace')
                (output/'api-error.json').write_text(detail)
            raise
    def rpc(url,method,params):
        value=request(url,{'jsonrpc':'2.0','id':1,'method':method,'params':params})
        if 'error' in value:raise RuntimeError(value['error'])
        return value['result']
    allowed={'eth_chainId','eth_blockNumber','eth_getBlockByNumber','eth_getBlockByHash','eth_getCode','eth_getStorageAt','eth_getBalance','eth_getTransactionCount','eth_getTransactionReceipt','eth_getTransactionByHash','eth_getProof','eth_gasPrice'}
    lock=threading.Lock();counts=collections.Counter();last=[0.0]
    class Proxy(http.server.BaseHTTPRequestHandler):
        def log_message(self,*unused):pass
        def do_POST(self):
            length=int(self.headers.get('Content-Length','0'))
            if not 0<length<=1048576:self.send_error(413);return
            def one(item):
                method=item.get('method');reply={'jsonrpc':'2.0','id':item.get('id')}
                if method not in allowed:
                    counts['rejected:'+str(method)]+=1
                    reply['error']={'code':-32601,'message':'read-only upstream'};return reply
                try:
                    with lock:
                        time.sleep(max(0,.2-(time.monotonic()-last[0])));last[0]=time.monotonic();counts[method]+=1
                        reply['result']=rpc(config['rpc_url'],method,item.get('params',[]))
                except Exception:reply['error']={'code':-32000,'message':'upstream unavailable'}
                return reply
            value=json.loads(self.rfile.read(length));body=json.dumps([one(v) for v in value] if isinstance(value,list) else one(value)).encode()
            try:self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
            except (BrokenPipeError,ConnectionResetError):pass
    proxy=http.server.ThreadingHTTPServer(('127.0.0.1',0),Proxy);thread=threading.Thread(target=proxy.serve_forever,daemon=True);thread.start();node=None
    try:
        upstream=f'http://127.0.0.1:{proxy.server_port}'
        assert int(rpc(upstream,'eth_chainId',[]),16)==143
        anchor=rpc(upstream,'eth_getBlockByNumber',['latest',False])
        with socket.socket() as listener:listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
        local=f'http://127.0.0.1:{port}'
        with (output/'anvil.log').open('w') as log:
            node=subprocess.Popen([args.anvil,'--host','127.0.0.1','--port',str(port),'--network','monad','--chain-id','31337','--fork-url',upstream,'--fork-block-number',str(int(anchor['number'],16)),'--no-storage-caching','--no-fork-node-info','--no-bal','--silent'],stdout=log,stderr=subprocess.STDOUT)
        for _ in range(200):
            if node.poll() is not None:raise RuntimeError('Anvil exited')
            try:
                if int(rpc(local,'eth_chainId',[]),16)==31337:break
            except (OSError,RuntimeError):time.sleep(.1)
        else:raise RuntimeError('Anvil startup timeout')
        actor=rpc(local,'eth_accounts',[])[0]
        def cast(*values):return subprocess.check_output([args.cast,*map(str,values)],text=True).strip()
        def send(to,data,value=0):
            # The only send destination is this exclusive loopback chain31337.
            assert int(rpc(local,'eth_chainId',[]),16)==31337
            tx={'from':actor,'data':data,'gas':hex(20_000_000),'gasPrice':rpc(local,'eth_gasPrice',[]),'value':hex(value)}
            if to:tx['to']=to
            hash_=rpc(local,'eth_sendTransaction',[tx]);receipt=None;deadline=time.monotonic()+30
            while receipt is None and time.monotonic()<deadline:
                receipt=rpc(local,'eth_getTransactionReceipt',[hash_])
                if receipt is None:time.sleep(.1)
            if receipt is None or receipt['status']!='0x1':
                (output/'failed-receipt.json').write_text(json.dumps(receipt))
                if receipt:
                    trace=rpc(local,'debug_traceTransaction',[hash_,{'tracer':'callTracer'}])
                    (output/'failed-trace.json').write_text(json.dumps(trace))
                    def errors(item):
                        if item.get('error'):print({k:item.get(k) for k in ['type','to','error','revertReason','output']},flush=True)
                        for child in item.get('calls',[]):errors(child)
                    errors(trace)
                rpc(local,'eth_call',[tx,receipt['blockNumber'] if receipt else 'latest'])
                raise RuntimeError('local transaction reverted or remained pending: '+hash_)
            return receipt
        def deploy(source,name,signature,values):
            artifact=json.loads((repo/'solidity-out'/source/(name+'.json')).read_text())
            receipt=send(None,artifact['bytecode']['object']+cast('abi-encode',signature,*values)[2:]);return receipt['contractAddress']
        def call(to,signature,*values):return rpc(local,'eth_call',[{'to':to,'data':cast('calldata',signature,*values)},'latest'])
        def transact(to,signature,*values,value=0):return send(to,cast('calldata',signature,*values),value)
        collateral=config['input'];asset=config['output'];router=config['kyber_router'];amount=int(config['amount'])
        lending_factory=deploy('TestLendingFactory.sol','TestLendingFactory','f(address)',[actor])
        executor=deploy('LiquidationExecutor.sol','LiquidationExecutor','f(address,address,address,address,address[])',[actor,actor,config['v3_router'],config['v3_factory'],'['+lending_factory+']'])
        pool=deploy('ExecutorSecurity.t.sol','AdversarialCreditPool','f(address,address)',[asset,collateral])
        for token in [asset,collateral]:transact(executor,'setVerifiedToken(address,bool)',token,'true')
        transact(lending_factory,'registerPool(address)',pool)
        pin=cast('keccak',rpc(local,'eth_getCode',[router,'latest']));transact(executor,'setKyberRouter(address,bytes32)',router,pin)
        # Test uses genuine WMON deposit, not a fabricated token balance or fake DEX pool.
        transact(collateral,'deposit()',value=amount+10**18)
        transact(collateral,'transfer(address,uint256)',pool,amount)
        transact(collateral,'transfer(address,uint256)',executor,10**18)
        api=config.get('api_url','https://aggregator-api.kyberswap.com').rstrip('/')+'/monad/api/v1'
        headers={'x-client-id':'barterlend-liquidator'}
        route=request(api+'/routes?'+urllib.parse.urlencode({'tokenIn':collateral,'tokenOut':asset,'amountIn':str(amount),'gasInclude':'false','excludeRFQSources':'true'}),headers=headers)
        assert route['code']==0 and route['data']['routerAddress'].lower()==router.lower()
        (output/'api-route.json').write_text(json.dumps(route))
        time.sleep(.34)
        build=request(api+'/route/build',{'routeSummary':route['data']['routeSummary'],'sender':executor,'recipient':executor,'slippageTolerance':100,'enableGasEstimation':False},headers)
        assert build['code']==0;build=build['data'];assert build['routerAddress'].lower()==router.lower()
        captured={'input':collateral,'output':asset,'router':router,'executor':executor,'amount':str(amount),'build':build}
        capture=output/'api-build.json';capture.write_text(json.dumps(captured))
        env=dict(os.environ,BARTERLEND_KYBER_BUILD=str(capture))
        subprocess.run(['cargo','test','--offline','--locked','--test','kyber_api_build','--','--ignored','--exact','actual_api_build_matches_executor_constraints'],cwd=repo,env=env,check=True,timeout=180)
        quoted=int(build['amountOut']);minimum=quoted*99//100;repay=minimum//2
        transact(pool,'configure(uint8,uint128,uint128)',0,repay,amount)
        path=cast('abi-encode','f(bytes4,address,bytes)','0x4b594252',router,build['data'])
        timestamp=int(rpc(local,'eth_getBlockByNumber',['latest',False])['timestamp'],16)
        arguments=f'({pool},1,{actor},{repay},0,{timestamp+900},{path},1,{minimum},0x'+('11'*32)+')'
        data=cast('calldata','execute((address,uint64,address,uint128,uint256,uint256,bytes,uint256,uint256,bytes32))',arguments)
        simulated=rpc(local,'eth_call',[{'from':actor,'to':executor,'data':data,'gas':hex(20_000_000),'gasPrice':rpc(local,'eth_gasPrice',[])},'latest'])
        receipt=send(executor,data)
        gain=int(call(asset,'balanceOf(address)',executor),16)
        assert gain>=minimum-repay and int(call(asset,'balanceOf(address)',pool),16)==repay
        assert int(call(collateral,'balanceOf(address)',executor),16)==10**18
        assert int(call(collateral,'allowance(address,address)',executor,router),16)==0
        assert int(call(asset,'allowance(address,address)',executor,pool),16)==0
        report={'passed':True,'anchor':{k:anchor[k] for k in ['number','hash','timestamp']},'router':router,'router_code_hash':pin,'quoted_output':quoted,'minimum_output':minimum,'repay':repay,'asset_gain':gain,'receipt':receipt,'upstream_read_only':True,'rpc_counts':dict(counts),'scope':'real Kyber API/router/WMON/USDC/DEX; synthetic lending callback; local chain31337 only'}
        (output/'result.json').write_text(json.dumps(report,indent=2)+'\n');print('PASS real Kyber swap inside atomic liquidation on local Monad fork',flush=True)
    finally:
        if node is not None:
            node.terminate()
            try:node.wait(timeout=5)
            except subprocess.TimeoutExpired:node.kill();node.wait()
        proxy.shutdown();proxy.server_close();thread.join(timeout=5)
if __name__=='__main__':main()
