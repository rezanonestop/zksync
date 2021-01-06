// External deps
use web3::{
    contract::Contract,
    types::{H160, H256},
    Transport, Web3,
};
// Workspace deps
use zksync_contracts::{governance_contract, upgrade_gatekeeper};
use zksync_crypto::Fr;

use zksync_types::{AccountMap, AccountUpdate};
// Local deps
use crate::contract::{ZkSyncContractVersion, ZkSyncDeployedContract};
use crate::storage_interactor::StorageInteractor;
use crate::{
    contract::get_genesis_account, eth_tx_helpers::get_ethereum_transaction,
    events_state::EventsState, rollup_ops::RollupOpsBlock, tree_state::TreeState,
};
use ethabi::Address;
use serde::export::PhantomData;
use std::convert::TryFrom;
use web3::types::{BlockNumber, FilterBuilder, Log, U256};

/// Storage state update:
/// - None - The state is updated completely last time - start from fetching the new events
/// - Events - The events fetched and saved successfully - now get operations from them and update tree
/// - Operations - There are operations that are not presented in the tree state - update tree state
#[derive(Debug, Copy, Clone)]
pub enum StorageUpdateState {
    None,
    Events,
    Operations,
}

/// Data restore driver is a high level interface for all restoring components.
/// It is actually a finite state machine, that has following states:
/// - Empty - The state is new
/// - None - The state is completely updated last time, driver will load state from storage and fetch new events
/// - Events - The events has been fetched and saved successfully and firstly driver will load state from storage
///   and get new operation for last saved events
/// - Operations - The operations and events has been fetched and saved successfully and firstly driver will load
///   state from storage and update merkle tree by last saved operations
///
/// Driver can interact with other restoring components for their updating:
/// - Events
/// - Operations
/// - Tree
/// - Storage
pub struct DataRestoreDriver<T: Transport, I> {
    /// Web3 provider endpoint
    pub web3: Web3<T>,
    /// Provides Ethereum Governance contract unterface
    pub governance_contract: (ethabi::Contract, Contract<T>),
    /// Provides Ethereum Rollup contract unterface
    pub zksync_contracts: Vec<ZkSyncDeployedContract<T>>,
    /// Rollup contract events state
    pub events_state: EventsState,
    /// Rollup accounts state
    pub tree_state: TreeState,
    /// The step distance of viewing events in the ethereum blocks
    pub eth_blocks_step: u64,
    /// The distance to the last ethereum block
    pub end_eth_blocks_offset: u64,
    /// Available block chunk sizes
    pub available_block_chunk_sizes: Vec<usize>,
    /// Finite mode flag. In finite mode, driver will only work until
    /// amount of restored blocks will become equal to amount of known
    /// verified blocks. After that, it will stop.
    pub finite_mode: bool,
    /// Expected root hash to be observed after restoring process. Only
    /// available in finite mode, and intended for tests.
    pub final_hash: Option<Fr>,
    phantom_data: PhantomData<I>,
}

