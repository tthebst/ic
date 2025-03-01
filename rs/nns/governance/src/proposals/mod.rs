use crate::pb::v1::{governance_error::ErrorType, GovernanceError, Topic};
use ic_base_types::CanisterId;
use ic_nns_constants::{
    BITCOIN_MAINNET_CANISTER_ID, BITCOIN_TESTNET_CANISTER_ID, CYCLES_LEDGER_CANISTER_ID,
    CYCLES_LEDGER_INDEX_CANISTER_ID, CYCLES_MINTING_CANISTER_ID, EXCHANGE_RATE_CANISTER_ID,
    GENESIS_TOKEN_CANISTER_ID, GOVERNANCE_CANISTER_ID, ICP_LEDGER_ARCHIVE_1_CANISTER_ID,
    ICP_LEDGER_ARCHIVE_CANISTER_ID, LEDGER_CANISTER_ID, LEDGER_INDEX_CANISTER_ID,
    LIFELINE_CANISTER_ID, REGISTRY_CANISTER_ID, ROOT_CANISTER_ID, SUBNET_RENTAL_CANISTER_ID,
};

pub mod call_canister;
pub mod create_service_nervous_system;
pub mod install_code;
pub mod proposal_submission;
pub mod stop_or_start_canister;

const PROTOCOL_CANISTER_IDS: [&CanisterId; 16] = [
    &REGISTRY_CANISTER_ID,
    &GOVERNANCE_CANISTER_ID,
    &LEDGER_CANISTER_ID,
    &ROOT_CANISTER_ID,
    &CYCLES_MINTING_CANISTER_ID,
    &LIFELINE_CANISTER_ID,
    &GENESIS_TOKEN_CANISTER_ID,
    &ICP_LEDGER_ARCHIVE_CANISTER_ID,
    &LEDGER_INDEX_CANISTER_ID,
    &ICP_LEDGER_ARCHIVE_1_CANISTER_ID,
    &SUBNET_RENTAL_CANISTER_ID,
    &EXCHANGE_RATE_CANISTER_ID,
    &BITCOIN_MAINNET_CANISTER_ID,
    &BITCOIN_TESTNET_CANISTER_ID,
    &CYCLES_LEDGER_CANISTER_ID,
    &CYCLES_LEDGER_INDEX_CANISTER_ID,
];

pub(crate) fn topic_to_manage_canister(canister_id: &CanisterId) -> Result<Topic, GovernanceError> {
    if PROTOCOL_CANISTER_IDS.contains(&canister_id) {
        Ok(Topic::ProtocolCanisterManagement)
    } else {
        Err(invalid_proposal_error(&format!(
            "Canister id {:?} is not a protocol canister",
            canister_id
        )))
    }
}

pub(crate) fn invalid_proposal_error(reason: &str) -> GovernanceError {
    GovernanceError::new_with_message(
        ErrorType::InvalidProposal,
        format!("Proposal invalid because of {}", reason),
    )
}
