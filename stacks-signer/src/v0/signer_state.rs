// Copyright (C) 2025 Stacks Open Internet Foundation
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

use blockstack_lib::chainstate::burn::ConsensusHashExtensions;
use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader};
use libsigner::v0::messages::{
    MessageSlotID, SignerMessage, StateMachineUpdate as StateMachineUpdateMessage,
    StateMachineUpdateContent, StateMachineUpdateMinerState,
};
use serde::{Deserialize, Serialize};
use stacks_common::bitvec::BitVec;
use stacks_common::codec::Error as CodecError;
use stacks_common::types::chainstate::{ConsensusHash, StacksBlockId, TrieHash};
use stacks_common::util::hash::{Hash160, Sha512Trunc256Sum};
use stacks_common::util::secp256k1::MessageSignature;
use stacks_common::{info, warn};

use crate::chainstate::{
    ProposalEvalConfig, SignerChainstateError, SortitionState, SortitionsView,
};
use crate::client::{ClientError, CurrentAndLastSortition, StackerDB, StacksClient};
use crate::signerdb::SignerDb;

/// This is the latest supported protocol version for this signer binary
pub static SUPPORTED_SIGNER_PROTOCOL_VERSION: u64 = 0;

/// A signer state machine view. This struct can
///  be used to encode the local signer's view or
///  the global view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignerStateMachine {
    /// The tip burn block (i.e., the latest bitcoin block) seen by this signer
    pub burn_block: ConsensusHash,
    /// The tip burn block height (i.e., the latest bitcoin block) seen by this signer
    pub burn_block_height: u64,
    /// The signer's view of who the current miner should be (and their tenure building info)
    pub current_miner: MinerState,
    /// The active signing protocol version
    pub active_signer_protocol_version: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Enum for capturing the signer state machine's view of who
///  should be the active miner and what their tenure should be
///  built on top of.
pub enum MinerState {
    /// The information for the current active miner
    ActiveMiner {
        /// The pubkeyhash of the current miner's signing key
        current_miner_pkh: Hash160,
        /// The tenure ID of the current miner's active tenure
        tenure_id: ConsensusHash,
        /// The tenure that the current miner is building on top of
        parent_tenure_id: ConsensusHash,
        /// The last block of the parent tenure (which should be
        ///  the block that the next tenure starts from)
        parent_tenure_last_block: StacksBlockId,
        /// The height of the last block of the parent tenure (which should be
        ///  the block that the next tenure starts from)
        parent_tenure_last_block_height: u64,
    },
    /// This signer doesn't believe there's any valid miner
    NoValidMiner,
}

/// The local signer state machine
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LocalStateMachine {
    /// The local state machine couldn't be instantiated
    Uninitialized,
    /// The local state machine is instantiated
    Initialized(SignerStateMachine),
    /// The local state machine has a pending update
    Pending {
        /// The pending update
        update: StateMachineUpdate,
        /// The local state machine before the pending update
        prior: SignerStateMachine,
    },
}

/// A pending update for a signer state machine
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateMachineUpdate {
    /// A new burn block at height u64 is expected
    BurnBlock(u64),
}

impl TryInto<StateMachineUpdateMessage> for &LocalStateMachine {
    type Error = CodecError;

    fn try_into(self) -> Result<StateMachineUpdateMessage, Self::Error> {
        let LocalStateMachine::Initialized(state_machine) = self else {
            return Err(CodecError::SerializeError(
                "Local state machine is not ready to be serialized into an update message".into(),
            ));
        };

        let current_miner = match state_machine.current_miner {
            MinerState::ActiveMiner {
                current_miner_pkh,
                tenure_id,
                parent_tenure_id,
                parent_tenure_last_block,
                parent_tenure_last_block_height,
            } => StateMachineUpdateMinerState::ActiveMiner {
                current_miner_pkh,
                tenure_id,
                parent_tenure_id,
                parent_tenure_last_block,
                parent_tenure_last_block_height,
            },
            MinerState::NoValidMiner => StateMachineUpdateMinerState::NoValidMiner,
        };

        StateMachineUpdateMessage::new(
            state_machine.active_signer_protocol_version,
            SUPPORTED_SIGNER_PROTOCOL_VERSION,
            StateMachineUpdateContent::V0 {
                burn_block: state_machine.burn_block,
                burn_block_height: state_machine.burn_block_height,
                current_miner,
            },
        )
    }
}

