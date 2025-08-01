use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockId;
use alloy_evm::block::calc::{base_block_reward_pre_merge, block_reward, ommer_reward};
use alloy_primitives::{map::HashSet, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    state::{EvmOverrides, StateOverride},
    transaction::TransactionRequest,
    BlockOverrides, Index,
};
use alloy_rpc_types_trace::{
    filter::TraceFilter,
    opcode::{BlockOpcodeGas, TransactionOpcodeGas},
    parity::*,
    tracerequest::TraceCallRequest,
};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardfork, MAINNET, SEPOLIA};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{BlockBody, BlockHeader};
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_api::TraceApiServer;
use reth_rpc_eth_api::{
    helpers::{Call, LoadPendingBlock, LoadTransaction, Trace, TraceExt},
    FromEthApiError, RpcNodeCore,
};
use reth_rpc_eth_types::{error::EthApiError, utils::recover_raw_transaction, EthConfig};
use reth_storage_api::{BlockNumReader, BlockReader};
use reth_tasks::pool::BlockingTaskGuard;
use reth_transaction_pool::{PoolPooledTx, PoolTransaction, TransactionPool};
use revm::DatabaseCommit;
use revm_inspectors::{
    opcode::OpcodeGasInspector,
    tracing::{parity::populate_state_diff, TracingInspector, TracingInspectorConfig},
};
use std::sync::Arc;
use tokio::sync::{AcquireError, OwnedSemaphorePermit};

/// `trace` API implementation.
///
/// This type provides the functionality for handling `trace` related requests.
pub struct TraceApi<Eth> {
    inner: Arc<TraceApiInner<Eth>>,
}

// === impl TraceApi ===

impl<Eth> TraceApi<Eth> {
    /// Create a new instance of the [`TraceApi`]
    pub fn new(
        eth_api: Eth,
        blocking_task_guard: BlockingTaskGuard,
        eth_config: EthConfig,
    ) -> Self {
        let inner = Arc::new(TraceApiInner { eth_api, blocking_task_guard, eth_config });
        Self { inner }
    }

    /// Acquires a permit to execute a tracing call.
    async fn acquire_trace_permit(
        &self,
    ) -> std::result::Result<OwnedSemaphorePermit, AcquireError> {
        self.inner.blocking_task_guard.clone().acquire_owned().await
    }

    /// Access the underlying `Eth` API.
    pub fn eth_api(&self) -> &Eth {
        &self.inner.eth_api
    }
}

impl<Eth: RpcNodeCore> TraceApi<Eth> {
    /// Access the underlying provider.
    pub fn provider(&self) -> &Eth::Provider {
        self.inner.eth_api.provider()
    }
}

// === impl TraceApi === //

