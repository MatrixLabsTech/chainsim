//! `send --to` is optional on the command line, because Solana's `--data`
//! form has no receiver. Every other family still needs it: an empty `--to`
//! is a usage error in each adapter's own parsing.

use chainsim_core::ErrorCode;

#[test]
fn an_empty_send_target_is_a_usage_error_on_the_other_families() {
    assert_eq!(chainsim_evm::units::parse_address("").unwrap_err().code, ErrorCode::Usage);
    assert_eq!(chainsim_sui::ptb::parse_target("").unwrap_err().code, ErrorCode::Usage);
    assert_eq!(chainsim_aptos::parse_function("").unwrap_err().code, ErrorCode::Usage);
}