impl<T, I> DataRestoreDriver<T, I>
where
    T: Transport,
    I: StorageInteractor,
{
    /// Returns new data restore driver with empty events and tree states.
    ///
    /// # Arguments
    ///
    /// * `web3_transport` - Web3 provider transport
    /// * `governance_contract_eth_addr` - Governance contract address
    /// * `zksync_contract_eth_addr` - Rollup contract address
    /// * `eth_blocks_step` - The step distance of viewing events in the ethereum blocks
    /// * `end_eth_blocks_offset` - The distance to the last ethereum block
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        web3_transport: T,
        governance_contract_eth_addr: H160,
        eth_blocks_step: u64,
        end_eth_blocks_offset: u64,
        available_block_chunk_sizes: Vec<usize>,
        finite_mode: bool,
        final_hash: Option<Fr>,
    ) -> Self {
        let web3 = Web3::new(web3_transport);

        let governance_contract = {
            let abi = governance_contract();
            (
                abi.clone(),
                Contract::new(web3.eth(), governance_contract_eth_addr, abi),
            )
        };

        let events_state = EventsState::default();

        let tree_state = TreeState::new(available_block_chunk_sizes.clone());
        Self {
            web3,
            governance_contract,
            zksync_contracts: vec![],
            events_state,
            tree_state,
            eth_blocks_step,
            end_eth_blocks_offset,
            available_block_chunk_sizes,
            finite_mode,
            final_hash,
            phantom_data: Default::default(),
        }
    }

    pub async fn get_gatekeeper_logs(
        &self,
        upgrade_gatekeeper_contract_address: Address,
    ) -> anyhow::Result<Vec<Log>> {
        let gatekeeper_abi = upgrade_gatekeeper();
        let upgrade_contract_event = gatekeeper_abi
            .event("UpgradeComplete")
            .expect("Upgrade Gatekeeper contract abi error")
            .signature();

        let filter = FilterBuilder::default()
            .address(vec![upgrade_gatekeeper_contract_address])
            .from_block(BlockNumber::Earliest)
            .to_block(BlockNumber::Latest)
            .topics(Some(vec![upgrade_contract_event]), None, None, None)
            .build();

        let result = self
            .web3
            .eth()
            .logs(filter)
            .await
            .map_err(|e| anyhow::format_err!("No new logs: {}", e))?;
        Ok(result)
    }
    pub async fn init_contracts(
        &mut self,
        upgrade_gatekeeper_contract_addr: Address,
        zksync_contract_addr: Address,
    ) -> anyhow::Result<()> {
        let logs = self
            .get_gatekeeper_logs(upgrade_gatekeeper_contract_addr)
            .await?;

        let mut last_updated_block = BlockNumber::Earliest;
        let mut contract_version = vec![];
        let mut previous_version: Option<ZkSyncContractVersion> = None;
        // Find starts and ends for contracts
        for log in logs {
            let block_number = log.block_number.expect("Block number should exist");
            let version = U256::from(log.topics[1].as_bytes()).as_u32();
            let version = ZkSyncContractVersion::try_from(version)?;
            let current_block = BlockNumber::Number(block_number);
            if let Some(previous_version) = previous_version {
                contract_version.push((last_updated_block, current_block, previous_version));
            }
            previous_version = Some(version);
            last_updated_block = current_block;
        }
        contract_version.push((
            last_updated_block,
            BlockNumber::Latest,
            previous_version.expect("At least one contract should exist"),
        ));

        for (from, to, version) in contract_version {
            match version {
                ZkSyncContractVersion::V0 => {
                    self.zksync_contracts.push(ZkSyncDeployedContract::version0(
                        self.web3.eth(),
                        zksync_contract_addr,
                        from,
                        to,
                    ))
                }
                ZkSyncContractVersion::V1 => {
                    self.zksync_contracts.push(ZkSyncDeployedContract::version1(
                        self.web3.eth(),
                        zksync_contract_addr,
                        from,
                        to,
                    ))
                }
                ZkSyncContractVersion::V2 => {
                    self.zksync_contracts.push(ZkSyncDeployedContract::version2(
                        self.web3.eth(),
                        zksync_contract_addr,
                        from,
                        to,
                    ))
                }
                ZkSyncContractVersion::V3 => {
                    self.zksync_contracts.push(ZkSyncDeployedContract::version3(
                        self.web3.eth(),
                        zksync_contract_addr,
                        from,
                        to,
                    ))
                }
                ZkSyncContractVersion::V4 => {
                    self.zksync_contracts.push(ZkSyncDeployedContract::version4(
                        self.web3.eth(),
                        zksync_contract_addr,
                        from,
                        to,
                    ))
                }
            }
        }
        Ok(())
    }

    /// Sets the 'genesis' state.
    /// Tree with inserted genesis account will be created.
    /// Used when restore driver is restarted.
    ///
    /// # Arguments
    ///
    /// * `governance_contract_genesis_tx_hash` - Governance contract creation tx hash
    ///
    pub async fn set_genesis_state(&mut self, interactor: &mut I, genesis_tx_hash: H256) {
        let genesis_transaction = get_ethereum_transaction(&self.web3, &genesis_tx_hash)
            .await
            .expect("Cant get zkSync genesis transaction");

        // Setting genesis block number for events state
        let genesis_eth_block_number = self
            .events_state
            .set_genesis_block_number(&genesis_transaction)
            .expect("Cant set genesis block number for events state");
        log::info!("genesis_eth_block_number: {:?}", &genesis_eth_block_number);

        interactor
            .save_events_state(&[], &[], genesis_eth_block_number)
            .await;

        let genesis_fee_account =
            get_genesis_account(&genesis_transaction).expect("Cant get genesis account address");

        log::info!(
            "genesis fee account address: 0x{}",
            hex::encode(genesis_fee_account.address.as_ref())
        );

        let account_update = AccountUpdate::Create {
            address: genesis_fee_account.address,
            nonce: genesis_fee_account.nonce,
        };

        let mut account_map = AccountMap::default();
        account_map.insert(0, genesis_fee_account);

        let current_block = 0;
        let current_unprocessed_priority_op = 0;
        let fee_acc_num = 0;

        let tree_state = TreeState::load(
            current_block,
            account_map,
            current_unprocessed_priority_op,
            fee_acc_num,
            self.available_block_chunk_sizes.clone(),
        );

        log::info!("Genesis tree root hash: {:?}", tree_state.root_hash());
        log::debug!("Genesis accounts: {:?}", tree_state.get_accounts());

        interactor.save_genesis_tree_state(account_update).await;

        log::info!("Saved genesis tree state\n");

        self.tree_state = tree_state;
    }

    fn actual_zksync_contract(&self) -> &ZkSyncDeployedContract<T> {
        self.zksync_contracts
            .last()
            .expect("At least one should exist")
    }
    /// Stops states from storage
    pub async fn load_state_from_storage(&mut self, interactor: &mut I) -> bool {
        log::info!("Loading state from storage");
        let state = interactor.get_storage_state().await;
        self.events_state = interactor.get_block_events_state_from_storage().await;
        let tree_state = interactor.get_tree_state().await;
        self.tree_state = TreeState::load(
            tree_state.last_block_number,     // current block
            tree_state.account_map,           // account map
            tree_state.unprocessed_prior_ops, // unprocessed priority op
            tree_state.fee_acc_id,            // fee account
            self.available_block_chunk_sizes.clone(),
        );
        match state {
            StorageUpdateState::Events => {
                // Update operations
                let new_ops_blocks = self.update_operations_state(interactor).await;
                // Update tree
                self.update_tree_state(interactor, new_ops_blocks).await;
            }
            StorageUpdateState::Operations => {
                // Update operations
                let new_ops_blocks = interactor.get_ops_blocks_from_storage().await;
                // Update tree
                self.update_tree_state(interactor, new_ops_blocks).await;
            }
            StorageUpdateState::None => {}
        }
        let total_verified_blocks = self
            .actual_zksync_contract()
            .get_total_verified_blocks()
            .await;

        let last_verified_block = self.tree_state.state.block_number;
        log::info!(
            "State has been loaded\nProcessed {:?} blocks on contract\nRoot hash: {:?}\n",
            last_verified_block,
            self.tree_state.root_hash()
        );

        self.finite_mode && (total_verified_blocks == last_verified_block)
    }

    /// Activates states updates
    pub async fn run_state_update(&mut self, interactor: &mut I) {
        let mut last_watched_block: u64 = self.events_state.last_watched_eth_block_number;
        let mut final_hash_was_found = false;
        loop {
            log::info!("Last watched ethereum block: {:?}", last_watched_block);

            // Update events
            if self.update_events_state(interactor).await {
                // Update operations
                let new_ops_blocks = self.update_operations_state(interactor).await;

                if !new_ops_blocks.is_empty() {
                    // Update tree
                    log::info!(
                        "new ops blocks {:?} {:?}",
                        &new_ops_blocks.len(),
                        new_ops_blocks.last()
                    );
                    self.update_tree_state(interactor, new_ops_blocks).await;

                    let total_verified_blocks = self
                        .actual_zksync_contract()
                        .get_total_verified_blocks()
                        .await;

                    let last_verified_block = self.tree_state.state.block_number;

                    // We must update the Ethereum stats table to match the actual stored state
                    // to keep the `state_keeper` consistent with the `eth_sender`.
                    interactor.update_eth_state().await;

                    log::info!(
                        "State updated\nProcessed {:?} blocks of total {:?} verified on contract\nRoot hash: {:?}\n",
                        last_verified_block,
                        total_verified_blocks,
                        self.tree_state.root_hash()
                    );

                    // If there is an expected root hash, check if current root hash matches the observed
                    // one.
                    // We check it after every block, since provided final hash may be not the latest hash
                    // by the time when it was processed.
                    if let Some(root_hash) = self.final_hash {
                        if root_hash == self.tree_state.root_hash() {
                            final_hash_was_found = true;

                            log::info!(
                                "Correct expected root hash was met on the block {} out of {}",
                                last_verified_block,
                                total_verified_blocks
                            );
                        }
                    }

                    if self.finite_mode && last_verified_block == total_verified_blocks {
                        // Check if the final hash was found and panic otherwise.
                        if self.final_hash.is_some() && !final_hash_was_found {
                            panic!("Final hash was not met during the state restoring process");
                        }

                        // We've restored all the blocks, our job is done.
                        break;
                    }
                }
            }

            if last_watched_block == self.events_state.last_watched_eth_block_number {
                log::info!("sleep block");
                std::thread::sleep(std::time::Duration::from_secs(5));
            } else {
                last_watched_block = self.events_state.last_watched_eth_block_number;
            }
        }
    }

    /// Updates events state, saves new blocks, tokens events and the last watched eth block number in storage
    /// Returns bool flag, true if there are new block events
    async fn update_events_state(&mut self, interactor: &mut I) -> bool {
        let (block_events, token_events, last_watched_eth_block_number) = self
            .events_state
            .update_events_state(
                &self.web3,
                &self.zksync_contracts,
                &self.governance_contract,
                self.eth_blocks_step,
                self.end_eth_blocks_offset,
            )
            .await
            .expect("Updating events state: cant update events state");
        log::info!("Before saving to store");
        interactor
            .save_events_state(
                &block_events,
                token_events.as_slice(),
                last_watched_eth_block_number,
            )
            .await;
        log::info!("Updated events storage");

        !block_events.is_empty()
    }

    /// Updates tree state from the new Rollup operations blocks, saves it in storage
    ///
    /// # Arguments
    ///
    /// * `new_ops_blocks` - the new Rollup operations blocks
    ///
    async fn update_tree_state(&mut self, interactor: &mut I, new_ops_blocks: Vec<RollupOpsBlock>) {
        let mut blocks = vec![];
        let mut updates = vec![];
        let mut count = 0;
        for op_block in new_ops_blocks {
            log::info!("Block number {:?}", op_block.block_num);
            let (block, acc_updates) = self
                .tree_state
                .update_tree_states_from_ops_block(&op_block)
                .expect("Updating tree state: cant update tree from operations");
            blocks.push(block);
            updates.push(acc_updates);
            count += 1;
        }
        for i in 0..count {
            interactor
                .update_tree_state(blocks[i].clone(), updates[i].clone())
                .await;
        }

        log::debug!("Updated state");
    }

    /// Gets new operations blocks from events, updates rollup operations stored state.
    /// Returns new rollup operations blocks
    async fn update_operations_state(&mut self, interactor: &mut I) -> Vec<RollupOpsBlock> {
        log::info!("Pre get new operations");
        let new_blocks = self.get_new_operation_blocks_from_events().await;
        log::info!("Post get new operations");

        interactor.save_rollup_ops(&new_blocks).await;

        log::debug!("Updated operations storage");

        new_blocks
    }

    /// Returns verified comitted operations blocks from verified op blocks events
    pub async fn get_new_operation_blocks_from_events(&mut self) -> Vec<RollupOpsBlock> {
        let mut blocks = Vec::new();

        for event in self
            .events_state
            .get_only_verified_committed_events()
            .iter()
        {
            let block = RollupOpsBlock::get_rollup_ops_blocks(&self.web3, &event)
                .await
                .expect("Cant get new operation blocks from events");
            blocks.extend(block);
        }

        blocks
    }
}
