#!/usr/bin/env python3
"""Local orchestration failure and recovery checks, without keys or RPC."""
import importlib.util
import pathlib
import tempfile
import json
import types
import io
import sys
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("testnet_orchestrator", pathlib.Path(__file__).resolve().parents[2] / 'tests/runners/run-testnet.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

extra_spec = importlib.util.spec_from_file_location('extra_scenarios', pathlib.Path(__file__).resolve().parents[2] / 'tests/runners/testnet-scenarios.py')
extra = importlib.util.module_from_spec(extra_spec)
extra_spec.loader.exec_module(extra)


class ExtraScenarioRecovery(unittest.TestCase):
    def test_bot_events_read_current_text_and_historical_json_without_losing_payloads(self):
        expected={'event':'stopped','active_pending':False,'text':'quotes " and newline\nend'}
        lines=[
            '2026-11-04T08:00:00.123Z  INFO barterlend_liquidator::diagnostics_io: event="stopped" details='+json.dumps(expected),
            json.dumps({'event':'stopped','details':json.dumps(expected)}),
            json.dumps(expected),
            '2026-11-04T08:00:01Z DEBUG barterlend_liquidator::routes: close time.busy=1ms time.idle=2ms',
        ]
        with tempfile.TemporaryDirectory() as directory:
            path=pathlib.Path(directory)/'bot.log';path.write_text('\n'.join(lines)+'\n')
            self.assertEqual(module.Runner.bot_events(path),[expected]*3)
            path.write_text('2026-11-04T08:00:00Z ERROR bot: event="stopped" details={broken}\n')
            with self.assertRaises(json.JSONDecodeError):module.Runner.bot_events(path)

    def test_extra_cleanup_keeps_liquidity_until_every_extra_market_matures(self):
        position = {'pool':'0x'+'11'*20,'market':200,'borrower':'0x'+'22'*20,'actor':'borrower'}
        for now in [100, 300]:
            runner = types.SimpleNamespace(state={'scenario_market':50,'extra_scenarios':{'extra':{'position':position}}},
                context={'credit_pool':'0x'+'33'*20,'lender':'lender','owner':'owner','bootstrap':'bootstrap',
                         'asset':'asset','collateral':'collateral','wrapped_native':'wrapped'},
                args=types.SimpleNamespace(wait_market=False,market_wait_seconds=600),
                bot_journal=mock.Mock(return_value=(None,{'active':None})),
                ctl=mock.Mock(return_value={'latest_timestamp':now}), cleanup=mock.Mock(), send=mock.Mock(),
                assertion=mock.Mock(return_value={'test':'assertion'}))
            runner.scalar = lambda to, method, *args: 0 if method.startswith('allowance') else 20
            with mock.patch.object(extra,'close_position') as close:
                extra.cleanup(runner)
            close.assert_called_once()
            if now < 200:
                runner.send.assert_not_called();runner.cleanup.assert_not_called()
            else:
                self.assertEqual([c.args[2] for c in runner.send.call_args_list],
                                 ['claim(address,uint64,uint128)','withdraw(address,address,uint128)'])
                runner.cleanup.assert_called_once()

    def test_synthetic_price_reset_uses_actual_router_abi_and_replays_no_new_mint(self):
        fields = [('tokenIn','address'),('tokenOut','address'),('fee','uint24'),('recipient','address'),
                  ('amountIn','uint256'),('amountOutMinimum','uint256'),('sqrtPriceLimitX96','uint160')]
        entry = {'name': 'exactInputSingle', 'inputs': [{'type':'tuple', 'components': [{'name':n,'type':t} for n,t in fields]}]}
        for high in [True, False]:
            q = 2**96
            runner = types.SimpleNamespace(state={'v3_pools':{'collateral_asset':'pool'}},
                context={'asset':'0x'+'11'*20,'collateral':'0x'+'22'*20,'router':'0x'+'33'*20,'owner':'0x'+'44'*20},
                call=mock.Mock(side_effect=['0x'+format(2*q if high else q//2,'064x'), '0x'+format(q,'064x')]),
                scalar=mock.Mock(return_value=100*10**18), save=mock.Mock(), send=mock.Mock(),
                contracts={'SwapRouter02':{'abi':[entry]}})
            runner.address = lambda pool, getter: runner.context['asset' if getter=='token0()' else 'collateral']
            extra.restore_synthetic_pair_price(runner, 'reset')
            sends = runner.send.call_args_list
            self.assertEqual(sends[0].args[2], 'mint(address,uint256)')
            self.assertEqual(sends[0].args[1], runner.context['asset' if high else 'collateral'])
            self.assertEqual(sends[2].args[2], 'exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))')
            self.assertEqual(sends[3].args[3][-1], 0)
            self.assertTrue(runner.state['amm_resets']['reset']['passed'])
            runner.send.reset_mock()
            extra.restore_synthetic_pair_price(runner, 'reset')
            runner.send.assert_not_called()

    def test_only_read_observations_retry_transient_http_failures(self):
        runner = object.__new__(module.Runner)
        runner.common = mock.Mock(return_value=['ctl'])
        runner.tool = mock.Mock(side_effect=[RuntimeError('RPC HTTP status/size rejected'), '{"ok":true}'])
        with mock.patch.object(module.time, 'sleep') as sleep:
            self.assertEqual(runner.ctl('inspect'), {'ok': True})
        self.assertEqual(runner.tool.call_count, 2)
        sleep.assert_called_once_with(1)
        for command, key in [('send', True), ('inspect', True), ('deploy', False)]:
            runner.tool = mock.Mock(side_effect=RuntimeError('RPC HTTP status/size rejected'))
            with self.assertRaises(RuntimeError): runner.ctl(command, key=key)
            self.assertEqual(runner.tool.call_count, 1)

    def test_read_retry_has_a_finite_attempt_limit(self):
        runner = object.__new__(module.Runner)
        runner.common = mock.Mock(return_value=['ctl'])
        runner.tool = mock.Mock(side_effect=RuntimeError('RPC HTTP status/size rejected'))
        with mock.patch.object(module.time, 'sleep') as sleep, self.assertRaises(RuntimeError):
            runner.ctl('call')
        self.assertEqual(runner.tool.call_count, 3)
        self.assertEqual(sleep.call_args_list, [mock.call(1), mock.call(2)])

    def test_cast_json_envelopes_never_hide_errors(self):
        expected = {'timestamp': '0x42'}
        self.assertEqual(extra.cast_json(json.dumps(expected)), expected)
        self.assertEqual(extra.cast_json(json.dumps({'schema_version': 1, 'success': True, 'errors': [], 'data': expected})), expected)
        for value in [[], {'schema_version': 1, 'success': False, 'data': expected},
                      {'schema_version': 1, 'success': True, 'errors': ['failed'], 'data': expected}]:
            with self.assertRaises(RuntimeError): extra.cast_json(json.dumps(value))

    def test_maturity_audit_resume_never_replays_mutations(self):
        runner = types.SimpleNamespace(state={'primary_liquidation_passed': True,
            'extra_scenarios': {'maturity-sized': {'stage': 'running'}}})
        with mock.patch.object(extra, 'finish_maturity') as audit:
            extra.maturity(runner)
        audit.assert_called_once_with(runner, 'maturity-sized', {'stage': 'running'})

    def test_completed_cleanup_rechecks_chain_without_repeating_deposit(self):
        runner = types.SimpleNamespace(state={'extra_cleanup': {'case': {'passed': True}}},
            scalar=mock.Mock(return_value=0), send=mock.Mock())
        position = {'pool': 'pool', 'actor': 'actor', 'borrower': 'borrower', 'market': 100}
        extra.close_position(runner, 'case', position)
        runner.send.assert_not_called()
        runner.scalar.return_value = 1
        with self.assertRaises(RuntimeError): extra.close_position(runner, 'case', position)


class ExplicitRunInputs(unittest.TestCase):
    def argv(self):
        return ['runner', '--genesis', '0x' + '11' * 32, '--cast', '/unused/cast',
                '--owner', '0x' + '01' * 20, '--artifacts', '/unused/artifacts.json',
                '--evidence-dir', '/unused/evidence', '--secret-dir', '/unused/private']

    def test_historical_paths_and_owner_are_never_implicit(self):
        for flag in ['--owner', '--artifacts', '--evidence-dir', '--secret-dir']:
            argv = self.argv()
            index = argv.index(flag)
            del argv[index:index + 2]
            with self.subTest(flag=flag), mock.patch.object(sys, 'argv', argv), mock.patch('sys.stderr', new=io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    module.parse_args()
                self.assertEqual(error.exception.code, 2)

    def test_plan_retains_explicit_run_inputs_without_reading_files(self):
        with mock.patch.object(sys, 'argv', self.argv()), mock.patch.object(pathlib.Path, 'read_text') as read:
            args = module.parse_args()
        read.assert_not_called()
        self.assertEqual(args.owner, '0x' + '01' * 20)
        self.assertEqual(args.artifacts, pathlib.Path('/unused/artifacts.json'))
        self.assertEqual(args.evidence_dir, pathlib.Path('/unused/evidence'))
        self.assertEqual(args.secret_dir, pathlib.Path('/unused/private'))


class Controls(unittest.TestCase):
    def test_live_observer_waits_for_initial_health_and_idle_finalization(self):
        for value in [None, {}, {"counters": {"finalized_liquidations": 0}},
                      {"counters": {"finalized_liquidations": 1}, "tx": {"phase": "pending"}}]:
            self.assertFalse(module.finalized_health(value, 1))
        self.assertTrue(module.finalized_health({"counters": {"finalized_liquidations": 1}, "tx": {"phase": "idle"}}, 1))
        with self.assertRaises(RuntimeError):
            module.finalized_health({"counters": {"finalized_liquidations": 2}, "tx": {"phase": "idle"}}, 1)

    def test_public_mutation_requires_explicit_execute_and_key_path(self):
        for phase in ["deploy", "positions", "guards", "amm-guard", "oracle-refresh", "liquidate", "cleanup"]:
            argv = ExplicitRunInputs().argv() + ["--phase", phase]
            with self.subTest(phase=phase), mock.patch.object(sys, "argv", argv), mock.patch("sys.stderr", new=io.StringIO()):
                with self.assertRaises(SystemExit): module.parse_args()
        argv = ExplicitRunInputs().argv() + ["--phase", "deploy", "--execute", "--owner-key-file", "/external/key", "--owner-start-nonce", "42"]
        with mock.patch.object(sys, "argv", argv):
            self.assertEqual(module.parse_args().owner_start_nonce, 42)

    def runner(self, retained=(), resumed=False, result=None):
        runner = object.__new__(module.Runner)
        runner.state = {"steps": {}}
        runner.saves = []
        runner.calls = []
        runner.save = lambda: runner.saves.append(dict(runner.state["steps"]))
        def ctl(*command, **kwargs):
            runner.calls.append(command[0])
            if command[0] == "status":
                return {"entries": list(retained)}
            if command[0] == "resume":
                return {"id": "step", "finalized": True}
            return result or {"id": "step", "hash": "0x1234", "gas_used": 123,
                              "finalized": True, "assertions_passed": True}
        runner.ctl = ctl
        return runner

    def old(self, runner, request, status):
        runner.state["steps"]["step"] = {"actor": "owner", "request_sha256": module.sha(module.canonical(request)), "status": status}

    def test_changed_signed_request_never_calls_control_tool(self):
        runner = self.runner()
        self.old(runner, {"amount": 1}, "passed")
        with self.assertRaises(RuntimeError):
            runner.run_step("step", {"amount": 2}, ["send"])
        self.assertEqual(runner.calls, [])

    def test_completed_step_requires_retained_signed_journal(self):
        runner = self.runner()
        self.old(runner, {"amount": 1}, "passed")
        with self.assertRaises(RuntimeError):
            runner.run_step("step", {"amount": 1}, ["send"])
        self.assertEqual(runner.calls, ["status"])

    def test_uncertain_before_signing_retries_without_empty_resume(self):
        runner = self.runner()
        self.old(runner, {"amount": 1}, "uncertain")
        runner.run_step("step", {"amount": 1}, ["send"])
        self.assertEqual(runner.calls, ["status", "send"])
        self.assertEqual(runner.state["steps"]["step"]["status"], "passed")

    def test_signed_pending_is_resumed_before_idempotent_send(self):
        runner = self.runner([{"id": "step", "validated": False}])
        self.old(runner, {"amount": 1}, "uncertain")
        runner.run_step("step", {"amount": 1}, ["send"])
        self.assertEqual(runner.calls, ["status", "resume", "send"])

    def test_completed_id_is_revalidated_without_new_signing(self):
        runner = self.runner([{"id": "step", "validated": True}])
        self.old(runner, {"amount": 1}, "passed")
        runner.run_step("step", {"amount": 1}, ["send"])
        self.assertEqual(runner.calls, ["status", "send"])

    def test_invalid_public_outcome_retains_uncertain_state(self):
        runner = self.runner(result={"id": "other", "finalized": True, "assertions_passed": True})
        with self.assertRaises(RuntimeError):
            runner.run_step("step", {}, ["send"])
        self.assertEqual(runner.state["steps"]["step"]["status"], "uncertain")
        self.assertGreaterEqual(len(runner.saves), 2)

    def test_recursive_tuple_array_and_bool_encoding(self):
        abi = {"type": "tuple", "components": [{"type": "address[]"}, {"type": "tuple", "components": [{"type": "uint64[3]"}, {"type": "bool"}]}]}
        self.assertEqual(module.abi_type(abi), "(address[],(uint64[3],bool))")
        self.assertEqual(module.abi_arg(abi, [["0x11", "0x22"], [[600, 600, 600], True]]), "([0x11,0x22],([600,600,600],true))")
        with self.assertRaises(RuntimeError):
            module.abi_arg(abi, [[], []])

    def test_receipt_config_is_bytes32_and_known_price_expression_is_bounded(self):
        context = {"receipt": "0x" + "ab" * 20, "asset": "0x01", "wrapped_native": "0x02"}
        self.assertEqual(module.resolve("${receipt_bytes32_left_padded}", context), "0x" + "00" * 12 + "ab" * 20)
        self.assertEqual(module.resolve("${asset_lt_wrapped ? 32 : 1024}", context), "32")
        with self.assertRaises(RuntimeError):
            module.resolve("${asset_lt_wrapped ? __import__('os') : 1}", context)
        with self.assertRaises(RuntimeError):
            module.resolve("${missing}", context)

    def test_atomic_public_report_refuses_symlink_target(self):
        with tempfile.TemporaryDirectory() as name:
            directory = pathlib.Path(name)
            real = directory / "real"
            real.write_text("untouched")
            link = directory / "link"
            link.symlink_to(real)
            with self.assertRaises(RuntimeError):
                module.atomic_json(link, {"changed": True})
            self.assertEqual(real.read_text(), "untouched")

    def scenario_runner(self, directory):
        runner = object.__new__(module.Runner)
        runner.directory = pathlib.Path(directory)
        runner.args = types.SimpleNamespace(owner="0x" + "01" * 20, secret_dir=runner.directory / "private",
                                            genesis="0x" + "11" * 32, wait_market=False, market_wait_seconds=0)
        runner.context = {key: "0x" + format(i, "040x") for i, key in enumerate(
            ["credit_pool", "asset", "collateral", "oracle", "executor", "borrower", "lender", "operator", "quoter", "router", "wrapped_native", "bootstrap"], 1)}
        runner.binding = {"chain_id": 10143}
        runner.state = {"steps": {}, "scenario_market": 200}
        runner.save = lambda: None
        runner.assertion = lambda *args: {"getter": args[1], "expected": args[-1]}
        return runner

    def test_new_manifest_policy_uses_standard60seconds_200blocks(self):
        template = {"additional_profit_asset": "0x3e8", "planner": {"ttl_seconds": 30}, "v3": {"limits": {"quote_ttl_seconds": 30, "max_age_blocks": 20, "slippage_bps": 100}},
                    "fees": {"max_gas_limit": 2000000}, "rpc": {"requests_per_second": 1000}}
        for local, proxy, profile, ttl, age in [(False, False, "monad-testnet", 60, 200),
                                               (False, True, "monad-testnet-proxy", 60, 200),
                                               (True, False, "local", 60, 200)]:
            with self.subTest(profile=profile), tempfile.TemporaryDirectory() as directory:
                runner = self.scenario_runner(directory)
                runner.args.local_rehearsal, runner.args.testnet_proxy = local, proxy
                runner.args.rpc = "http://127.0.0.1:8545" if local or proxy else "https://testnet-rpc.monad.xyz"
                runner.args.max_gas_price_wei = 150000000000
                runner.args.rpc_fallback_urls = [] if local or proxy else ["https://alternate.example.org"]
                runner.binding["chain_id"] = 31337 if local else 10143
                runner.context["factory"] = "0x" + "ab" * 20
                runner.state["contracts"] = {name: {"contract_address": runner.context[name], "runtime_code_hash": "0x" + "11" * 32,
                                                    "block_number": 10, "block_hash": "0x" + "22" * 32}
                                             for name in ["executor", "credit_pool", "oracle", "factory", "router", "quoter", "wrapped_native"]}
                runner.address = mock.Mock(return_value="0x" + "33" * 20)
                with mock.patch.object(pathlib.Path, "read_text", return_value=json.dumps(template)):
                    runner.write_manifest()
                manifest = json.loads((runner.directory / "bot-manifest.json").read_text())
                self.assertEqual(manifest["profile"], profile)
                self.assertEqual(manifest["rpc_fallback_urls"], runner.args.rpc_fallback_urls)
                self.assertEqual(manifest["rpc"], {"requests_per_second": 1000 if local else 15})
                self.assertNotIn("postgres", manifest)
                self.assertEqual(manifest["cancellation_retry_seconds"], 30)
                self.assertEqual(manifest["sdk_history_chunks_per_step"], 8)
                self.assertEqual(manifest["recovery"]["retry_max_ms"], 30000)
                self.assertEqual(int(manifest["additional_profit_asset"], 0), 0)
                self.assertEqual(manifest["planner"]["ttl_seconds"], ttl)
                self.assertEqual(manifest["v3"]["limits"]["quote_ttl_seconds"], ttl)
                self.assertEqual(manifest["v3"]["limits"]["max_age_blocks"], age)
                self.assertLessEqual(age, 2000)
                self.assertEqual(manifest["v3"]["limits"]["slippage_bps"], 100)
                self.assertEqual(manifest["v3"]["limits"]["max_quote_calls"], 16)
                self.assertEqual(manifest["fees"]["max_gas_price"], hex(150000000000))
                self.assertEqual(manifest["fees"]["native_reserve"], hex(module.WAD // 100))
                self.assertEqual(manifest["fees"]["max_gas_limit"], 2000000)
                self.assertEqual(manifest["chain"]["chain_id"], runner.binding["chain_id"])
                self.assertNotIn("runtime_dir", manifest)
                self.assertEqual(manifest["operator"], runner.context["operator"])
                self.assertEqual(manifest["executor_code_hash"], runner.state["contracts"]["executor"]["runtime_code_hash"])
                self.assertEqual(runner.state["manifest_sha256"], module.sha((runner.directory / "bot-manifest.json").read_bytes()))
                runner.address.assert_called()

    def test_active_markets_rejects_wrong_offset_duplicate_and_uint64_overflow(self):
        runner = object.__new__(module.Runner)
        runner.context = {"credit_pool": "0x01"}
        def encoded(offset, values):
            return "0x" + b"".join(word.to_bytes(32, "big") for word in [offset, len(values), *values]).hex()
        runner.call = lambda *args: encoded(32, [600, 1200])
        self.assertEqual(runner.active_markets(), [600, 1200])
        for invalid in [encoded(0, [600]), encoded(32, [600, 600]), encoded(32, [2**64])]:
            runner.call = lambda *args, value=invalid: value
            with self.assertRaises(RuntimeError):
                runner.active_markets()

    def test_actual_amm_resume_after_restoration_does_not_repeat_negative_census(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.scenario_runner(directory)
            (runner.directory / "bot-manifest.json").write_text("{}")
            runner.state.update(guards_passed=True, guard_cases={"actual-amm-unprofitable": {"passed": True, "skip_events": [{"reason": "unprofitable_route"}]}},
                                amm_guard={"asset_balance_before": 1, "restore_amount_asset": 50,
                                           "loss_quote_all_collateral": 40, "actual_full_debt": 100})
            runner.send = lambda *args, **kwargs: None
            runner.router_swap = lambda *args: None
            runner.action = lambda *args: None
            runner.ensure_oracle = lambda *args: None
            runner.quote_input = lambda *args: 150
            runner.guard_case = lambda *args: self.fail("restored liquidity must not repeat the loss census")
            runner.scalar = lambda *args: self.fail("immutable AMM snapshots must not be reconstructed after restoration")
            runner.amm_guard()
            self.assertTrue(runner.state["amm_guard_passed"])

    def test_completed_amm_negative_guard_requires_immutable_loss_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.scenario_runner(directory)
            (runner.directory / "bot-manifest.json").write_text("{}")
            runner.state.update(guards_passed=True, guard_cases={"actual-amm-unprofitable": {"passed": True, "skip_events": [{"reason": "unprofitable_route"}]}},
                                amm_guard={"asset_balance_before": 1, "restore_amount_asset": 50})
            runner.send = lambda *args, **kwargs: None
            runner.router_swap = lambda *args: None
            runner.ensure_oracle = lambda *args: None
            with self.assertRaisesRegex(RuntimeError, "immutable actual loss"):
                runner.amm_guard()

    def test_amm_never_repeats_no_send_guard_on_restored_profitable_market(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.scenario_runner(directory)
            runner.state.update(guards_passed=True, guard_cases={"actual-amm-unprofitable": {"passed": True}},
                                steps={"amm-recovery-swap": {"status": "uncertain"}})
            runner.guard_case = lambda *args: self.fail("restored pool cannot safely be used as a loss no-send trial")
            with self.assertRaisesRegex(RuntimeError, "after liquidity was restored"):
                runner.amm_guard()

    def guard_runner(self, directory, debt_positive=1, emitted="gas_price_cap_exceeded", label="fee-cap"):
        runner = self.scenario_runner(directory)
        runner.state.update(guard_cases={"fee-cap": {"passed": True, "skip_events": [{"reason": "unclassified_redacted"}]}}, bot_runs=[])
        runner.test_health = {"pools": [{"verified": True, "debt_positive": debt_positive,
            "eligible": 1, "oracle_status": "ready", "risk_unavailable": 0}]}
        runner.ctl = lambda *args, **kwargs: {"latest_nonce": 0, "pending_nonce": 0, "balance_wei": "0x1"}
        def run(*args, **kwargs):
            log = runner.directory / "mock-guard.jsonl"
            log.write_text(json.dumps({"event": "position_skipped", "reason": emitted}) + "\n")
            runner.state["bot_runs"].append({"stdout": str(log)})
            return {"finalized_liquidations": 0, "active_pending": False, "health": runner.test_health}
        runner.bot_run = run
        return runner

    def test_stale_guard_requires_exact_oracle_failure_not_rpc_outage(self):
        for kind in ["stale", "rpc"]:
            with tempfile.TemporaryDirectory() as directory:
                runner=self.guard_runner(directory,label="stale-oracle")
                runner.test_health={"pools":[{"verified":True,"debt_positive":1,"eligible":None,
                    "oracle_status":"unavailable","oracle_failure":kind,"risk_unavailable":1}]}
                if kind=="stale":
                    runner.guard_case("stale-oracle",{"planner":{},"v3":{}},"stale",False)
                    self.assertEqual(runner.state["guard_cases"]["stale-oracle"]["state_observation"]["oracle_failure"],"stale")
                else:
                    with self.assertRaisesRegex(RuntimeError,"exact SDK stale"):
                        runner.guard_case("stale-oracle",{"planner":{},"v3":{}},"stale",False)

    def oracle_runner(self, directory, initial):
        runner = self.scenario_runner(directory)
        runner.test_observation = initial
        runner.test_head = 100
        runner.ctl = lambda *args, **kwargs: {"latest_timestamp": runner.test_head}
        runner.oracle_observation = lambda: runner.test_observation
        runner.test_oracle_sends = []
        def send(step, to, method, args, **kwargs):
            self.assertIn(method, ["setObservation(uint256,uint256,uint256)", "setPriceFraction(uint256,uint256)"])
            runner.test_oracle_sends.append((step, args))
            runner.test_head += 2
            runner.test_observation = (*args, runner.test_head) if method == "setPriceFraction(uint256,uint256)" else tuple(args)
            runner.state["steps"][step] = {"actor": "owner", "status": "passed"}
        runner.send = send
        return runner

    def test_oracle_retry_after_stale_uses_new_unique_steps_not_historical_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.oracle_runner(directory, (4, 10, 0))
            runner.state["steps"]["old-eligible-price"] = {"actor": "owner", "status": "passed"}
            runner.ensure_oracle("eligible", 4, 10)
            runner.ensure_oracle("stale", 4, 10, 0)
            runner.ensure_oracle("eligible", 4, 10)
            ids = [item[0] for item in runner.test_oracle_sends]
            self.assertEqual(len(set(ids)), 3)
            self.assertEqual(runner.test_observation, (4, 10, runner.test_head))
            self.assertEqual(runner.state["steps"]["old-eligible-price"], {"actor": "owner", "status": "passed"})

    def test_fresh_oracle_timestamp_comes_from_execution_block_not_predicted_initial_head(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.oracle_runner(directory, (4, 10, 0))
            observation = runner.ensure_oracle("fresh", 4, 10)
            self.assertEqual(observation, (4, 10, 102))
            self.assertEqual(runner.test_oracle_sends[0][1], [4, 10])
            self.assertEqual(runner.state["oracle_controls"][0]["observed_head_timestamp"], 100)

    def test_oracle_uncertain_owner_step_is_reconciled_before_current_state_or_new_nonce(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.oracle_runner(directory, (4, 10, 0))
            runner.state["steps"]["pending-oracle"] = {"actor": "owner", "status": "uncertain"}
            plans = runner.directory / "plans"
            plans.mkdir()
            (plans / "pending-oracle.json").write_text('{"id":"pending-oracle"}')
            order = []
            def reconcile(*args):
                order.append("reconcile")
                runner.state["steps"]["pending-oracle"]["status"] = "passed"
                runner.test_observation = (4, 10, 100)
            runner.run_step = reconcile
            runner.ctl = lambda *args: order.append("inspect") or {"latest_timestamp": 100}
            runner.oracle_observation = lambda: order.append("read-price") or runner.test_observation
            runner.ensure_oracle("eligible", 4, 10)
            self.assertEqual(order, ["reconcile", "inspect", "read-price"])
            self.assertEqual(runner.test_oracle_sends, [])

    def test_oracle_missing_uncertain_original_plan_blocks_before_any_fresh_send(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.oracle_runner(directory, (4, 10, 0))
            runner.state["steps"]["pending-oracle"] = {"actor": "owner", "status": "uncertain"}
            runner.ctl = lambda *args: self.fail("must reconcile original plan before any new chain read/nonce")
            with self.assertRaisesRegex(RuntimeError, "original plan"):
                runner.ensure_oracle("eligible", 4, 10)
            self.assertEqual(runner.test_oracle_sends, [])

    def test_strict_fee_revalidation_preserves_old_unclassified_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.guard_runner(directory)
            runner.guard_case("fee-cap", {"planner": {}, "v3": {}}, "ready", True, "gas_price_cap_exceeded")
            self.assertEqual(runner.state["guard_history"]["fee-cap"][0]["skip_events"][0]["reason"], "unclassified_redacted")
            self.assertEqual(runner.state["guard_cases"]["fee-cap"]["skip_events"][0]["reason"], "gas_price_cap_exceeded")

    def test_guard_does_not_accept_empty_census_or_wrong_skip_reason(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.guard_runner(directory, debt_positive=0)
            with self.assertRaisesRegex(RuntimeError, "nonempty debt census"):
                runner.guard_case("fee-cap", {"planner": {}, "v3": {}}, "ready", True, "gas_price_cap_exceeded")
        with tempfile.TemporaryDirectory() as directory:
            runner = self.guard_runner(directory, emitted="unprofitable_route")
            with self.assertRaisesRegex(RuntimeError, "concrete rejection reason"):
                runner.guard_case("fee-cap", {"planner": {}, "v3": {}}, "ready", True, "gas_price_cap_exceeded")

    def archived_outcome(self, runner, cancel=False):
        meta = {"key": {"pool": runner.context["credit_pool"], "borrower": runner.context["borrower"], "market": 200},
                "asset": runner.context["asset"], "min_asset_profit": "0xa"}
        attempt = {"kind": {"Liquidation": meta}, "envelope": {"gas_limit": 50000, "gas_price": "0xa", "nonce": 0,
                   "to": runner.context["executor"], "input": "0x01", "chain_id": 10143},
                   "hash": "0x" + "aa" * 32, "raw": "0xdeadbeef"}
        chosen = dict(attempt, kind="Cancel", hash="0x" + "bb" * 32,
                      envelope=dict(attempt["envelope"], gas_limit=21000, to=runner.context["operator"], input="0x", input_hash="0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")) if cancel else attempt
        return {"attempt": chosen, "attempts": [attempt, chosen] if cancel else [attempt],
                "kind": "Cancelled" if cancel else "Liquidated", "native_charge": hex(chosen["envelope"]["gas_limit"] * 10),
                "asset_gain": None if cancel else "0x14", "net_asset": None,
                "receipt": {"succeeded": True, "hash": chosen["hash"], "block_hash": "0x" + "cc" * 32,
                            "block_number": 1, "effective_gas_price": "0xa", "gas_used": 21000, "logs": []}}

    def test_outcome_rejects_unrelated_paid_cancel_and_redacts_success_signed_attempts(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.scenario_runner(directory)
            path = runner.directory / "private-journal.json"
            path.write_text("synthetic source kept only for fingerprint")
            unrelated = {"attempt": {"kind": "Cancel"}, "attempts": [], "kind": "Cancelled"}
            runner.bot_journal = lambda: (path, {"active": None, "outcomes": [unrelated, self.archived_outcome(runner)]})
            with self.assertRaisesRegex(RuntimeError, "unrelated paid outcome"):
                runner.outcome()
            self.assertFalse((runner.directory / "LIQUIDATION_OUTCOME.json").exists())
            runner.bot_journal = lambda: (path, {"active": None, "outcomes": [self.archived_outcome(runner)]})
            runner.call = lambda *args: "0x" + format(1, "064x")
            runner.scalar = lambda to, *args: 0 if to == runner.context["credit_pool"] else 20
            runner.outcome()
            report = (runner.directory / "LIQUIDATION_OUTCOME.json").read_text()
            self.assertNotIn("deadbeef", report)
            self.assertNotIn('"raw"', report)
            self.assertTrue(runner.state["primary_liquidation_passed"])

    def test_primary_paid_cancellation_without_success_is_not_a_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.scenario_runner(directory)
            runner.bot_journal = lambda: (runner.directory / "unused", {"active": None, "outcomes": [self.archived_outcome(runner, cancel=True)]})
            with self.assertRaisesRegex(RuntimeError, "exactly one liquidation"):
                runner.outcome()

    def cleanup_runner(self, directory, mature=False):
        runner = self.scenario_runner(directory)
        report = runner.directory / "LIQUIDATION_OUTCOME.json"
        report.write_text('{"passed":true}')
        runner.state.update(primary_liquidation_passed=True, outcome_report_sha256=module.sha(report.read_bytes()),
                            v3_pools={"collateral_asset": "0x01", "asset_native": "0x02"})
        runner.bot_journal = lambda: (None, {"active": None})
        runner.ctl = lambda *args: {"latest_timestamp": 200 if mature else 100}
        values = {"debtOf(uint64,address)": 0, "collateralOf(uint64,address)": 20,
                  "loanOf(uint64,address)": 100, "getMarketBalance(uint64)": 100}
        executor, wrapped, pool = (runner.context[key] for key in ["executor", "wrapped_native", "credit_pool"])
        balances = {executor: 10, pool: 0, wrapped: 0}
        def scalar(to, method, *args):
            if method == "liquidity()":
                label = next(label for label, address in runner.state["v3_pools"].items() if address == to)
                return 0 if "cleanup-lp-" + label in runner.state["steps"] else 1
            if method == "balanceOf(address)":
                return balances[executor] if to == runner.context["asset"] and args[0] == [executor] else balances.get(to, 0)
            return values[method]
        runner.scalar = scalar
        runner.sent = []
        def send(step, to, method, args, **kwargs):
            runner.sent.append((step, method, args, kwargs.get("actor", "owner")))
            if step == "cleanup-owner-profit": balances[executor] = 0
            if step == "cleanup-borrower-collateral": values["collateralOf(uint64,address)"] = 0
            if step == "cleanup-lender-claim": values["loanOf(uint64,address)"] = 0; balances[pool] = 100
            if step == "cleanup-lender-withdraw": balances[pool] = 0
            if step == "cleanup-lp-asset_native": balances[wrapped] = module.WAD
            if step == "cleanup-native-unwrap": balances[wrapped] = 0
            runner.state["steps"][step] = {"status": "passed"}
        runner.send = send
        return runner

    def test_cleanup_waits_for_actual_market_without_early_claim_or_lp_removal(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.cleanup_runner(directory)
            runner.cleanup()
            self.assertEqual([item[0] for item in runner.sent], ["cleanup-owner-profit", "cleanup-borrower-collateral"])
            self.assertEqual(runner.state["phase"], "cleanup_pending_actual_market_maturity")
            self.assertNotIn("cleanup_passed", runner.state)

    def test_cleanup_mature_claims_actual_roles_collects_lp_and_unwraps_honest_mon(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.cleanup_runner(directory, mature=True)
            runner.cleanup()
            self.assertTrue(runner.state["cleanup_passed"])
            self.assertEqual([item[0] for item in runner.sent], ["cleanup-owner-profit", "cleanup-borrower-collateral", "cleanup-lender-claim",
                "cleanup-lender-withdraw", "cleanup-lp-collateral_asset", "cleanup-lp-asset_native", "cleanup-native-unwrap"])
            self.assertEqual([item[3] for item in runner.sent[:4]], ["owner", "borrower", "lender", "lender"])

    def test_cleanup_refuses_changed_accepted_profit_report(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.cleanup_runner(directory, mature=True)
            (runner.directory / "LIQUIDATION_OUTCOME.json").write_text("changed")
            with self.assertRaisesRegex(RuntimeError, "report changed"):
                runner.cleanup()
            self.assertEqual(runner.sent, [])

    def test_cleanup_requires_available_market_balance_and_zero_actual_lp_liquidity(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = self.cleanup_runner(directory, mature=True)
            original = runner.scalar
            runner.scalar = lambda to, method, *args: 0 if method == "getMarketBalance(uint64)" else original(to, method, *args)
            with self.assertRaisesRegex(RuntimeError, "available lender loan"):
                runner.cleanup()
            self.assertFalse(any(item[0] == "cleanup-lender-claim" for item in runner.sent))
        with tempfile.TemporaryDirectory() as directory:
            runner = self.cleanup_runner(directory, mature=True)
            original = runner.scalar
            runner.scalar = lambda to, method, *args: 1 if method == "liquidity()" else original(to, method, *args)
            with self.assertRaisesRegex(RuntimeError, "post-cleanup"):
                runner.cleanup()
            self.assertNotIn("cleanup_passed", runner.state)


if __name__ == "__main__":
    unittest.main()