impl<Eth> TraceApi<Eth>
where
    // tracing methods do _not_ read from mempool, hence no `LoadBlock` trait
    // bound
    Eth: Trace + Call + LoadPendingBlock + LoadTransaction + 'static,
{
    /// Executes the given call and returns a number of possible traces for it.
    pub async fn trace_call(
        &self,
        trace_request: TraceCallRequest,
    ) -> Result<TraceResults, Eth::Error> {
        let at = trace_request.block_id.unwrap_or_default();
        let config = TracingInspectorConfig::from_parity_config(&trace_request.trace_types);
        let overrides =
            EvmOverrides::new(trace_request.state_overrides, trace_request.block_overrides);
        let mut inspector = TracingInspector::new(config);
        let this = self.clone();
        self.eth_api()
            .spawn_with_call_at(trace_request.call, at, overrides, move |db, evm_env, tx_env| {
                // wrapper is hack to get around 'higher-ranked lifetime error', see
                // <https://github.com/rust-lang/rust/issues/100013>
                let db = db.0;

                let (res, _) = this.eth_api().inspect(&mut *db, evm_env, tx_env, &mut inspector)?;
                let trace_res = inspector
                    .into_parity_builder()
                    .into_trace_results_with_state(&res, &trace_request.trace_types, &db)
                    .map_err(Eth::Error::from_eth_err)?;
                Ok(trace_res)
            })
            .await
    }

    /// Traces a call to `eth_sendRawTransaction` without making the call, returning the traces.
    pub async fn trace_raw_transaction(
        &self,
        tx: Bytes,
        trace_types: HashSet<TraceType>,
        block_id: Option<BlockId>,
    ) -> Result<TraceResults, Eth::Error> {
        let tx = recover_raw_transaction::<PoolPooledTx<Eth::Pool>>(&tx)?
            .map(<Eth::Pool as TransactionPool>::Transaction::pooled_into_consensus);

        let (evm_env, at) = self.eth_api().evm_env_at(block_id.unwrap_or_default()).await?;
        let tx_env = self.eth_api().evm_config().tx_env(tx);

        let config = TracingInspectorConfig::from_parity_config(&trace_types);

        self.eth_api()
            .spawn_trace_at_with_state(evm_env, tx_env, config, at, move |inspector, res, db| {
                inspector
                    .into_parity_builder()
                    .into_trace_results_with_state(&res, &trace_types, &db)
                    .map_err(Eth::Error::from_eth_err)
            })
            .await
    }

    /// Performs multiple call traces on top of the same block. i.e. transaction n will be executed
    /// on top of a pending block with all n-1 transactions applied (traced) first.
    ///
    /// Note: Allows tracing dependent transactions, hence all transactions are traced in sequence
    pub async fn trace_call_many(
        &self,
        calls: Vec<(TransactionRequest, HashSet<TraceType>)>,
        block_id: Option<BlockId>,
    ) -> Result<Vec<TraceResults>, Eth::Error> {
        let at = block_id.unwrap_or(BlockId::pending());
        let (evm_env, at) = self.eth_api().evm_env_at(at).await?;

        let this = self.clone();
        // execute all transactions on top of each other and record the traces
        self.eth_api()
            .spawn_with_state_at_block(at, move |state| {
                let mut results = Vec::with_capacity(calls.len());
                let mut db = CacheDB::new(StateProviderDatabase::new(state));

                let mut calls = calls.into_iter().peekable();

                while let Some((call, trace_types)) = calls.next() {
                    let (evm_env, tx_env) = this.eth_api().prepare_call_env(
                        evm_env.clone(),
                        call,
                        &mut db,
                        Default::default(),
                    )?;
                    let config = TracingInspectorConfig::from_parity_config(&trace_types);
                    let mut inspector = TracingInspector::new(config);
                    let (res, _) =
                        this.eth_api().inspect(&mut db, evm_env, tx_env, &mut inspector)?;

                    let trace_res = inspector
                        .into_parity_builder()
                        .into_trace_results_with_state(&res, &trace_types, &db)
                        .map_err(Eth::Error::from_eth_err)?;

                    results.push(trace_res);

                    // need to apply the state changes of this call before executing the
                    // next call
                    if calls.peek().is_some() {
                        // need to apply the state changes of this call before executing
                        // the next call
                        db.commit(res.state)
                    }
                }

                Ok(results)
            })
            .await
    }

    /// Replays a transaction, returning the traces.
    pub async fn replay_transaction(
        &self,
        hash: B256,
        trace_types: HashSet<TraceType>,
    ) -> Result<TraceResults, Eth::Error> {
        let config = TracingInspectorConfig::from_parity_config(&trace_types);
        self.eth_api()
            .spawn_trace_transaction_in_block(hash, config, move |_, inspector, res, db| {
                let trace_res = inspector
                    .into_parity_builder()
                    .into_trace_results_with_state(&res, &trace_types, &db)
                    .map_err(Eth::Error::from_eth_err)?;
                Ok(trace_res)
            })
            .await
            .transpose()
            .ok_or(EthApiError::TransactionNotFound)?
    }

    /// Returns transaction trace objects at the given index
    ///
    /// Note: For compatibility reasons this only supports 1 single index, since this method is
    /// supposed to return a single trace. See also: <https://github.com/ledgerwatch/erigon/blob/862faf054b8a0fa15962a9c73839b619886101eb/turbo/jsonrpc/trace_filtering.go#L114-L133>
    ///
    /// This returns `None` if `indices` is empty
    pub async fn trace_get(
        &self,
        hash: B256,
        indices: Vec<usize>,
    ) -> Result<Option<LocalizedTransactionTrace>, Eth::Error> {
        if indices.len() != 1 {
            // The OG impl failed if it gets more than a single index
            return Ok(None)
        }
        self.trace_get_index(hash, indices[0]).await
    }

    /// Returns transaction trace object at the given index.
    ///
    /// Returns `None` if the trace object at that index does not exist
    pub async fn trace_get_index(
        &self,
        hash: B256,
        index: usize,
    ) -> Result<Option<LocalizedTransactionTrace>, Eth::Error> {
        Ok(self.trace_transaction(hash).await?.and_then(|traces| traces.into_iter().nth(index)))
    }

    /// Returns all traces for the given transaction hash
    pub async fn trace_transaction(
        &self,
        hash: B256,
    ) -> Result<Option<Vec<LocalizedTransactionTrace>>, Eth::Error> {
        self.eth_api()
            .spawn_trace_transaction_in_block(
                hash,
                TracingInspectorConfig::default_parity(),
                move |tx_info, inspector, _, _| {
                    let traces =
                        inspector.into_parity_builder().into_localized_transaction_traces(tx_info);
                    Ok(traces)
                },
            )
            .await
    }

    /// Returns all opcodes with their count and combined gas usage for the given transaction in no
    /// particular order.
    pub async fn trace_transaction_opcode_gas(
        &self,
        tx_hash: B256,
    ) -> Result<Option<TransactionOpcodeGas>, Eth::Error> {
        self.eth_api()
            .spawn_trace_transaction_in_block_with_inspector(
                tx_hash,
                OpcodeGasInspector::default(),
                move |_tx_info, inspector, _res, _| {
                    let trace = TransactionOpcodeGas {
                        transaction_hash: tx_hash,
                        opcode_gas: inspector.opcode_gas_iter().collect(),
                    };
                    Ok(trace)
                },
            )
            .await
    }

    /// Calculates the base block reward for the given block:
    ///
    /// - if Paris hardfork is activated, no block rewards are given
    /// - if Paris hardfork is not activated, calculate block rewards with block number only
    /// - if Paris hardfork is unknown, calculate block rewards with block number and ttd
    fn calculate_base_block_reward<H: BlockHeader>(
        &self,
        header: &H,
    ) -> Result<Option<u128>, Eth::Error> {
        let chain_spec = self.provider().chain_spec();
        let is_paris_activated = if chain_spec.chain() == MAINNET.chain() {
            Some(header.number()) >= EthereumHardfork::Paris.mainnet_activation_block()
        } else if chain_spec.chain() == SEPOLIA.chain() {
            Some(header.number()) >= EthereumHardfork::Paris.sepolia_activation_block()
        } else {
            true
        };

        if is_paris_activated {
            return Ok(None)
        }

        Ok(Some(base_block_reward_pre_merge(&chain_spec, header.number())))
    }

    /// Extracts the reward traces for the given block:
    ///  - block reward
    ///  - uncle rewards
    fn extract_reward_traces<H: BlockHeader>(
        &self,
        header: &H,
        ommers: Option<&[H]>,
        base_block_reward: u128,
    ) -> Vec<LocalizedTransactionTrace> {
        let ommers_cnt = ommers.map(|o| o.len()).unwrap_or_default();
        let mut traces = Vec::with_capacity(ommers_cnt + 1);

        let block_reward = block_reward(base_block_reward, ommers_cnt);
        traces.push(reward_trace(
            header,
            RewardAction {
                author: header.beneficiary(),
                reward_type: RewardType::Block,
                value: U256::from(block_reward),
            },
        ));

        let Some(ommers) = ommers else { return traces };

        for uncle in ommers {
            let uncle_reward = ommer_reward(base_block_reward, header.number(), uncle.number());
            traces.push(reward_trace(
                header,
                RewardAction {
                    author: uncle.beneficiary(),
                    reward_type: RewardType::Uncle,
                    value: U256::from(uncle_reward),
                },
            ));
        }
        traces
    }
}