impl LocalStateMachine {
    /// Initialize a local state machine by querying the local stacks-node
    ///  and signerdb for the current sortition information
    pub fn new(
        db: &SignerDb,
        stackerdb: &mut StackerDB<MessageSlotID>,
        client: &StacksClient,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<Self, SignerChainstateError> {
        let mut instance = Self::Uninitialized;
        instance.bitcoin_block_arrival(db, stackerdb, client, proposal_config, None)?;

        Ok(instance)
    }

    fn place_holder() -> SignerStateMachine {
        SignerStateMachine {
            burn_block: ConsensusHash::empty(),
            burn_block_height: 0,
            current_miner: MinerState::NoValidMiner,
            active_signer_protocol_version: SUPPORTED_SIGNER_PROTOCOL_VERSION,
        }
    }

    /// Send the local state machine as a signer update message to stackerdb
    pub fn send_signer_update_message(&self, stackerdb: &mut StackerDB<MessageSlotID>) {
        let update: Result<StateMachineUpdateMessage, _> = self.try_into();
        match update {
            Ok(update) => {
                if let Err(e) = stackerdb.send_message_with_retry::<SignerMessage>(update.into()) {
                    warn!("Failed to send signer update to stacker-db: {e:?}",);
                }
            }
            Err(e) => {
                warn!("Failed to convert local signer state to a signer message: {e:?}");
            }
        }
    }

    /// If this local state machine has pending updates, process them
    pub fn handle_pending_update(
        &mut self,
        db: &SignerDb,
        stackerdb: &mut StackerDB<MessageSlotID>,
        client: &StacksClient,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<(), SignerChainstateError> {
        let LocalStateMachine::Pending { update, .. } = self else {
            return self.check_miner_inactivity(db, stackerdb, client, proposal_config);
        };
        match update.clone() {
            StateMachineUpdate::BurnBlock(expected_burn_height) => self.bitcoin_block_arrival(
                db,
                stackerdb,
                client,
                proposal_config,
                Some(expected_burn_height),
            ),
        }
    }

    fn is_timed_out(
        sortition: &ConsensusHash,
        db: &SignerDb,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<bool, SignerChainstateError> {
        // if we've already signed a block in this tenure, the miner can't have timed out.
        let has_block = db.has_signed_block_in_tenure(sortition)?;
        if has_block {
            return Ok(false);
        }
        let Some(received_ts) = db.get_burn_block_receive_time_ch(sortition)? else {
            return Ok(false);
        };
        let received_time = UNIX_EPOCH + Duration::from_secs(received_ts);
        let last_activity = db
            .get_last_activity_time(sortition)?
            .map(|time| UNIX_EPOCH + Duration::from_secs(time))
            .unwrap_or(received_time);

        let Ok(elapsed) = std::time::SystemTime::now().duration_since(last_activity) else {
            return Ok(false);
        };

        if elapsed > proposal_config.block_proposal_timeout {
            info!(
                "Tenure miner was inactive too long and timed out";
                "tenure_ch" => %sortition,
                "elapsed_inactive" => elapsed.as_secs(),
                "config_block_proposal_timeout" => proposal_config.block_proposal_timeout.as_secs()
            );
        }
        Ok(elapsed > proposal_config.block_proposal_timeout)
    }

    fn check_miner_inactivity(
        &mut self,
        db: &SignerDb,
        stackerdb: &mut StackerDB<MessageSlotID>,
        client: &StacksClient,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<(), SignerChainstateError> {
        let Self::Initialized(ref mut state_machine) = self else {
            // no inactivity if the state machine isn't initialized
            return Ok(());
        };

        let MinerState::ActiveMiner { ref tenure_id, .. } = state_machine.current_miner else {
            // no inactivity if there's no active miner
            return Ok(());
        };

        if !Self::is_timed_out(tenure_id, db, proposal_config)? {
            return Ok(());
        }

        // the tenure timed out, try to see if we can use the prior tenure instead
        let CurrentAndLastSortition { last_sortition, .. } =
            client.get_current_and_last_sortition()?;
        let last_sortition = last_sortition
            .map(SortitionState::try_from)
            .transpose()
            .ok()
            .flatten();
        let Some(last_sortition) = last_sortition else {
            warn!("Current miner timed out due to inactivity, but could not find a valid prior miner. Allowing current miner to continue");
            return Ok(());
        };

        if Self::is_tenure_valid(&last_sortition, db, client, proposal_config)? {
            let new_active_tenure_ch = last_sortition.consensus_hash;
            let inactive_tenure_ch = *tenure_id;
            state_machine.current_miner =
                Self::make_miner_state(last_sortition, client, db, proposal_config)?;
            info!(
                "Current tenure timed out, setting the active miner to the prior tenure";
                "inactive_tenure_ch" => %inactive_tenure_ch,
                "new_active_tenure_ch" => %new_active_tenure_ch
            );
            // We have updated our state, so let other signers know.
            self.send_signer_update_message(stackerdb);
            Ok(())
        } else {
            warn!("Current miner timed out due to inactivity, but prior miner is not valid. Allowing current miner to continue");
            Ok(())
        }
    }

    fn make_miner_state(
        sortition_to_set: SortitionState,
        client: &StacksClient,
        db: &SignerDb,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<MinerState, SignerChainstateError> {
        let next_current_miner_pkh = sortition_to_set.miner_pkh;
        let next_parent_tenure_id = sortition_to_set.parent_tenure_id;

        let stacks_node_last_block = client
            .get_tenure_tip(&next_parent_tenure_id)
            .inspect_err(|e| {
                warn!(
                    "Failed to fetch last block in parent tenure from stacks-node";
                    "parent_tenure_id" => %sortition_to_set.parent_tenure_id,
                    "err" => ?e,
                )
            })
            .ok()
            .map(|header| {
                (
                    header.height(),
                    StacksBlockId::new(&next_parent_tenure_id, &header.block_hash()),
                )
            });
        let signerdb_last_block = SortitionsView::get_tenure_last_block_info(
            &next_parent_tenure_id,
            db,
            proposal_config.tenure_last_block_proposal_timeout,
        )?
        .map(|info| (info.block.header.chain_length, info.block.block_id()));

        let (parent_tenure_last_block_height, parent_tenure_last_block) =
            match (stacks_node_last_block, signerdb_last_block) {
                (Some(stacks_node_info), Some(signerdb_info)) => {
                    std::cmp::max_by_key(stacks_node_info, signerdb_info, |info| info.0)
                }
                (None, Some(signerdb_info)) => signerdb_info,
                (Some(stacks_node_info), None) => stacks_node_info,
                (None, None) => {
                    return Err(SignerChainstateError::NoParentTenureInfo(
                        next_parent_tenure_id,
                    ))
                }
            };

        let miner_state = MinerState::ActiveMiner {
            current_miner_pkh: next_current_miner_pkh,
            tenure_id: sortition_to_set.consensus_hash,
            parent_tenure_id: next_parent_tenure_id,
            parent_tenure_last_block,
            parent_tenure_last_block_height,
        };

        Ok(miner_state)
    }

    /// Handle a new stacks block arrival
    pub fn stacks_block_arrival(
        &mut self,
        stackerdb: &mut StackerDB<MessageSlotID>,
        ch: &ConsensusHash,
        height: u64,
        block_id: &StacksBlockId,
    ) -> Result<(), SignerChainstateError> {
        // set self to uninitialized so that if this function errors,
        //  self is left as uninitialized.
        let prior_state = std::mem::replace(self, Self::Uninitialized);
        let mut prior_state_machine = match prior_state {
            // if the local state machine was uninitialized, just initialize it
            LocalStateMachine::Initialized(signer_state_machine) => signer_state_machine,
            LocalStateMachine::Uninitialized => {
                // we don't need to update any state when we're uninitialized for new stacks block
                //  arrivals
                return Ok(());
            }
            LocalStateMachine::Pending { update, prior } => {
                // This works as long as the pending updates are only burn blocks,
                //  but if we have other kinds of pending updates, this logic will need
                //  to be changed.
                match &update {
                    StateMachineUpdate::BurnBlock(..) => {
                        *self = LocalStateMachine::Pending { update, prior };
                        return Ok(());
                    }
                }
            }
        };

        let MinerState::ActiveMiner {
            parent_tenure_id,
            parent_tenure_last_block,
            parent_tenure_last_block_height,
            ..
        } = &mut prior_state_machine.current_miner
        else {
            // if there's no valid miner, then we don't need to update any state for new stacks blocks
            *self = LocalStateMachine::Initialized(prior_state_machine);
            return Ok(());
        };

        if parent_tenure_id != ch {
            // if the new block isn't from the parent tenure, we don't need any updates
            *self = LocalStateMachine::Initialized(prior_state_machine);
            return Ok(());
        }

        if height <= *parent_tenure_last_block_height {
            // if the new block isn't higher than we already expected, we don't need any updates
            *self = LocalStateMachine::Initialized(prior_state_machine);
            return Ok(());
        }

        *parent_tenure_last_block = *block_id;
        *parent_tenure_last_block_height = height;
        *self = LocalStateMachine::Initialized(prior_state_machine);
        // We updated the block id and/or the height. Let other signers know our view has changed
        self.send_signer_update_message(stackerdb);
        Ok(())
    }

    /// check if the tenure defined by sortition state:
    ///  (1) chose an appropriate parent tenure
    ///  (2) has not "timed out"
    fn is_tenure_valid(
        sortition_state: &SortitionState,
        signer_db: &SignerDb,
        client: &StacksClient,
        proposal_config: &ProposalEvalConfig,
    ) -> Result<bool, SignerChainstateError> {
        let standin_block = NakamotoBlock {
            header: NakamotoBlockHeader {
                version: 0,
                chain_length: 0,
                burn_spent: 0,
                consensus_hash: sortition_state.consensus_hash,
                parent_block_id: StacksBlockId::first_mined(),
                tx_merkle_root: Sha512Trunc256Sum([0; 32]),
                state_index_root: TrieHash([0; 32]),
                timestamp: 0,
                miner_signature: MessageSignature::empty(),
                signer_signature: vec![],
                pox_treatment: BitVec::ones(1).unwrap(),
            },
            txs: vec![],
        };

        let chose_good_parent = SortitionsView::check_parent_tenure_choice(
            sortition_state,
            &standin_block,
            signer_db,
            client,
            &proposal_config.first_proposal_burn_block_timing,
        )?;
        if !chose_good_parent {
            return Ok(false);
        }
        Self::is_timed_out(&sortition_state.consensus_hash, signer_db, proposal_config)
            .map(|timed_out| !timed_out)
    }

    /// Handle a new bitcoin block arrival
    pub fn bitcoin_block_arrival(
        &mut self,
        db: &SignerDb,
        stackerdb: &mut StackerDB<MessageSlotID>,
        client: &StacksClient,
        proposal_config: &ProposalEvalConfig,
        mut expected_burn_height: Option<u64>,
    ) -> Result<(), SignerChainstateError> {
        // set self to uninitialized so that if this function errors,
        //  self is left as uninitialized.
        let prior_state = std::mem::replace(self, Self::Uninitialized);
        let prior_state_machine = match prior_state.clone() {
            // if the local state machine was uninitialized, just initialize it
            LocalStateMachine::Uninitialized => Self::place_holder(),
            LocalStateMachine::Initialized(signer_state_machine) => signer_state_machine,
            LocalStateMachine::Pending { update, prior } => {
                // This works as long as the pending updates are only burn blocks,
                //  but if we have other kinds of pending updates, this logic will need
                //  to be changed.
                match update {
                    StateMachineUpdate::BurnBlock(pending_burn_height) => {
                        if pending_burn_height > expected_burn_height.unwrap_or(0) {
                            expected_burn_height = Some(pending_burn_height);
                        }
                    }
                }

                prior
            }
        };

        let peer_info = client.get_peer_info()?;
        let next_burn_block_height = peer_info.burn_block_height;
        let next_burn_block_hash = peer_info.pox_consensus;

        if let Some(expected_burn_height) = expected_burn_height {
            if next_burn_block_height < expected_burn_height {
                *self = Self::Pending {
                    update: StateMachineUpdate::BurnBlock(expected_burn_height),
                    prior: prior_state_machine,
                };
                return Err(ClientError::InvalidResponse(
                    "Node has not processed the next burn block yet".into(),
                )
                .into());
            }
        }

        let CurrentAndLastSortition {
            current_sortition,
            last_sortition,
        } = client.get_current_and_last_sortition()?;

        let cur_sortition = SortitionState::try_from(current_sortition)?;
        let last_sortition = last_sortition
            .map(SortitionState::try_from)
            .transpose()
            .ok()
            .flatten()
            .ok_or_else(|| {
                ClientError::InvalidResponse(
                    "Fetching latest and last sortitions failed to return both sortitions".into(),
                )
            })?;

        let is_current_valid = Self::is_tenure_valid(&cur_sortition, db, client, proposal_config)?;

        let miner_state = if is_current_valid {
            Self::make_miner_state(cur_sortition, client, db, proposal_config)?
        } else {
            let is_last_valid =
                Self::is_tenure_valid(&last_sortition, db, client, proposal_config)?;

            if is_last_valid {
                Self::make_miner_state(last_sortition, client, db, proposal_config)?
            } else {
                warn!("Neither the current nor the prior sortition winner is considered a valid tenure");
                MinerState::NoValidMiner
            }
        };

        // Note: we do this at the end so that the transform isn't fallible.
        //  we should come up with a better scheme here.
        *self = Self::Initialized(SignerStateMachine {
            burn_block: next_burn_block_hash,
            burn_block_height: next_burn_block_height,
            current_miner: miner_state,
            active_signer_protocol_version: prior_state_machine.active_signer_protocol_version,
        });

        if prior_state != *self {
            self.send_signer_update_message(stackerdb);
        }

        Ok(())
    }
}
