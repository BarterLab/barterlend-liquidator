//! Adding a quote source does not require changes to the scheduler or signer.
use std::sync::Arc;
use async_trait::async_trait;
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct Capabilities { pub exact_input: bool, pub exact_output: bool }

#[async_trait]
pub trait LiquidityProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Cheap hints, not executable quotes. Return an empty list for unsupported pairs.
    fn candidates(&self, input: Address, output: Address) -> RouteResult<Vec<RouteKind>>;
    async fn quote(&self, request: &ExactInputRequest, hint: &RouteKind) -> RouteResult<RouteQuote>;
}

#[async_trait]
impl LiquidityProvider for SwapRouteProvider {
    fn name(&self) -> &'static str { "uniswap" }
    fn capabilities(&self) -> Capabilities { Capabilities { exact_input: true, exact_output: true } }
    fn candidates(&self, input: Address, output: Address) -> RouteResult<Vec<RouteKind>> { self.configured_routes(input, output) }
    async fn quote(&self, request: &ExactInputRequest, hint: &RouteKind) -> RouteResult<RouteQuote> {
        self.quote_exact_input_route(request, hint).await
    }
}

#[async_trait]
impl LiquidityProvider for super::kyber::KyberProvider {
    fn name(&self) -> &'static str { "kyber" }
    fn capabilities(&self) -> Capabilities { Capabilities { exact_input: true, exact_output: false } }
    fn candidates(&self, input: Address, output: Address) -> RouteResult<Vec<RouteKind>> {
        Ok(if input == output { Vec::new() } else { vec![RouteKind::Kyber { path: Bytes::new() }] })
    }
    async fn quote(&self, request: &ExactInputRequest, _: &RouteKind) -> RouteResult<RouteQuote> { self.quote(request).await }
}

