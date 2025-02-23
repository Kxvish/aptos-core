// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, ensure, format_err, Context as AnyhowContext, Result};
use aptos_api_types::{AsConverter, BlockInfo, Error, LedgerInfo, TransactionOnChainData, U64};
use aptos_config::config::{NodeConfig, RoleType};
use aptos_crypto::HashValue;
use aptos_mempool::{MempoolClientRequest, MempoolClientSender, SubmissionStatus};
use aptos_state_view::StateView;
use aptos_types::{
    access_path::Path,
    account_address::AccountAddress,
    account_config::CORE_CODE_ADDRESS,
    account_state::AccountState,
    chain_id::ChainId,
    contract_event::ContractEvent,
    event::EventKey,
    ledger_info::LedgerInfoWithSignatures,
    state_store::{state_key::StateKey, state_key_prefix::StateKeyPrefix, state_value::StateValue},
    transaction::{SignedTransaction, TransactionWithProof, Version},
    write_set::WriteOp,
};
use aptos_vm::data_cache::{IntoMoveResolver, RemoteStorageOwned};
use futures::{channel::oneshot, SinkExt};
use move_deps::move_core_types::ident_str;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use storage_interface::{
    state_view::{DbStateView, DbStateViewAtVersion, LatestDbStateCheckpointView},
    DbReader, Order,
};
use warp::{filters::BoxedFilter, Filter, Reply};

use crate::poem_backend::{AptosErrorCode, InternalError};

// Context holds application scope context
#[derive(Clone)]
pub struct Context {
    chain_id: ChainId,
    pub db: Arc<dyn DbReader>,
    mp_sender: MempoolClientSender,
    node_config: NodeConfig,
}

impl Context {
    pub fn new(
        chain_id: ChainId,
        db: Arc<dyn DbReader>,
        mp_sender: MempoolClientSender,
        node_config: NodeConfig,
    ) -> Self {
        Self {
            chain_id,
            db,
            mp_sender,
            node_config,
        }
    }

    pub fn move_resolver(&self) -> Result<RemoteStorageOwned<DbStateView>> {
        self.db
            .latest_state_checkpoint_view()
            .map(|state_view| state_view.into_move_resolver())
    }

    pub fn move_resolver_poem<E: InternalError>(
        &self,
    ) -> Result<RemoteStorageOwned<DbStateView>, E> {
        self.move_resolver()
            .context("Failed to read latest state checkpoint from DB")
            .map_err(|e| E::internal(e).error_code(AptosErrorCode::ReadFromStorageError))
    }

