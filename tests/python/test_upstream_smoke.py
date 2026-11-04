#!/usr/bin/env python3
"""Offline fail-closed checks for the isolated upstream smoke runner."""
import importlib.util
import io
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch

REPO=pathlib.Path(__file__).resolve().parents[2]
SPEC=importlib.util.spec_from_file_location('upstream_smoke',REPO/'tests/runners/test-upstream-smoke.py')
SMOKE=importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)

class FakeOpener:
    def __init__(self,results):self.results=list(results);self.requests=[]
    def open(self,request,timeout):
        self.requests.append((request.full_url,json.loads(request.data)))
        return io.BytesIO(json.dumps({'jsonrpc':'2.0','id':1,'result':self.results.pop(0)}).encode())

class IsolatedRpcPolicy(unittest.TestCase):
    def rejected_without_request(self,url,method):
        fake=FakeOpener([])
        with patch.object(SMOKE,'OPENER',fake),self.assertRaises(RuntimeError):SMOKE.rpc(url,method,[])
        self.assertEqual(fake.requests,[])

    def test_primary_admin_call_is_rejected_before_http(self):self.rejected_without_request(SMOKE.PRIMARY,'anvil_setBalance')
    def test_primary_signed_broadcast_is_rejected_before_http(self):self.rejected_without_request(SMOKE.PRIMARY,'eth_sendRawTransaction')
    def test_unapproved_remote_endpoint_is_rejected(self):self.rejected_without_request('https://example.invalid','eth_chainId')
    def test_loopback_credentials_are_rejected(self):self.rejected_without_request('http://user:secret@127.0.0.1:18557','eth_chainId')
    def test_loopback_unknown_method_is_rejected(self):self.rejected_without_request('http://127.0.0.1:18557','eth_sendTransaction')
    def test_local_admin_wrong_chain_stops_before_mutation(self):
        fake=FakeOpener(['0x279f'])
        with patch.object(SMOKE,'OPENER',fake),self.assertRaises(RuntimeError):SMOKE.rpc('http://127.0.0.1:18557','anvil_mine',['0x40','0x0'])
        self.assertEqual([r[1]['method']for r in fake.requests],['eth_chainId'])
    def test_local_admin_verifies31337_before_exact_mutation(self):
        fake=FakeOpener(['0x7a69',None])
        with patch.object(SMOKE,'OPENER',fake):self.assertIsNone(SMOKE.rpc('http://127.0.0.1:18557','anvil_mine',['0x40','0x0']))
        self.assertEqual([r[1]['method']for r in fake.requests],['eth_chainId','anvil_mine'])
        self.assertEqual(fake.requests[1][1]['params'],['0x40','0x0'])
    def test_allowed_primary_read_preserves_method_and_fixed_tag(self):
        fake=FakeOpener([{'hash':SMOKE.FORK_HASH}])
        with patch.object(SMOKE,'OPENER',fake):SMOKE.rpc(SMOKE.PRIMARY,'eth_getBlockByNumber',[hex(SMOKE.FORK_BLOCK),False])
        self.assertEqual(fake.requests[0][1]['params'],[hex(SMOKE.FORK_BLOCK),False])
    def test_http_redirect_never_creates_second_request(self):
        with self.assertRaisesRegex(RuntimeError,'redirects forbidden'):SMOKE.NoRedirect().redirect_request(None,None,302,'moved',{},SMOKE.PRIMARY)
    def test_environment_proxy_handler_has_no_destinations(self):
        self.assertFalse(any(getattr(handler,'proxies',{})for handler in SMOKE.OPENER.handlers))

class FreshWorkspacePolicy(unittest.TestCase):
    def argv(self,directory,mode='plain',execute=True):
        argv=['runner','--mode',mode,'--artifacts','/unused/artifacts.json','--output',str(directory),'--anvil','/unused/anvil','--cast','/unused/cast','--ctl','/unused/ctl','--bot','/unused/bot']
        return argv+(['--execute-local']if execute else [])
    def test_missing_artifact_bundle_stops_before_output_or_rpc(self):
        with tempfile.TemporaryDirectory() as temp:
            destination = pathlib.Path(temp) / 'fresh'
            argv = self.argv(destination)
            index = argv.index('--artifacts')
            del argv[index:index + 2]
            with patch.object(sys, 'argv', argv), patch('sys.stderr', new=io.StringIO()), patch.object(SMOKE, 'rpc') as rpc:
                with self.assertRaises(SystemExit) as error:
                    SMOKE.main()
                self.assertEqual(error.exception.code, 2)
                rpc.assert_not_called()
                self.assertFalse(destination.exists())
    def test_existing_output_rejected_before_loading_or_rpc(self):
        with tempfile.TemporaryDirectory()as temp,patch.object(SMOKE,'load_module')as load,patch.object(SMOKE,'rpc')as rpc,patch.object(sys,'argv',self.argv(pathlib.Path(temp))):
            with self.assertRaises(FileExistsError):SMOKE.main()
            load.assert_not_called();rpc.assert_not_called()
    def test_missing_local_execution_opt_in_has_no_side_effects(self):
        with tempfile.TemporaryDirectory()as temp:
            destination=pathlib.Path(temp)/'fresh'
            with patch.object(sys,'argv',self.argv(destination,execute=False)),patch.object(SMOKE,'rpc')as rpc:
                with self.assertRaisesRegex(RuntimeError,'execute-local'):SMOKE.main()
                rpc.assert_not_called();self.assertFalse(destination.exists())
    def test_fork_read_opt_in_is_required_before_output_or_rpc(self):
        with tempfile.TemporaryDirectory()as temp:
            destination=pathlib.Path(temp)/'fresh'
            with patch.object(sys,'argv',self.argv(destination,mode='fork')),patch.object(SMOKE,'rpc')as rpc:
                with self.assertRaisesRegex(RuntimeError,'testnet opt-in'):SMOKE.main()
                rpc.assert_not_called();self.assertFalse(destination.exists())
    def test_external_resume_options_are_not_accepted(self):
        with tempfile.TemporaryDirectory()as temp,patch.object(sys,'argv',self.argv(pathlib.Path(temp)/'fresh')+['--resume-common-args','/old/runtime/common.json']),patch.object(SMOKE,'rpc')as rpc:
            with patch('sys.stderr',new=io.StringIO()),self.assertRaises(SystemExit)as error:SMOKE.main()
            self.assertEqual(error.exception.code,2);rpc.assert_not_called()

if __name__=='__main__':unittest.main()