#[derive(Clone, Debug)]
pub struct RouteCandidate { pub provider: usize, pub hint: RouteKind }
pub struct ProviderRegistry { providers: Vec<Arc<dyn LiquidityProvider>> }
impl ProviderRegistry {
    pub fn new(providers: Vec<Arc<dyn LiquidityProvider>>) -> RouteResult<Self> {
        if providers.is_empty() || providers.len() > 8 || providers.iter().any(|p| !p.capabilities().exact_input) {
            return Err(RouteError::InvalidConfig("1..8 exact-input providers required".into()));
        }
        Ok(Self { providers })
    }
    pub fn candidates(&self, input: Address, output: Address) -> RouteResult<Vec<RouteCandidate>> {
        let lists = self.providers.iter().map(|p| p.candidates(input, output).unwrap_or_else(|_| {
            tracing::debug!(provider=p.name(), "provider candidate discovery unavailable"); Vec::new()
        })).collect::<Vec<_>>();
        let mut result = Vec::new();
        // One provider with a large path catalog cannot starve another provider.
        for index in 0..lists.iter().map(Vec::len).max().unwrap_or(0).min(256) {
            for (provider, list) in lists.iter().enumerate() {
                if let Some(hint) = list.get(index) { result.push(RouteCandidate { provider, hint: hint.clone() }); }
            }
        }
        Ok(result)
    }
    /// Sample each provider/execution family before taking additional paths.
    /// A budget too small to cover the enabled sources cannot authorize a winner.
    pub fn comparison_candidates(&self, input: Address, output: Address, round: u64, budget: usize) -> RouteResult<Vec<RouteCandidate>> {
        let mut groups=std::collections::BTreeMap::<_,Vec<_>>::new();
        for candidate in self.candidates(input,output)? {
            groups.entry((candidate.provider,candidate.hint.source())).or_default().push(candidate);
        }
        let budget=budget.min(8);
        if budget<groups.len() { return Err(RouteError::InvalidConfig("comparison budget cannot cover all sources".into())); }
        let mut selected=Vec::new();
        for index in 0..budget {
            for group in groups.values() {
                if index<group.len() && selected.len()<budget {
                    selected.push(group[((round%group.len() as u64) as usize+index)%group.len()].clone());
                }
            }
        }
        Ok(selected)
    }
    pub async fn quote(&self, request: &ExactInputRequest, hint: &RouteCandidate) -> RouteResult<RouteQuote> {
        let provider = self.providers.get(hint.provider).ok_or_else(|| RouteError::InvalidConfig("unknown quote provider".into()))?;
        let quote = provider.quote(request, &hint.hint).await?;
        quote.validate(request.token_in, request.token_out, request.amount_in, request.executor, &request.at)?;
        Ok(quote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Broken;
    struct Working;
    #[async_trait]
    impl LiquidityProvider for Broken {
        fn name(&self)->&'static str {"broken"}
        fn capabilities(&self)->Capabilities {Capabilities {exact_input:true,exact_output:false}}
        fn candidates(&self,_:Address,_:Address)->RouteResult<Vec<RouteKind>> {Err(RouteError::Unavailable("offline".into()))}
        async fn quote(&self,_:&ExactInputRequest,_:&RouteKind)->RouteResult<RouteQuote> {unreachable!()}
    }
    #[async_trait]
    impl LiquidityProvider for Working {
        fn name(&self)->&'static str {"working"}
        fn capabilities(&self)->Capabilities {Capabilities {exact_input:true,exact_output:true}}
        fn candidates(&self,_:Address,_:Address)->RouteResult<Vec<RouteKind>> {Ok(vec![RouteKind::SameToken])}
        async fn quote(&self,_:&ExactInputRequest,_:&RouteKind)->RouteResult<RouteQuote> {unreachable!()}
    }
    #[test]
    fn unavailable_provider_does_not_mask_other_sources() {
        let registry=ProviderRegistry::new(vec![Arc::new(Broken),Arc::new(Working)]).unwrap();
        let hints=registry.candidates(Address::repeat_byte(1),Address::repeat_byte(1)).unwrap();
        assert_eq!(hints.len(),1);assert_eq!(hints[0].provider,1);
    }
    struct Catalog(Vec<RouteKind>);
    #[async_trait]
    impl LiquidityProvider for Catalog {
        fn name(&self)->&'static str {"catalog"}
        fn capabilities(&self)->Capabilities {Capabilities {exact_input:true,exact_output:false}}
        fn candidates(&self,_:Address,_:Address)->RouteResult<Vec<RouteKind>> {Ok(self.0.clone())}
        async fn quote(&self,_:&ExactInputRequest,_:&RouteKind)->RouteResult<RouteQuote> {unreachable!()}
    }
    #[test]
    fn comparison_reserves_a_slot_for_v3_v4_and_aggregator_and_rotates_paths() {
        // Hints alone cannot authorize execution; quote validation happens later.
        let v3=|fee|RouteKind::V3 {path:Bytes::new(),tokens:Vec::new(),fees:vec![fee],pools:Vec::new()};
        let v4=RouteKind::V4 {path:Bytes::new(),tokens:Vec::new(),fees:Vec::new(),tick_spacings:Vec::new(),pool_ids:Vec::new()};
        let mut catalog=vec![v4;12];catalog.extend([v3(500),v3(3000)]);
        let registry=ProviderRegistry::new(vec![Arc::new(Catalog(catalog)),Arc::new(Catalog(vec![RouteKind::Kyber {path:Bytes::new()}]))]).unwrap();
        let a=Address::repeat_byte(1);let b=Address::repeat_byte(2);
        let first=registry.comparison_candidates(a,b,0,3).unwrap();
        let next=registry.comparison_candidates(a,b,1,3).unwrap();
        assert_eq!(first.iter().map(|h|h.hint.source()).collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["uniswap_v3","uniswap_v4","kyber"]));
        assert_ne!(first[0].hint,next[0].hint);
        assert!(registry.comparison_candidates(a,b,0,2).is_err());
        assert_eq!(registry.comparison_candidates(a,b,0,256).unwrap().len(),8);
    }

}
