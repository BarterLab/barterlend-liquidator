//! Canonical ABI decoding at RPC boundaries; validated padding alone is not enough.
use alloy::sol_types::SolCall;
use barterlend_state::{Error, Result};

pub(crate) fn decode_return<C: SolCall>(bytes: &[u8]) -> Result<C::Return> {
    let value = C::abi_decode_returns_validate(bytes)
        .map_err(|error| Error::Decode(format!("{} return: {error}", C::SIGNATURE)))?;
    if C::abi_encode_returns(&value).as_slice() != bytes {
        return Err(Error::Decode(format!("{} noncanonical return", C::SIGNATURE)));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor_abi::LiquidationExecutor;
    use alloy::{primitives::U256, sol_types::SolValue};

    #[test]
    fn exact_simulation_returns_reject_padding_trailing_and_truncated_data() {
        let canonical = (80u128, 100u128, U256::from(100), U256::from(8)).abi_encode();
        assert!(decode_return::<LiquidationExecutor::executeCall>(&canonical).is_ok());
        let mut dirty = canonical.clone(); dirty[0] = 1;
        let mut extra = canonical.clone(); extra.extend_from_slice(&[0; 32]);
        for malformed in [dirty, extra, canonical[..127].to_vec()] {
            assert!(decode_return::<LiquidationExecutor::executeCall>(&malformed).is_err());
        }
    }
}
