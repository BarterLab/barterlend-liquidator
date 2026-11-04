//! Local Anvil only, forked from a pinned public block by tests/runners/test-monad-fork.py.
use std::{sync::Arc,collections::{BTreeMap,BTreeSet},time::Duration};
use alloy::primitives::{Address,B256,U256};
use barterlend_liquidator::{routes::*,economics::{NativeCostOracle,NativeCostQuality,RouteNativeCostOracle}};
use barterlend_state::{HttpReader,Commitment,StateReader,rpc::RpcConfig};
use serde::Deserialize;
#[derive(Deserialize)]struct Contract {address:Address,code_hash:B256}
#[derive(Deserialize)]struct Fixture {block:u64,hash:B256,weth:Address,ausd:Address,usdc:Address,v3_contracts:BTreeMap<String,Contract>,v4:V4Config}
#[tokio::test]
#[ignore="requires isolated loopback Monad fork; run tests/runners/test-monad-fork.py"]
async fn pinned_monad_v4_quotes_native_cost_and_identity_guards() {
    let url=std::env::var("BARTERLEND_MONAD_FORK_URL").expect("explicit local fork URL required");
    barterlend_liquidator::tx::rpc::validate_loopback_url(&url).unwrap();
    let fixture:Fixture=serde_json::from_str(include_str!("fixtures/monad-v4.json")).unwrap();
    let reader=Arc::new(HttpReader::new(&url,RpcConfig {min_request_interval:Duration::ZERO,max_retries:0,request_timeout:Duration::from_secs(60),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.chain_id,31337);
    let at=reader.head(Commitment::Latest).await.unwrap();assert_eq!(at.number,fixture.block);assert_eq!(at.hash,fixture.hash);
    let c=&fixture.v3_contracts;
    let config=V3Config {chain,factory:c["factory"].address,router:c["router"].address,quoter:c["quoter"].address,wrapped_native:c["wrapped_native"].address,
        code_hashes:V3CodeHashes {factory:c["factory"].code_hash,router:c["router"].code_hash,quoter:c["quoter"].code_hash,wrapped_native:c["wrapped_native"].code_hash},
        allowed_tokens:BTreeSet::from([fixture.weth,fixture.ausd,fixture.usdc,c["wrapped_native"].address]),intermediates:vec![fixture.usdc],fee_tiers:vec![100,500,3000],pool_code_hashes:BTreeMap::new(),limits:RouteLimits::default()};
    let provider=Arc::new(SwapRouteProvider::new(reader.clone(),config.clone()).unwrap().with_v4(Some(fixture.v4.clone())).unwrap());
    let route=provider.configured_routes(fixture.weth,fixture.ausd).unwrap().remove(0);
    assert!(matches!(route,RouteKind::V4 { .. }));
    let request=ExactInputRequest {token_in:fixture.weth,token_out:fixture.ausd,amount_in:U256::from(10u64.pow(16)),executor:Address::repeat_byte(42),at};
    let quote=provider.quote_exact_input_route(&request,&route).await.unwrap();
    quote.validate(request.token_in,request.token_out,request.amount_in,request.executor,&at).unwrap();
    assert_eq!(quote.amount_out,U256::from(23_174_418u64));assert_eq!(quote.minimum_amount_out,U256::from(22_942_673u64));
    let larger=provider.quote_exact_input_route(&ExactInputRequest {amount_in:U256::from(10u64.pow(18)),..request.clone()},&route).await.unwrap();
    assert_eq!(larger.amount_out,U256::from(2_316_862_273u64));
    let exact=ExactOutputRequest {token_in:fixture.weth,token_out:fixture.ausd,amount_out:U256::from(100_000_000),executor:request.executor,at};
    let reverse=provider.quote_exact_output_route(&exact,&route).await.unwrap();
    reverse.validate(exact.token_in,exact.token_out,exact.amount_out,exact.executor,&at).unwrap();
    let roundtrip=provider.quote_exact_input_route(&ExactInputRequest {amount_in:reverse.amount_in,..request.clone()},&route).await.unwrap();
    assert!(roundtrip.amount_out>=exact.amount_out && roundtrip.amount_out<=exact.amount_out+U256::from(2));
    let opposite=provider.configured_routes(fixture.ausd,fixture.weth).unwrap().remove(0);
    let quote_back=provider.quote_exact_input_route(&ExactInputRequest {token_in:fixture.ausd,token_out:fixture.weth,amount_in:U256::from(100_000_000),..request.clone()},&opposite).await.unwrap();
    assert!(quote_back.amount_out>U256::ZERO);
    let single=provider.configured_routes(fixture.weth,fixture.usdc).unwrap().remove(0);
    assert_eq!(single.exact_input_path().len(),47);
    provider.quote_exact_input_route(&ExactInputRequest {token_out:fixture.usdc,..request.clone()},&single).await.unwrap();
    let native=RouteNativeCostOracle::new(provider.clone(),request.executor,100).unwrap();
    let gas_route=provider.configured_routes(fixture.ausd,config.wrapped_native).unwrap().remove(0);
    native.select_route(fixture.ausd,&gas_route).await.unwrap();
    let cost=native.quote_cost(fixture.ausd,U256::from(100_000_000_000_000_000u64),&at).await.unwrap();
    assert!(matches!(cost.quality,NativeCostQuality::V3ExactOutput { .. }));assert!(cost.cost_asset>U256::ZERO);
    let mut bad=fixture.v4.clone();bad.manager_code_hash=B256::repeat_byte(1);
    let bad_provider=SwapRouteProvider::new(reader.clone(),config.clone()).unwrap().with_v4(Some(bad)).unwrap();
    assert!(matches!(bad_provider.quote_exact_input_route(&request,&route).await,Err(RouteError::Identity(_))));
    let mut tampered=quote.clone();if let RouteKind::V4 {pool_ids,..}=&mut tampered.route {pool_ids[0]=B256::repeat_byte(7);}
    tampered.fingerprint=tampered.compute_fingerprint();assert!(tampered.validate(request.token_in,request.token_out,request.amount_in,request.executor,&at).is_err());
    println!("V4 pinned WETH/AUSD: 0.01 WETH -> {}, 1 WETH -> {}; native bound 0.1 MON costs <= {} AUSD raw",quote.amount_out,larger.amount_out,cost.cost_asset);
}
