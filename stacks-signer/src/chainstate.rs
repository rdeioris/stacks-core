// Copyright (C) 2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::time::{Duration, UNIX_EPOCH};

use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::stacks::TenureChangePayload;
use blockstack_lib::net::api::getsortition::SortitionInfo;
use blockstack_lib::util_lib::db::Error as DBError;
use clarity::types::chainstate::BurnchainHeaderHash;
use slog::{slog_info, slog_warn};
use stacks_common::types::chainstate::{ConsensusHash, StacksPublicKey};
use stacks_common::util::hash::Hash160;
use stacks_common::{info, warn};

use crate::client::{ClientError, CurrentAndLastSortition, StacksClient};
use crate::config::SignerConfig;
use crate::signerdb::{BlockState, SignerDb};

#[derive(thiserror::Error, Debug)]
/// Error type for the signer chainstate module
pub enum SignerChainstateError {
    /// Error resulting from database interactions
    #[error("Database error: {0}")]
    DBError(#[from] DBError),
    /// Error resulting from crate::client interactions
    #[error("Client error: {0}")]
    ClientError(#[from] ClientError),
}

/// Captures this signer's current view of a sortition's miner.
#[derive(PartialEq, Eq, Debug)]
pub enum SortitionMinerStatus {
    /// The signer thinks this sortition's miner is invalid, and hasn't signed any blocks for them.
    InvalidatedBeforeFirstBlock,
    /// The signer thinks this sortition's miner is invalid, but already signed one or more blocks for them.
    InvalidatedAfterFirstBlock,
    /// The signer thinks this sortition's miner is valid
    Valid,
}

/// Captures the Stacks sortition related state for
///  a successful sortition.
///
/// Sortition state in this struct is
///  is indexed using consensus hashes, and fetched from a single "get latest" RPC call
///  to the stacks node. This ensures that the state in this struct is consistent with itself
///  (i.e., it does not span a bitcoin fork) and up to date.
#[derive(Debug)]
pub struct SortitionState {
    /// The miner's pub key hash
    pub miner_pkh: Hash160,
    /// If known already, the public key which hashes to `miner_pkh`
    pub miner_pubkey: Option<StacksPublicKey>,
    /// the last burn block in this fork which had a sortition
    pub prior_sortition: ConsensusHash,
    /// the committed to parent tenure ID
    pub parent_tenure_id: ConsensusHash,
    /// this sortition's consensus hash
    pub consensus_hash: ConsensusHash,
    /// what is this signer's view of the this sortition's miner? did they misbehave?
    pub miner_status: SortitionMinerStatus,
    /// the timestamp in the burn block that performed this sortition
    pub burn_header_timestamp: u64,
    /// the burn header hash of the burn block that performed this sortition
    pub burn_block_hash: BurnchainHeaderHash,
}

impl SortitionState {
    /// Check if the sortition is timed out (i.e., the miner did not propose a block in time)
    pub fn is_timed_out(
        &self,
        timeout: Duration,
        signer_db: &SignerDb,
    ) -> Result<bool, SignerChainstateError> {
        // if the miner has already been invalidated, we don't need to check if they've timed out.
        if self.miner_status != SortitionMinerStatus::Valid {
            return Ok(false);
        }
        // if we've already signed a block in this tenure, the miner can't have timed out.
        let has_blocks = signer_db
            .get_last_signed_block_in_tenure(&self.consensus_hash)?
            .is_some();
        if has_blocks {
            return Ok(false);
        }
        let Some(received_ts) = signer_db.get_burn_block_receive_time(&self.burn_block_hash)?
        else {
            return Ok(false);
        };
        let received_time = UNIX_EPOCH + Duration::from_secs(received_ts);
        let Ok(elapsed) = std::time::SystemTime::now().duration_since(received_time) else {
            return Ok(false);
        };
        if elapsed > timeout {
            return Ok(true);
        }
        Ok(false)
    }
}

/// Captures the configuration settings used by the signer when evaluating block proposals.
#[derive(Debug, Clone)]
pub struct ProposalEvalConfig {
    /// How much time must pass between the first block proposal in a tenure and the next bitcoin block
    ///  before a subsequent miner isn't allowed to reorg the tenure
    pub first_proposal_burn_block_timing: Duration,
    /// Time between processing a sortition and proposing a block before the block is considered invalid
    pub block_proposal_timeout: Duration,
}

impl From<&SignerConfig> for ProposalEvalConfig {
    fn from(value: &SignerConfig) -> Self {
        Self {
            first_proposal_burn_block_timing: value.first_proposal_burn_block_timing,
            block_proposal_timeout: value.block_proposal_timeout,
        }
    }
}

/// The signer's current view of the stacks chain's sortition
///  state
#[derive(Debug)]
pub struct SortitionsView {
    /// the prior successful sortition (this corresponds to the "prior" miner slot)
    pub last_sortition: Option<SortitionState>,
    /// the current successful sortition (this corresponds to the "current" miner slot)
    pub cur_sortition: SortitionState,
    /// configuration settings for evaluating proposals
    pub config: ProposalEvalConfig,
}

impl TryFrom<SortitionInfo> for SortitionState {
    type Error = ClientError;
    fn try_from(value: SortitionInfo) -> Result<Self, Self::Error> {
        Ok(Self {
            miner_pkh: value
                .miner_pk_hash160
                .ok_or_else(|| ClientError::UnexpectedSortitionInfo)?,
            miner_pubkey: None,
            prior_sortition: value
                .last_sortition_ch
                .ok_or_else(|| ClientError::UnexpectedSortitionInfo)?,
            consensus_hash: value.consensus_hash,
            parent_tenure_id: value
                .stacks_parent_ch
                .ok_or_else(|| ClientError::UnexpectedSortitionInfo)?,
            burn_header_timestamp: value.burn_header_timestamp,
            burn_block_hash: value.burn_block_hash,
            miner_status: SortitionMinerStatus::Valid,
        })
    }
}

enum ProposedBy<'a> {
    LastSortition(&'a SortitionState),
    CurrentSortition(&'a SortitionState),
}

impl<'a> ProposedBy<'a> {
    pub fn state(&self) -> &SortitionState {
        match self {
            ProposedBy::LastSortition(x) => x,
            ProposedBy::CurrentSortition(x) => x,
        }
    }
}

impl SortitionsView {
    /// Apply checks from the SortitionsView on the block proposal.
    pub fn check_proposal(
        &mut self,
        client: &StacksClient,
        signer_db: &mut SignerDb,
        block: &NakamotoBlock,
        block_pk: &StacksPublicKey,
        reward_cycle: u64,
        reset_view_if_wrong_consensus_hash: bool,
    ) -> Result<bool, SignerChainstateError> {
        if self
            .cur_sortition
            .is_timed_out(self.config.block_proposal_timeout, signer_db)?
        {
            info!(
                "Current miner timed out, marking as invalid.";
                "block_height" => block.header.chain_length,
                "current_sortition_consensus_hash" => ?self.cur_sortition.consensus_hash,
            );
            self.cur_sortition.miner_status = SortitionMinerStatus::InvalidatedBeforeFirstBlock;
        }
        if let Some(last_sortition) = self.last_sortition.as_mut() {
            if last_sortition.is_timed_out(self.config.block_proposal_timeout, signer_db)? {
                info!(
                    "Last miner timed out, marking as invalid.";
                    "block_height" => block.header.chain_length,
                    "last_sortition_consensus_hash" => ?last_sortition.consensus_hash,
                );
                last_sortition.miner_status = SortitionMinerStatus::InvalidatedBeforeFirstBlock;
            }
        }
        let bitvec_all_1s = block.header.pox_treatment.iter().all(|entry| entry);
        if !bitvec_all_1s {
            warn!(
                "Miner block proposal has bitvec field which punishes in disagreement with signer. Considering invalid.";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "current_sortition_consensus_hash" => ?self.cur_sortition.consensus_hash,
                "last_sortition_consensus_hash" => ?self.last_sortition.as_ref().map(|x| x.consensus_hash),
            );
            return Ok(false);
        }

        let block_pkh = Hash160::from_data(&block_pk.to_bytes_compressed());
        let Some(proposed_by) =
            (if block.header.consensus_hash == self.cur_sortition.consensus_hash {
                Some(ProposedBy::CurrentSortition(&self.cur_sortition))
            } else {
                None
            })
            .or_else(|| {
                self.last_sortition.as_ref().and_then(|last_sortition| {
                    if block.header.consensus_hash == last_sortition.consensus_hash {
                        Some(ProposedBy::LastSortition(last_sortition))
                    } else {
                        None
                    }
                })
            })
        else {
            if reset_view_if_wrong_consensus_hash {
                info!(
                    "Miner block proposal has consensus hash that is neither the current or last sortition. Resetting view.";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "current_sortition_consensus_hash" => ?self.cur_sortition.consensus_hash,
                    "last_sortition_consensus_hash" => ?self.last_sortition.as_ref().map(|x| x.consensus_hash),
                );
                self.reset_view(client)?;
                return self.check_proposal(
                    client,
                    signer_db,
                    block,
                    block_pk,
                    reward_cycle,
                    false,
                );
            }
            warn!(
                "Miner block proposal has consensus hash that is neither the current or last sortition. Considering invalid.";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "current_sortition_consensus_hash" => ?self.cur_sortition.consensus_hash,
                "last_sortition_consensus_hash" => ?self.last_sortition.as_ref().map(|x| x.consensus_hash),
            );
            return Ok(false);
        };

        if proposed_by.state().miner_pkh != block_pkh {
            warn!(
                "Miner block proposal pubkey does not match the winning pubkey hash for its sortition. Considering invalid.";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "proposed_block_pubkey" => &block_pk.to_hex(),
                "proposed_block_pubkey_hash" => %block_pkh,
                "sortition_winner_pubkey_hash" => %proposed_by.state().miner_pkh,
            );
            return Ok(false);
        }

        // check that this miner is the most recent sortition
        match proposed_by {
            ProposedBy::CurrentSortition(sortition) => {
                if sortition.miner_status != SortitionMinerStatus::Valid {
                    warn!(
                        "Current miner behaved improperly, this signer views the miner as invalid.";
                        "proposed_block_consensus_hash" => %block.header.consensus_hash,
                        "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                    );
                    return Ok(false);
                }
            }
            ProposedBy::LastSortition(_last_sortition) => {
                // should only consider blocks from the last sortition if the new sortition was invalidated
                //  before we signed their first block.
                if self.cur_sortition.miner_status
                    != SortitionMinerStatus::InvalidatedBeforeFirstBlock
                {
                    warn!(
                        "Miner block proposal is from last sortition winner, when the new sortition winner is still valid. Considering proposal invalid.";
                        "proposed_block_consensus_hash" => %block.header.consensus_hash,
                        "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                    );
                    return Ok(false);
                }
            }
        };

        if let Some(tenure_change) = block.get_tenure_change_tx_payload() {
            if !self.validate_tenure_change_payload(
                &proposed_by,
                tenure_change,
                block,
                reward_cycle,
                signer_db,
                client,
            )? {
                return Ok(false);
            }
        } else {
            // check if the new block confirms the last block in the current tenure
            let confirms_latest_in_tenure =
                Self::confirms_latest_block_in_same_tenure(block, signer_db)?;
            if !confirms_latest_in_tenure {
                return Ok(false);
            }
        }

        if let Some(tenure_extend) = block.get_tenure_extend_tx_payload() {
            // in tenure extends, we need to check:
            // (1) if this is the most recent sortition, an extend is allowed if it changes the burnchain view
            // (2) if this is the most recent sortition, an extend is allowed if enough time has passed to refresh the block limit
            let changed_burn_view =
                tenure_extend.burn_view_consensus_hash != proposed_by.state().consensus_hash;
            let enough_time_passed = Self::tenure_time_passed_block_lim()?;
            if !changed_burn_view && !enough_time_passed {
                warn!(
                    "Miner block proposal contains a tenure extend, but the burnchain view has not changed and enough time has not passed to refresh the block limit. Considering proposal invalid.";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                );
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn check_parent_tenure_choice(
        sortition_state: &SortitionState,
        block: &NakamotoBlock,
        signer_db: &SignerDb,
        client: &StacksClient,
        first_proposal_burn_block_timing: &Duration,
    ) -> Result<bool, SignerChainstateError> {
        // if the parent tenure is the last sortition, it is a valid choice.
        // if the parent tenure is a reorg, then all of the reorged sortitions
        //  must either have produced zero blocks _or_ produced their first block
        //  very close to the burn block transition.
        if sortition_state.prior_sortition == sortition_state.parent_tenure_id {
            return Ok(true);
        }
        info!(
            "Most recent miner's tenure does not build off the prior sortition, checking if this is valid behavior";
            "proposed_block_consensus_hash" => %block.header.consensus_hash,
            "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
            "sortition_state.consensus_hash" => %sortition_state.consensus_hash,
            "sortition_state.prior_sortition" => %sortition_state.prior_sortition,
            "sortition_state.parent_tenure_id" => %sortition_state.parent_tenure_id,
            "block_height" => block.header.chain_length,
        );

        let tenures_reorged = client.get_tenure_forking_info(
            &sortition_state.parent_tenure_id,
            &sortition_state.prior_sortition,
        )?;
        if tenures_reorged.is_empty() {
            warn!("Miner is not building off of most recent tenure, but stacks node was unable to return information about the relevant sortitions. Marking miner invalid.";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
            );
            return Ok(false);
        }

        // this value *should* always be some, but try to do the best we can if it isn't
        let sortition_state_received_time =
            signer_db.get_burn_block_receive_time(&sortition_state.burn_block_hash)?;

        for tenure in tenures_reorged.iter() {
            if tenure.consensus_hash == sortition_state.parent_tenure_id {
                // this was a built-upon tenure, no need to check this tenure as part of the reorg.
                continue;
            }

            if tenure.first_block_mined.is_some() {
                let Some(local_block_info) =
                    signer_db.get_first_signed_block_in_tenure(&tenure.consensus_hash)?
                else {
                    warn!(
                        "Miner is not building off of most recent tenure, but a tenure they attempted to reorg has already mined blocks, and there is no local knowledge for that tenure's block timing.";
                        "proposed_block_consensus_hash" => %block.header.consensus_hash,
                        "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                        "parent_tenure" => %sortition_state.parent_tenure_id,
                        "last_sortition" => %sortition_state.prior_sortition,
                        "violating_tenure_id" => %tenure.consensus_hash,
                        "violating_tenure_first_block_id" => ?tenure.first_block_mined,
                    );
                    return Ok(false);
                };

                let checked_proposal_timing = if let Some(sortition_state_received_time) =
                    sortition_state_received_time
                {
                    // how long was there between when the proposal was received and the next sortition started?
                    let proposal_to_sortition = if let Some(signed_at) =
                        local_block_info.signed_self
                    {
                        sortition_state_received_time.saturating_sub(signed_at)
                    } else {
                        info!("We did not sign over the reorged tenure's first block, considering it as a late-arriving proposal");
                        0
                    };
                    if Duration::from_secs(proposal_to_sortition)
                        <= *first_proposal_burn_block_timing
                    {
                        info!(
                            "Miner is not building off of most recent tenure. A tenure they reorg has already mined blocks, but the block was poorly timed, allowing the reorg.";
                            "proposed_block_consensus_hash" => %block.header.consensus_hash,
                            "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                            "proposed_block_height" => block.header.chain_length,
                            "parent_tenure" => %sortition_state.parent_tenure_id,
                            "last_sortition" => %sortition_state.prior_sortition,
                            "violating_tenure_id" => %tenure.consensus_hash,
                            "violating_tenure_first_block_id" => ?tenure.first_block_mined,
                            "violating_tenure_proposed_time" => local_block_info.proposed_time,
                            "new_tenure_received_time" => sortition_state_received_time,
                            "new_tenure_burn_timestamp" => sortition_state.burn_header_timestamp,
                        );
                        continue;
                    }
                    true
                } else {
                    false
                };

                warn!(
                    "Miner is not building off of most recent tenure, but a tenure they attempted to reorg has already mined blocks.";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                    "parent_tenure" => %sortition_state.parent_tenure_id,
                    "last_sortition" => %sortition_state.prior_sortition,
                    "violating_tenure_id" => %tenure.consensus_hash,
                    "violating_tenure_first_block_id" => ?tenure.first_block_mined,
                    "checked_proposal_timing" => checked_proposal_timing,
                );
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Check if the tenure change block confirms the expected parent block (i.e., the last globally accepted block in the parent tenure)
    /// It checks the local DB first, and if the block is not present in the local DB, it asks the
    /// Stacks node for the highest processed block header in the given tenure (and then caches it
    /// in the DB).
    ///
    /// The rationale here is that the signer DB can be out-of-sync with the node.  For example,
    /// the signer may have been added to an already-running node.
    fn check_tenure_change_confirms_parent(
        tenure_change: &TenureChangePayload,
        block: &NakamotoBlock,
        reward_cycle: u64,
        signer_db: &mut SignerDb,
        client: &StacksClient,
    ) -> Result<bool, ClientError> {
        // If the tenure change block confirms the expected parent block, it should confirm at least one more block than the last globally accepted block in the parent tenure.
        let last_globally_accepted_block = signer_db
            .get_last_globally_accepted_block(&tenure_change.prev_tenure_consensus_hash)
            .map_err(|e| ClientError::InvalidResponse(e.to_string()))?;

        if let Some(global_info) = last_globally_accepted_block {
            // N.B. this block might not be the last globally accepted block across the network;
            // it's just the highest one in this tenure that we know about.  If this given block is
            // no higher than it, then it's definitely no higher than the last globally accepted
            // block across the network, so we can do an early rejection here.
            if block.header.chain_length <= global_info.block.header.chain_length {
                warn!(
                    "Miner's block proposal does not confirm as many blocks as we expect";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                    "proposed_chain_length" => block.header.chain_length,
                    "expected_at_least" => global_info.block.header.chain_length + 1,
                );
                return Ok(false);
            }
        }

        let tip = match client.get_tenure_tip(&tenure_change.prev_tenure_consensus_hash) {
            Ok(tip) => tip,
            Err(e) => {
                warn!(
                    "Miner block proposal contains a tenure change, but failed to fetch the tenure tip for the parent tenure: {e:?}. Considering proposal invalid.";
                    "proposed_block_consensus_hash" => %block.header.consensus_hash,
                    "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                    "parent_tenure" => %tenure_change.prev_tenure_consensus_hash,
                );
                return Ok(false);
            }
        };
        if let Some(nakamoto_tip) = tip.as_stacks_nakamoto() {
            // If we have seen this block already, make sure its state is updated to globally accepted.
            // Otherwise, don't worry about it.
            if let Ok(Some(mut block_info)) =
                signer_db.block_lookup(reward_cycle, &nakamoto_tip.signer_signature_hash())
            {
                if block_info.state != BlockState::GloballyAccepted {
                    if let Err(e) = block_info.mark_globally_accepted() {
                        warn!("Failed to update block info in db: {e}");
                    } else if let Err(e) = signer_db.insert_block(&block_info) {
                        warn!("Failed to update block info in db: {e}");
                    }
                }
            }
        }
        let tip_height = tip.height();
        if block.header.chain_length > tip_height {
            Ok(true)
        } else {
            warn!(
                "Miner's block proposal does not confirm as many blocks as we expect";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "proposed_chain_length" => block.header.chain_length,
                "expected_at_least" => tip_height + 1,
            );
            Ok(false)
        }
    }

    /// in tenure changes, we need to check:
    /// (1) if the tenure change confirms the expected parent block (i.e.,
    /// the last globally accepted block in the parent tenure)
    /// (2) if the parent tenure was a valid choice
    fn validate_tenure_change_payload(
        &self,
        proposed_by: &ProposedBy,
        tenure_change: &TenureChangePayload,
        block: &NakamotoBlock,
        reward_cycle: u64,
        signer_db: &mut SignerDb,
        client: &StacksClient,
    ) -> Result<bool, SignerChainstateError> {
        // Ensure that the tenure change block confirms the expected parent block
        let confirms_expected_parent = Self::check_tenure_change_confirms_parent(
            tenure_change,
            block,
            reward_cycle,
            signer_db,
            client,
        )?;
        if !confirms_expected_parent {
            return Ok(false);
        }
        // now, we have to check if the parent tenure was a valid choice.
        let is_valid_parent_tenure = Self::check_parent_tenure_choice(
            proposed_by.state(),
            block,
            signer_db,
            client,
            &self.config.first_proposal_burn_block_timing,
        )?;
        if !is_valid_parent_tenure {
            return Ok(false);
        }
        let last_in_tenure = signer_db
            .get_last_globally_accepted_block(&block.header.consensus_hash)
            .map_err(|e| ClientError::InvalidResponse(e.to_string()))?;
        if let Some(last_in_tenure) = last_in_tenure {
            warn!(
                "Miner block proposal contains a tenure change, but we've already signed a block in this tenure. Considering proposal invalid.";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "last_in_tenure_signer_sighash" => %last_in_tenure.block.header.signer_signature_hash(),
            );
            return Ok(false);
        }
        Ok(true)
    }

    fn confirms_latest_block_in_same_tenure(
        block: &NakamotoBlock,
        signer_db: &SignerDb,
    ) -> Result<bool, ClientError> {
        let Some(last_known_block) = signer_db
            .get_last_accepted_block(&block.header.consensus_hash)
            .map_err(|e| ClientError::InvalidResponse(e.to_string()))?
        else {
            info!(
                "Have no accepted blocks in the tenure, assuming block confirmation is correct";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "proposed_block_height" => block.header.chain_length,
            );
            return Ok(true);
        };
        if block.header.chain_length > last_known_block.block.header.chain_length {
            Ok(true)
        } else {
            warn!(
                "Miner's block proposal does not confirm as many blocks as we expect";
                "proposed_block_consensus_hash" => %block.header.consensus_hash,
                "proposed_block_signer_sighash" => %block.header.signer_signature_hash(),
                "proposed_chain_length" => block.header.chain_length,
                "expected_at_least" => last_known_block.block.header.chain_length + 1,
            );
            Ok(false)
        }
    }

    /// Has the current tenure lasted long enough to extend the block limit?
    pub fn tenure_time_passed_block_lim() -> Result<bool, ClientError> {
        // TODO
        Ok(false)
    }

    /// Fetch a new view of the recent sortitions
    pub fn fetch_view(
        config: ProposalEvalConfig,
        client: &StacksClient,
    ) -> Result<Self, ClientError> {
        let CurrentAndLastSortition {
            current_sortition,
            last_sortition,
        } = client.get_current_and_last_sortition()?;

        let cur_sortition = SortitionState::try_from(current_sortition)?;
        let last_sortition = last_sortition
            .map(SortitionState::try_from)
            .transpose()
            .ok()
            .flatten();

        Ok(Self {
            cur_sortition,
            last_sortition,
            config,
        })
    }

    /// Reset the view to the current sortition and last sortition
    pub fn reset_view(&mut self, client: &StacksClient) -> Result<(), ClientError> {
        let CurrentAndLastSortition {
            current_sortition,
            last_sortition,
        } = client.get_current_and_last_sortition()?;

        let cur_sortition = SortitionState::try_from(current_sortition)?;
        let last_sortition = last_sortition
            .map(SortitionState::try_from)
            .transpose()
            .ok()
            .flatten();

        self.cur_sortition = cur_sortition;
        self.last_sortition = last_sortition;
        Ok(())
    }
}