    pub fn state_view_at_version(&self, version: Version) -> Result<DbStateView> {
        self.db.state_view_at_version(Some(version))
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    pub fn node_role(&self) -> RoleType {
        self.node_config.base.role
    }

    pub fn content_length_limit(&self) -> u64 {
        self.node_config.api.content_length_limit()
    }

    pub fn filter(self) -> impl Filter<Extract = (Context,), Error = Infallible> + Clone {
        warp::any().map(move || self.clone())
    }

    pub async fn submit_transaction(&self, txn: SignedTransaction) -> Result<SubmissionStatus> {
        let (req_sender, callback) = oneshot::channel();
        self.mp_sender
            .clone()
            .send(MempoolClientRequest::SubmitTransaction(txn, req_sender))
            .await?;

        callback.await?
    }

    pub fn get_latest_ledger_info(&self) -> Result<LedgerInfo, Error> {
        if let Some(oldest_version) = self.db.get_first_txn_version()? {
            Ok(LedgerInfo::new(
                &self.chain_id(),
                &self.get_latest_ledger_info_with_signatures()?,
                oldest_version,
            ))
        } else {
            return Err(anyhow! {"Failed to retrieve oldest version"}.into());
        }
    }

    // TODO: Add error codes to these errors.
    pub fn get_latest_ledger_info_poem<E: InternalError>(&self) -> Result<LedgerInfo, E> {
        if let Some(oldest_version) = self
            .db
            .get_first_txn_version()
            .map_err(|e| E::internal(e).error_code(AptosErrorCode::ReadFromStorageError))?
        {
            Ok(LedgerInfo::new(
                &self.chain_id(),
                &self
                    .get_latest_ledger_info_with_signatures()
                    .map_err(E::internal)?,
                oldest_version,
            ))
        } else {
            Err(E::internal(anyhow!(
                "Failed to retrieve latest ledger info"
            )))
        }
    }

    pub fn get_latest_ledger_info_with_signatures(&self) -> Result<LedgerInfoWithSignatures> {
        self.db.get_latest_ledger_info()
    }

    pub fn get_state_value(&self, state_key: &StateKey, version: u64) -> Result<Option<Vec<u8>>> {
        self.db
            .state_view_at_version(Some(version))?
            .get_state_value(state_key)
    }

    pub fn get_state_value_poem<E: InternalError>(
        &self,
        state_key: &StateKey,
        version: u64,
    ) -> Result<Option<Vec<u8>>, E> {
        self.get_state_value(state_key, version)
            .context("Failed to retrieve state value")
            .map_err(|e| E::internal(e).error_code(AptosErrorCode::ReadFromStorageError))
    }

    pub fn get_state_values(
        &self,
        address: AccountAddress,
        version: u64,
    ) -> Result<HashMap<StateKey, StateValue>> {
        self.db
            .get_state_values_by_key_prefix(&StateKeyPrefix::from(address), version)
    }

    pub fn get_account_state(
        &self,
        address: AccountAddress,
        version: u64,
    ) -> Result<Option<AccountState>> {
        AccountState::from_access_paths_and_values(&self.get_state_values(address, version)?)
    }

    pub fn get_block_timestamp(&self, version: u64) -> Result<u64> {
        self.db.get_block_timestamp(version)
    }

    /// Retrieves information about a block
    pub fn get_block_info(&self, version: u64, ledger_version: u64) -> Result<BlockInfo> {
        // We scan the DB to get the block boundaries
        let (start, end) = match self.db.get_block_boundaries(version, ledger_version) {
            Ok(inner) => inner,
            Err(error) => {
                // None means we can't find the block
                return Err(anyhow!("Failed to find block boundaries {}", error));
            }
        };

        let txn_with_proof = self
            .db
            .get_transaction_by_version(start, ledger_version, false)?;

        // Retrieve block timestamp and hash
        let timestamp;
        let block_hash;
        use aptos_types::transaction::Transaction::*;
        match &txn_with_proof.transaction {
            GenesisTransaction(_) => {
                timestamp = 0;
                block_hash = HashValue::zero();
            }
            BlockMetadata(inner) => {
                timestamp = inner.timestamp_usecs();
                block_hash = inner.id();
            }
            _ => {
                return Err(anyhow!(
                    "Failed to retrieve BlockMetadata or Genesis transaction"
                ));
            }
        }

        // If timestamp is 0, it's the genesis transaction, and we can stop now
        if timestamp == 0 {
            return Ok(BlockInfo {
                block_height: 0,
                start_version: start,
                end_version: end,
                block_hash: block_hash.into(),
                block_timestamp: timestamp,
                num_transactions: end.saturating_sub(start).saturating_add(1) as u16,
            });
        }

        // Retrieve block height from the transaction outputs
        let height_id = ident_str!("height");
        let block_metadata_type = move_deps::move_core_types::language_storage::StructTag {
            address: CORE_CODE_ADDRESS,
            module: ident_str!("block").into(),
            name: ident_str!("BlockMetadata").into(),
            type_params: vec![],
        };

        let resolver = self.move_resolver()?;
        let converter = resolver.as_converter(self.db.clone());
        let txn = self.get_transaction_by_version(start, ledger_version)?;

        // Parse the resources and find the block metadata resource update
        let maybe_block_height = txn.changes.iter().find_map(|(key, op)| {
            if let StateKey::AccessPath(path) = key {
                if let Path::Resource(typ) = path.get_path() {
                    // If it's block metadata, we can convert it to get the block height
                    // And it must be the root address
                    if path.address == CORE_CODE_ADDRESS && typ == block_metadata_type {
                        if let WriteOp::Value(value) = op {
                            if let Ok(mut resource) = converter.try_into_resource(&typ, value) {
                                if let Some(value) = resource.data.0.remove(&height_id.into()) {
                                    if let Ok(height) = serde_json::from_value::<U64>(value) {
                                        return Some(height.0);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            None
        });

        // This should always work unless there's something unexpected in the block format
        if let Some(block_height) = maybe_block_height {
            Ok(BlockInfo {
                block_height,
                start_version: start,
                end_version: end,
                block_hash: block_hash.into(),
                block_timestamp: timestamp,
                num_transactions: end.saturating_sub(start).saturating_add(1) as u16,
            })
        } else {
            Err(anyhow!(
                "Unable to find block height in metadata transaction {}:{}",
                start,
                end
            ))
        }
    }

    pub fn get_transactions(
        &self,
        start_version: u64,
        limit: u16,
        ledger_version: u64,
    ) -> Result<Vec<TransactionOnChainData>> {
        let data = self
            .db
            .get_transaction_outputs(start_version, limit as u64, ledger_version)?;

        let txn_start_version = data
            .first_transaction_output_version
            .ok_or_else(|| format_err!("no start version from database"))?;
        ensure!(
            txn_start_version == start_version,
            "invalid start version from database: {} != {}",
            txn_start_version,
            start_version
        );

        let infos = data.proof.transaction_infos;
        let transactions_and_outputs = data.transactions_and_outputs;

        ensure!(
            transactions_and_outputs.len() == infos.len(),
            "invalid data size from database: {}, {}",
            transactions_and_outputs.len(),
            infos.len(),
        );

        transactions_and_outputs
            .into_iter()
            .zip(infos.into_iter())
            .enumerate()
            .map(|(i, ((txn, txn_output), info))| {
                let version = start_version + i as u64;
                let (write_set, events, _, _) = txn_output.unpack();
                self.get_accumulator_root_hash(version)
                    .map(|h| (version, txn, info, events, h, write_set).into())
            })
            .collect()
    }

    pub fn get_account_transactions(
        &self,
        address: AccountAddress,
        start_seq_number: u64,
        limit: u16,
        ledger_version: u64,
    ) -> Result<Vec<TransactionOnChainData>> {
        let txns = self.db.get_account_transactions(
            address,
            start_seq_number,
            limit as u64,
            true,
            ledger_version,
        )?;
        txns.into_inner()
            .into_iter()
            .map(|t| self.convert_into_transaction_on_chain_data(t))
            .collect::<Result<Vec<_>>>()
    }

    pub fn get_transaction_by_hash(
        &self,
        hash: HashValue,
        ledger_version: u64,
    ) -> Result<Option<TransactionOnChainData>> {
        self.db
            .get_transaction_by_hash(hash, ledger_version, true)?
            .map(|t| self.convert_into_transaction_on_chain_data(t))
            .transpose()
    }

    pub async fn get_pending_transaction_by_hash(
        &self,
        hash: HashValue,
    ) -> Result<Option<SignedTransaction>> {
        let (req_sender, callback) = oneshot::channel();

        self.mp_sender
            .clone()
            .send(MempoolClientRequest::GetTransactionByHash(hash, req_sender))
            .await
            .map_err(anyhow::Error::from)?;

        callback.await.map_err(anyhow::Error::from)
    }

    pub fn get_transaction_by_version(
        &self,
        version: u64,
        ledger_version: u64,
    ) -> Result<TransactionOnChainData> {
        self.convert_into_transaction_on_chain_data(self.db.get_transaction_by_version(
            version,
            ledger_version,
            true,
        )?)
    }

    pub fn get_accumulator_root_hash(&self, version: u64) -> Result<HashValue> {
        self.db.get_accumulator_root_hash(version)
    }

    fn convert_into_transaction_on_chain_data(
        &self,
        txn: TransactionWithProof,
    ) -> Result<TransactionOnChainData> {
        // the type is Vec<(Transaction, TransactionOutput)> - given we have one transaction here, there should only ever be one value in this array
        let (_, txn_output) = &self
            .db
            .get_transaction_outputs(txn.version, 1, txn.version)?
            .transactions_and_outputs[0];
        self.get_accumulator_root_hash(txn.version)
            .map(|h| (txn, h, txn_output).into())
    }

    pub fn get_events(
        &self,
        event_key: &EventKey,
        start: u64,
        limit: u16,
        ledger_version: u64,
    ) -> Result<Vec<ContractEvent>> {
        let events = self
            .db
            .get_events(event_key, start, Order::Ascending, limit as u64)?;
        Ok(events
            .into_iter()
            .filter(|event| event.transaction_version <= ledger_version)
            .map(|event| event.event)
            .collect::<Vec<_>>())
    }

    pub fn health_check_route(&self) -> BoxedFilter<(impl Reply,)> {
        super::health_check::health_check_route(self.db.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockMetadataState {
    epoch_internal: U64,
    height: U64,
}
