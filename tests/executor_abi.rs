use alloy::{primitives::keccak256,sol_types::{SolCall,SolEvent}};
use barterlend_liquidator::executor_abi::LiquidationExecutor;
use serde_json::Value;

fn abi_type(item:&Value)->String {
    let kind=item["type"].as_str().unwrap();
    if let Some(suffix)=kind.strip_prefix("tuple") {
        format!("({}){}",item["components"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>().join(","),suffix)
    } else {kind.to_string()}
}
#[test]
fn executor_request_return_and_event_match_compiler_generated_abi() {
    let abi:Value=serde_json::from_str(include_str!("../abi/LiquidationExecutor.json")).unwrap();
    let entries=abi.as_array().unwrap();
    let call=entries.iter().find(|e|e["type"]=="function"&&e["name"]=="execute").unwrap();
    let signature=format!("execute({})",call["inputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>().join(","));
    assert_eq!(signature,LiquidationExecutor::executeCall::SIGNATURE);
    assert_eq!(&keccak256(signature)[..4],LiquidationExecutor::executeCall::SELECTOR);
    assert_eq!(call["outputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>(),["uint128","uint128","uint256","uint256"]);
    let event=entries.iter().find(|e|e["type"]=="event"&&e["name"]=="LiquidationExecuted").unwrap();
    let signature=format!("LiquidationExecuted({})",event["inputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>().join(","));
    assert_eq!(keccak256(signature),LiquidationExecutor::LiquidationExecuted::SIGNATURE_HASH);
    assert_eq!(event["inputs"].as_array().unwrap().iter().map(|i|i["indexed"].as_bool().unwrap()).collect::<Vec<_>>(),[true,true,true,false,false,false,false,false]);
}

#[test]
fn factory_admission_views_and_constructor_match_compiler_abi() {
    let abi:Value=serde_json::from_str(include_str!("../abi/LiquidationExecutor.json")).unwrap();
    let entries=abi.as_array().unwrap();
    assert!(!entries.iter().any(|entry|matches!(entry["name"].as_str(),Some("setPool"|"approvedPools"|"PoolSet"|"version"))));
    let constructor=entries.iter().find(|entry|entry["type"]=="constructor").unwrap();
    assert_eq!(constructor["inputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>(),
        ["address","address","address","address","address[]"]);
    for (signature,selector,outputs) in [
        (LiquidationExecutor::poolConfigCall::SIGNATURE,LiquidationExecutor::poolConfigCall::SELECTOR,vec!["address","address","bool"]),
        (LiquidationExecutor::isPoolCall::SIGNATURE,LiquidationExecutor::isPoolCall::SELECTOR,vec!["bool"]),
        (LiquidationExecutor::verifiedTokensCall::SIGNATURE,LiquidationExecutor::verifiedTokensCall::SELECTOR,vec!["bool"]),
        (LiquidationExecutor::lendingFactoriesCall::SIGNATURE,LiquidationExecutor::lendingFactoriesCall::SELECTOR,vec!["address[]"]),
        (LiquidationExecutor::lendingFactoryCodeHashesCall::SIGNATURE,LiquidationExecutor::lendingFactoryCodeHashesCall::SELECTOR,vec!["bytes32"]),
    ] {
        let entry=entries.iter().find(|entry|entry["type"]=="function" && signature.starts_with(&format!("{}(",entry["name"].as_str().unwrap()))).unwrap();
        let actual=format!("{}({})",entry["name"].as_str().unwrap(),entry["inputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>().join(","));
        assert_eq!(signature,actual);assert_eq!(&keccak256(actual)[..4],selector);
        assert_eq!(entry["outputs"].as_array().unwrap().iter().map(abi_type).collect::<Vec<_>>(),outputs);
    }
}
