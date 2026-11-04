use alloy::primitives::{address, b256, Address, B256};
use barterlend_liquidator::routes::{encode_v4_path, uniswap_v4::pool_ids, V4PoolKey, V4Config, SwapRouteProvider, RouteKind};
fn keys() -> Vec<V4PoolKey> { vec![
    V4PoolKey { currency0:address!("754704bc059f8c67012fed69bc8a327a5aafb603"),currency1:address!("ee8c0e9f1bffb4eb878d8f15f368a02a35481242"),fee:500,tick_spacing:10,hooks:Address::ZERO },
    V4PoolKey { currency0:address!("00000000efe302beaa2b3e6e1b18d08d69a9012a"),currency1:address!("754704bc059f8c67012fed69bc8a327a5aafb603"),fee:50,tick_spacing:1,hooks:Address::ZERO },
] }
#[test]
fn v4_pool_ids_and_path_match_real_monad_keys_and_executor_encoding() {
    let keys=keys();
    assert_eq!(keys[0].id().unwrap(),b256!("ad408916c1c310da9c258d4c128a7bf50fd9edc42a218cc970da39cfc8a05d93"));
    assert_eq!(keys[1].id().unwrap(),b256!("092b650478145f0aee73a1b400b342b9c6314db2e07aeb91faf7e75e8159ce72"));
    let tokens=vec![keys[0].currency1,keys[0].currency0,keys[1].currency0];
    let fees=vec![500,50];let tick_spacings=vec![10,1];
    let path=encode_v4_path(&tokens,&fees,&tick_spacings).unwrap();
    assert_eq!(path.len(),73);assert_eq!(path[0],4);
    assert_eq!(&path[1..21],tokens[0].as_slice());assert_eq!(&path[21..27],&[0,1,244,0,0,10]);
    assert_eq!(&path[27..47],tokens[1].as_slice());assert_eq!(&path[47..53],&[0,0,50,0,0,1]);
    assert_eq!(&path[53..73],tokens[2].as_slice());
    let route=RouteKind::V4 {path,tokens:tokens.clone(),fees,tick_spacings,pool_ids:pool_ids(&tokens,&[500,50],&[10,1]).unwrap()};
    let reversed=route.exact_output_path().unwrap();assert_eq!(&reversed[1..21],tokens[2].as_slice());
}
#[test]
fn v4_rejects_hooks_native_currency_dynamic_fee_wrong_spacing_and_duplicate_pools() {
    for field in 0..5 { let mut key=keys()[0].clone();match field {
        0=>key.hooks=Address::repeat_byte(1),1=>key.currency0=Address::ZERO,2=>key.fee=0x800000,
        3=>key.tick_spacing=0,4=>key.tick_spacing=32768,_=>unreachable!(),
    } assert!(key.id().is_err()); }
    let manifest:barterlend_liquidator::local_config::LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    let mut common=manifest.v3;let keys=keys();common.allowed_tokens.extend(keys.iter().flat_map(|key|[key.currency0,key.currency1]));
    common.intermediates=vec![keys[0].currency0];common.limits.max_candidates=1;
    let mut config=V4Config {manager:Address::repeat_byte(90),quoter:Address::repeat_byte(91),manager_code_hash:B256::repeat_byte(1),quoter_code_hash:B256::repeat_byte(2),pools:keys.clone()};
    config.validate(&common).unwrap();config.pools.push(keys[0].clone());assert!(config.validate(&common).is_err());config.pools.pop();
    let reader=std::sync::Arc::new(barterlend_state::HttpReader::new("http://127.0.0.1:1",Default::default()).unwrap());
    let provider=SwapRouteProvider::new(reader,common).unwrap().with_v4(Some(config)).unwrap();
    let routes=provider.configured_routes(keys[0].currency1,keys[1].currency0).unwrap();
    assert!(matches!(routes[0],RouteKind::V4 { .. }));assert!(routes.len()>1,"work budget never truncates full catalog");
    assert!(encode_v4_path(&[keys[0].currency0,keys[0].currency1],&[500],&[-10]).is_err());
}
