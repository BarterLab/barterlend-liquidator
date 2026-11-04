use alloy::primitives::{Address,U256,B256};
use barterlend_liquidator::routes::{ExactInputRequest,kyber::validate_build};
use barterlend_state::{BlockRef,Commitment};

#[test]
#[ignore="explicit captured API response from local fork runner; no network or signing"]
fn actual_api_build_matches_executor_constraints() {
    let path=std::env::var("BARTERLEND_KYBER_BUILD").expect("explicit external response file");
    let value:serde_json::Value=serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let address=|key:&str|value[key].as_str().unwrap().parse::<Address>().unwrap();
    let request=ExactInputRequest {token_in:address("input"),token_out:address("output"),executor:address("executor"),
        amount_in:U256::from_str_radix(value["amount"].as_str().unwrap(),10).unwrap(),
        at:BlockRef {number:1,hash:B256::repeat_byte(1),parent_hash:B256::ZERO,timestamp:1,commitment:Commitment::Latest}};
    validate_build(&value["build"],&request,address("router"),100).expect("real Kyber calldata violates ERC20 exact-input policy");
}

#[tokio::test]
#[ignore="explicit external config; mainnet read-only RPC and AMM quote/build, never signs"]
async fn rust_provider_builds_actual_monad_amm_route() {
    use std::{sync::Arc,time::Duration};
    use barterlend_state::{HttpReader,StateReader,rpc::RpcConfig};
    use barterlend_liquidator::routes::{kyber::{KyberConfig,KyberProvider},RouteLimits};
    let cfg:serde_json::Value=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_KYBER_READONLY_CONFIG").unwrap()).unwrap()).unwrap();
    let addr=|name:&str|cfg[name].as_str().unwrap().parse::<Address>().unwrap();
    let reader=Arc::new(HttpReader::new(cfg["rpc_url"].as_str().unwrap(),RpcConfig{min_request_interval:Duration::from_millis(67),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.chain_id,143);
    let at=reader.head(Commitment::Latest).await.unwrap();let router=addr("kyber_router");
    let pin=alloy::primitives::keccak256(reader.get_code(router,&at).await.unwrap());
    let provider=KyberProvider::new(reader,chain,KyberConfig {base_url:"https://aggregator-api.kyberswap.com".into(),router,router_code_hash:pin,
        client_id:"barterlend-liquidator".into(),api_key_env:None,requests_per_second:3,ttl_seconds:10},&RouteLimits::default()).unwrap();
    let request=ExactInputRequest{token_in:addr("input"),token_out:addr("output"),executor:addr("executor"),
        amount_in:U256::from_str_radix(cfg["amount"].as_str().unwrap(),10).unwrap(),at};
    let quote=provider.quote(&request).await.expect("actual Rust HTTP/validation pipeline must accept the AMM build");
    assert!(quote.amount_out>U256::ZERO && quote.minimum_amount_out>U256::ZERO);
    quote.validate(request.token_in,request.token_out,request.amount_in,request.executor,&at).unwrap();
}