impl<Eth> TraceApi<Eth>
where
    // tracing methods read from mempool, hence `LoadBlock` trait bound via
    // `TraceExt`
    Eth: TraceExt + 'static,
{
    /// Returns all transaction traces that match the given filter.
    ///
    /// This is similar to [`Self::trace_block`] but only returns traces for transactions that match
    /// the filter.
    pub async fn trace_filter(
        &self,
        filter: TraceFilter,
    ) -> Result<Vec<LocalizedTransactionTrace>, Eth::Error> {
        // We'll reuse the matcher across multiple blocks that are traced in parallel
        let matcher = Arc::new(filter.matcher());
        let TraceFilter { from_block, to_block, after, count, .. } = filter;
        let start = from_block.unwrap_or(0);

        let latest_block = self.provider().best_block_number().map_err(Eth::Error::from_eth_err)?;
        if start > latest_block {
            // can't trace that range
            return Err(EthApiError::HeaderNotFound(start.into()).into());
        }
        let end = to_block.unwrap_or(latest_block);

        if start > end {
            return Err(EthApiError::InvalidParams(
                "invalid parameters: fromBlock cannot be greater than toBlock".to_string(),
            )
            .into())
        }

        // ensure that the range is not too large, since we need to fetch all blocks in the range
        let distance = end.saturating_sub(start);
        if distance > self.inner.eth_config.max_trace_filter_blocks {
            return Err(EthApiError::InvalidParams(
                "Block range too large; currently limited to 100 blocks".to_string(),
            )
            .into())
        }

        // fetch all blocks in that range
        let blocks = self
            .provider()
            .recovered_block_range(start..=end)
            .map_err(Eth::Error::from_eth_err)?
            .into_iter()
            .map(Arc::new)
            .collect::<Vec<_>>();

        // trace all blocks
        let mut block_traces = Vec::with_capacity(blocks.len());
        for block in &blocks {
            let matcher = matcher.clone();
            let traces = self.eth_api().trace_block_until(
                block.hash().into(),
                Some(block.clone()),
                None,
                TracingInspectorConfig::default_parity(),
                move |tx_info, ctx| {
                    let mut traces = ctx
                        .inspector
                        .into_parity_builder()
                        .into_localized_transaction_traces(tx_info);
                    traces.retain(|trace| matcher.matches(&trace.trace));
                    Ok(Some(traces))
                },
            );
            block_traces.push(traces);
        }

        let block_traces = futures::future::try_join_all(block_traces).await?;
        let mut all_traces = block_traces
            .into_iter()
            .flatten()
            .flat_map(|traces| traces.into_iter().flatten().flat_map(|traces| traces.into_iter()))
            .collect::<Vec<_>>();

        // add reward traces for all blocks
        for block in &blocks {
            if let Some(base_block_reward) = self.calculate_base_block_reward(block.header())? {
                all_traces.extend(
                    self.extract_reward_traces(
                        block.header(),
                        block.body().ommers(),
                        base_block_reward,
                    )
                    .into_iter()
                    .filter(|trace| matcher.matches(&trace.trace)),
                );
            } else {
                // no block reward, means we're past the Paris hardfork and don't expect any rewards
                // because the blocks in ascending order
                break
            }
        }

        // Skips the first `after` number of matching traces.
        // If `after` is greater than or equal to the number of matched traces, it returns an empty
        // array.
        if let Some(after) = after.map(|a| a as usize) {
            if after < all_traces.len() {
                all_traces.drain(..after);
            } else {
                return Ok(vec![])
            }
        }

        // Return at most `count` of traces
        if let Some(count) = count {
            let count = count as usize;
            if count < all_traces.len() {
                all_traces.truncate(count);
            }
        };

        Ok(all_traces)
    }

    /// Returns traces created at given block.
    pub async fn trace_block(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Vec<LocalizedTransactionTrace>>, Eth::Error> {
        let traces = self.eth_api().trace_block_with(
            block_id,
            None,
            TracingInspectorConfig::default_parity(),
            |tx_info, ctx| {
                let traces =
                    ctx.inspector.into_parity_builder().into_localized_transaction_traces(tx_info);
                Ok(traces)
            },
        );

        let block = self.eth_api().recovered_block(block_id);
        let (maybe_traces, maybe_block) = futures::try_join!(traces, block)?;

        let mut maybe_traces =
            maybe_traces.map(|traces| traces.into_iter().flatten().collect::<Vec<_>>());

        if let (Some(block), Some(traces)) = (maybe_block, maybe_traces.as_mut()) {
            if let Some(base_block_reward) = self.calculate_base_block_reward(block.header())? {
                traces.extend(self.extract_reward_traces(
                    block.header(),
                    block.body().ommers(),
                    base_block_reward,
                ));
            }
        }

        Ok(maybe_traces)
    }

    /// Replays all transactions in a block
    pub async fn replay_block_transactions(
        &self,
        block_id: BlockId,
        trace_types: HashSet<TraceType>,
    ) -> Result<Option<Vec<TraceResultsWithTransactionHash>>, Eth::Error> {
        self.eth_api()
            .trace_block_with(
                block_id,
                None,
                TracingInspectorConfig::from_parity_config(&trace_types),
                move |tx_info, ctx| {
                    let mut full_trace = ctx
                        .inspector
                        .into_parity_builder()
                        .into_trace_results(&ctx.result, &trace_types);

                    // If statediffs were requested, populate them with the account balance and
                    // nonce from pre-state
                    if let Some(ref mut state_diff) = full_trace.state_diff {
                        populate_state_diff(state_diff, &ctx.db, ctx.state.iter())
                            .map_err(Eth::Error::from_eth_err)?;
                    }

                    let trace = TraceResultsWithTransactionHash {
                        transaction_hash: tx_info.hash.expect("tx hash is set"),
                        full_trace,
                    };
                    Ok(trace)
                },
            )
            .await
    }

    /// Returns the opcodes of all transactions in the given block.
    ///
    /// This is the same as [`Self::trace_transaction_opcode_gas`] but for all transactions in a
    /// block.
    pub async fn trace_block_opcode_gas(
        &self,
        block_id: BlockId,
    ) -> Result<Option<BlockOpcodeGas>, Eth::Error> {
        let res = self
            .eth_api()
            .trace_block_inspector(
                block_id,
                None,
                OpcodeGasInspector::default,
                move |tx_info, ctx| {
                    let trace = TransactionOpcodeGas {
                        transaction_hash: tx_info.hash.expect("tx hash is set"),
                        opcode_gas: ctx.inspector.opcode_gas_iter().collect(),
                    };
                    Ok(trace)
                },
            )
            .await?;

        let Some(transactions) = res else { return Ok(None) };

        let Some(block) = self.eth_api().recovered_block(block_id).await? else { return Ok(None) };

        Ok(Some(BlockOpcodeGas {
            block_hash: block.hash(),
            block_number: block.number(),
            transactions,
        }))
    }
}

#[async_trait]
impl<Eth> TraceApiServer for TraceApi<Eth>
where
    Eth: TraceExt + 'static,
{
    /// Executes the given call and returns a number of possible traces for it.
    ///
    /// Handler for `trace_call`
    async fn trace_call(
        &self,
        call: TransactionRequest,
        trace_types: HashSet<TraceType>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<TraceResults> {
        let _permit = self.acquire_trace_permit().await;
        let request =
            TraceCallRequest { call, trace_types, block_id, state_overrides, block_overrides };
        Ok(Self::trace_call(self, request).await.map_err(Into::into)?)
    }

    /// Handler for `trace_callMany`
    async fn trace_call_many(
        &self,
        calls: Vec<(TransactionRequest, HashSet<TraceType>)>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<TraceResults>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_call_many(self, calls, block_id).await.map_err(Into::into)?)
    }

    /// Handler for `trace_rawTransaction`
    async fn trace_raw_transaction(
        &self,
        data: Bytes,
        trace_types: HashSet<TraceType>,
        block_id: Option<BlockId>,
    ) -> RpcResult<TraceResults> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_raw_transaction(self, data, trace_types, block_id)
            .await
            .map_err(Into::into)?)
    }

    /// Handler for `trace_replayBlockTransactions`
    async fn replay_block_transactions(
        &self,
        block_id: BlockId,
        trace_types: HashSet<TraceType>,
    ) -> RpcResult<Option<Vec<TraceResultsWithTransactionHash>>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::replay_block_transactions(self, block_id, trace_types)
            .await
            .map_err(Into::into)?)
    }

    /// Handler for `trace_replayTransaction`
    async fn replay_transaction(
        &self,
        transaction: B256,
        trace_types: HashSet<TraceType>,
    ) -> RpcResult<TraceResults> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::replay_transaction(self, transaction, trace_types).await.map_err(Into::into)?)
    }

    /// Handler for `trace_block`
    async fn trace_block(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_block(self, block_id).await.map_err(Into::into)?)
    }

    /// Handler for `trace_filter`
    ///
    /// This is similar to `eth_getLogs` but for traces.
    ///
    /// # Limitations
    /// This currently requires block filter fields, since reth does not have address indices yet.
    async fn trace_filter(&self, filter: TraceFilter) -> RpcResult<Vec<LocalizedTransactionTrace>> {
        Ok(Self::trace_filter(self, filter).await.map_err(Into::into)?)
    }

    /// Returns transaction trace at given index.
    /// Handler for `trace_get`
    async fn trace_get(
        &self,
        hash: B256,
        indices: Vec<Index>,
    ) -> RpcResult<Option<LocalizedTransactionTrace>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_get(self, hash, indices.into_iter().map(Into::into).collect())
            .await
            .map_err(Into::into)?)
    }

    /// Handler for `trace_transaction`
    async fn trace_transaction(
        &self,
        hash: B256,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_transaction(self, hash).await.map_err(Into::into)?)
    }

    /// Handler for `trace_transactionOpcodeGas`
    async fn trace_transaction_opcode_gas(
        &self,
        tx_hash: B256,
    ) -> RpcResult<Option<TransactionOpcodeGas>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_transaction_opcode_gas(self, tx_hash).await.map_err(Into::into)?)
    }

    /// Handler for `trace_blockOpcodeGas`
    async fn trace_block_opcode_gas(&self, block_id: BlockId) -> RpcResult<Option<BlockOpcodeGas>> {
        let _permit = self.acquire_trace_permit().await;
        Ok(Self::trace_block_opcode_gas(self, block_id).await.map_err(Into::into)?)
    }
}

impl<Eth> std::fmt::Debug for TraceApi<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceApi").finish_non_exhaustive()
    }
}
impl<Eth> Clone for TraceApi<Eth> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

struct TraceApiInner<Eth> {
    /// Access to commonly used code of the `eth` namespace
    eth_api: Eth,
    // restrict the number of concurrent calls to `trace_*`
    blocking_task_guard: BlockingTaskGuard,
    // eth config settings
    eth_config: EthConfig,
}

/// Helper to construct a [`LocalizedTransactionTrace`] that describes a reward to the block
/// beneficiary.
fn reward_trace<H: BlockHeader>(header: &H, reward: RewardAction) -> LocalizedTransactionTrace {
    LocalizedTransactionTrace {
        block_hash: Some(header.hash_slow()),
        block_number: Some(header.number()),
        transaction_hash: None,
        transaction_position: None,
        trace: TransactionTrace {
            trace_address: vec![],
            subtraces: 0,
            action: Action::Reward(reward),
            error: None,
            result: None,
        },
    }
}
