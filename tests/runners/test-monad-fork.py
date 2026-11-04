#!/usr/bin/env python3
"""Pinned Monad state, read-only upstream, local Anvil/Forge/Rust execution only.
No private keys or remote send methods are accepted. Tool paths are explicit.
"""
import argparse, collections, http.server, json, os, pathlib, socket, subprocess, tempfile, threading, time, urllib.request
BLOCK=117583907
HASH="0x68b17d3d27c3310b75cae9e719169580766419bad4e29f3a7c9f57b4d4968eb0"
ALLOWED={"eth_chainId","eth_getBlockByNumber","eth_getBlockByHash","eth_getCode","eth_getStorageAt","eth_getBalance","eth_getTransactionCount","eth_getProof","eth_gasPrice"}
def port():
    with socket.socket() as s:s.bind(("127.0.0.1",0));return s.getsockname()[1]
def rpc(url,method,params):
    body=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode()
    request=urllib.request.Request(url,data=body,headers={"Content-Type":"application/json","User-Agent":"BarterLend-readonly-route-review/1.0"})
    with urllib.request.urlopen(request,timeout=30) as response:value=json.load(response)
    if "error" in value:raise RuntimeError(str(value["error"]))
    return value["result"]
def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-url",default="https://rpc1.monad.xyz")
    parser.add_argument("--forge",required=True);parser.add_argument("--anvil",required=True);parser.add_argument("--solc",required=True)
    parser.add_argument("--output",required=True)
    parser.add_argument("--prepare-only",action="store_true",help="Keep the isolated node running for manual local checks; stop with Ctrl-C")
    args=parser.parse_args();repo=pathlib.Path(__file__).resolve().parents[2]
    output=pathlib.Path(args.output).resolve();output.mkdir(parents=True,exist_ok=False)
    counts=collections.Counter();lock=threading.Lock();last=[0.0]
    class Proxy(http.server.BaseHTTPRequestHandler):
        def log_message(self,*unused):pass
        def do_POST(self):
            length=int(self.headers.get("Content-Length","0"))
            if not 0<length<=1048576:self.send_error(413);return
            def one(item):
                method=item.get("method");response={"jsonrpc":"2.0","id":item.get("id")}
                if method not in ALLOWED:
                    counts["rejected:"+str(method)]+=1
                    response["error"]={"code":-32601,"message":"read-only fork gateway: method not allowed"};return response
                try:
                    with lock:
                        time.sleep(max(0,0.2-(time.monotonic()-last[0])));last[0]=time.monotonic()
                        counts[method]+=1
                        response["result"]=rpc(args.rpc_url,method,item.get("params",[]))
                except Exception as error:response["error"]={"code":-32000,"message":str(error)}
                return response
            body=json.loads(self.rfile.read(length))
            payload=json.dumps([one(item) for item in body] if isinstance(body,list) else one(body)).encode()
            try:
                self.send_response(200);self.send_header("Content-Type","application/json");self.send_header("Content-Length",str(len(payload)));self.end_headers();self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):pass
    proxy=http.server.ThreadingHTTPServer(("127.0.0.1",0),Proxy)
    thread=threading.Thread(target=proxy.serve_forever,daemon=True);thread.start()
    upstream="http://127.0.0.1:"+str(proxy.server_port)
    node=None
    try:
        assert int(rpc(upstream,"eth_chainId",[]),16)==143
        header=rpc(upstream,"eth_getBlockByNumber",[hex(BLOCK),False]);assert header["hash"]==HASH
        with tempfile.TemporaryDirectory(prefix="barterlend-monad-fork-") as work:
            local="http://127.0.0.1:"+str(port())
            with (output/"anvil.log").open("w") as log:
                node=subprocess.Popen([args.anvil,"--host","127.0.0.1","--port",local.rsplit(":",1)[1],"--network","monad","--chain-id","31337","--fork-url",upstream,"--fork-block-number",str(BLOCK),"--no-storage-caching","--no-bal","--no-fork-node-info","--timeout","30000","--retries","2","--silent"],stdout=log,stderr=subprocess.STDOUT)
                for _ in range(200):
                    if node.poll() is not None:raise RuntimeError("Anvil exited; see anvil.log")
                    try:
                        if int(rpc(local,"eth_chainId",[]),16)==31337:break
                    except (OSError,RuntimeError):time.sleep(.1)
                else:raise RuntimeError("Anvil startup timeout")
                assert rpc(local,"eth_getBlockByNumber",[hex(BLOCK),False])["hash"]==HASH
                metadata={"block":BLOCK,"hash":HASH,"local_url":local,"upstream_read_only":True,"max_upstream_rps":5}
                (output/"fork.json").write_text(json.dumps(metadata,indent=2)+"\n")
                print(json.dumps(metadata),flush=True)
                if args.prepare_only:
                    while True:time.sleep(.5)
                env=dict(os.environ,BARTERLEND_MONAD_FORK_URL=local,BARTERLEND_MONAD_FORK_BLOCK=str(BLOCK))
                env.setdefault("CARGO_TARGET_DIR",str(pathlib.Path(work)/"target"))
                env.setdefault("CARGO_BUILD_JOBS","4");env.setdefault("CARGO_PROFILE_TEST_DEBUG","0");env.setdefault("CARGO_INCREMENTAL","0")
                forge=[args.forge,"test","--root",str(repo),"--use",args.solc,"--offline","--network","monad","--fork-url",local,"--fork-block-number",str(BLOCK),"--no-storage-caching","--out",str(pathlib.Path(work)/"out"),"--cache-path",str(pathlib.Path(work)/"cache"),"--match-contract","ExecutorMonadFork","-vv"]
                with (output/"forge.log").open("w") as log:subprocess.run(forge,env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
                with (output/"rust.log").open("w") as log:subprocess.run(["cargo","+"+os.environ.get("BARTERLEND_RUST_TOOLCHAIN","1.98.0"),"test","--offline","--locked","--test","routes_monad_fork","--","--ignored","--nocapture"],cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
                metadata["status"]="passed";(output/"fork.json").write_text(json.dumps(metadata,indent=2)+"\n")
    finally:
        if node is not None:
            node.terminate()
            try:node.wait(timeout=10)
            except subprocess.TimeoutExpired:node.kill();node.wait()
        proxy.shutdown();proxy.server_close();thread.join(timeout=5)
        (output/"upstream-methods.json").write_text(json.dumps(dict(counts),indent=2)+"\n")
if __name__=="__main__":main()
