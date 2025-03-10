//! Contains RPC handler implementations specific to endpoints that call/execute within evm.

use crate::{
    eth::{
        error::{ensure_success, EthApiError, EthResult, RevertError, RpcInvalidTransactionError},
        revm_utils::{
            build_call_evm_env, caller_gas_allowance, cap_tx_gas_limit_with_caller_allowance,
            get_precompiles, inspect, prepare_call_env, transact, EvmOverrides,
        },
        EthTransactions,
    },
    EthApi,
};
use ethers_core::utils::get_contract_address;
use reth_network_api::NetworkInfo;
use reth_primitives::{AccessList, BlockId, BlockNumberOrTag, Bytes, U256};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, EvmEnvProvider, StateProvider, StateProviderFactory,
};
use reth_revm::{
    access_list::AccessListInspector,
    database::{State, SubState},
    env::tx_env_with_recovered,
};
use reth_rpc_types::{
    state::StateOverride, BlockError, Bundle, CallRequest, EthCallResponse, StateContext,
};
use reth_transaction_pool::TransactionPool;
use revm::{
    db::{CacheDB, DatabaseRef},
    primitives::{BlockEnv, CfgEnv, Env, ExecutionResult, Halt, TransactTo},
    DatabaseCommit,
};
use tracing::trace;

// Gas per transaction not creating a contract.
const MIN_TRANSACTION_GAS: u64 = 21_000u64;
const MIN_CREATE_GAS: u64 = 53_000u64;

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + Clone + 'static,
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
{
    /// Estimate gas needed for execution of the `request` at the [BlockId].
    pub async fn estimate_gas_at(&self, request: CallRequest, at: BlockId) -> EthResult<U256> {
        let (cfg, block_env, at) = self.evm_env_at(at).await?;
        let state = self.state_at(at)?;
        self.estimate_gas_with(cfg, block_env, request, state)
    }

    /// Executes the call request (`eth_call`) and returns the output
    pub async fn call(
        &self,
        request: CallRequest,
        block_number: Option<BlockId>,
        overrides: EvmOverrides,
    ) -> EthResult<Bytes> {
        let (res, _env) = self
            .transact_call_at(
                request,
                block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)),
                overrides,
            )
            .await?;

        ensure_success(res.result)
    }

    /// Simulate arbitrary number of transactions at an arbitrary blockchain index, with the
    /// optionality of state overrides
    pub async fn call_many(
        &self,
        bundle: Bundle,
        state_context: Option<StateContext>,
        mut state_override: Option<StateOverride>,
    ) -> EthResult<Vec<EthCallResponse>> {
        let Bundle { transactions, block_override } = bundle;
        if transactions.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("transactions are empty.")))
        }

        let StateContext { transaction_index, block_number } = state_context.unwrap_or_default();
        let transaction_index = transaction_index.unwrap_or_default();

        let target_block = block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));
        let ((cfg, block_env, _), block) =
            futures::try_join!(self.evm_env_at(target_block), self.block_by_id(target_block))?;

        let block = block.ok_or_else(|| EthApiError::UnknownBlockNumber)?;
        let gas_limit = self.inner.gas_cap;

        // we're essentially replaying the transactions in the block here, hence we need the state
        // that points to the beginning of the block, which is the state at the parent block
        let mut at = block.parent_hash;
        let mut replay_block_txs = true;

        // but if all transactions are to be replayed, we can use the state at the block itself
        let num_txs = transaction_index.index().unwrap_or(block.body.len());
        if num_txs == block.body.len() {
            at = block.hash;
            replay_block_txs = false;
        }

        self.spawn_with_state_at_block(at.into(), move |state| {
            let mut results = Vec::with_capacity(transactions.len());
            let mut db = SubState::new(State::new(state));

            if replay_block_txs {
                // only need to replay the transactions in the block if not all transactions are
                // to be replayed
                let transactions = block.body.into_iter().take(num_txs);

                // Execute all transactions until index
                for tx in transactions {
                    let tx = tx.into_ecrecovered().ok_or(BlockError::InvalidSignature)?;
                    let tx = tx_env_with_recovered(&tx);
                    let env = Env { cfg: cfg.clone(), block: block_env.clone(), tx };
                    let (res, _) = transact(&mut db, env)?;
                    db.commit(res.state);
                }
            }

            let block_overrides = block_override.map(Box::new);

            let mut transactions = transactions.into_iter().peekable();
            while let Some(tx) = transactions.next() {
                // apply state overrides only once, before the first transaction
                let state_overrides = state_override.take();
                let overrides = EvmOverrides::new(state_overrides, block_overrides.clone());

                let env = prepare_call_env(
                    cfg.clone(),
                    block_env.clone(),
                    tx,
                    gas_limit,
                    &mut db,
                    overrides,
                )?;
                let (res, _) = transact(&mut db, env)?;

                match ensure_success(res.result) {
                    Ok(output) => {
                        results.push(EthCallResponse { output: Some(output), error: None });
                    }
                    Err(err) => {
                        results
                            .push(EthCallResponse { output: None, error: Some(err.to_string()) });
                    }
                }

                if transactions.peek().is_some() {
                    // need to apply the state changes of this call before executing the next call
                    db.commit(res.state);
                }
            }

            Ok(results)
        })
        .await
    }

    /// Estimates the gas usage of the `request` with the state.
    ///
    /// This will execute the [CallRequest] and find the best gas limit via binary search
    fn estimate_gas_with<S>(
        &self,
        mut cfg: CfgEnv,
        block: BlockEnv,
        request: CallRequest,
        state: S,
    ) -> EthResult<U256>
    where
        S: StateProvider,
    {
        // Disabled because eth_estimateGas is sometimes used with eoa senders
        // See <htps://github.com/paradigmxyz/reth/issues/1959>
        cfg.disable_eip3607 = true;

        // The basefee should be ignored for eth_createAccessList
        // See:
        // <https://github.com/ethereum/go-ethereum/blob/ee8e83fa5f6cb261dad2ed0a7bbcde4930c41e6c/internal/ethapi/api.go#L985>
        cfg.disable_base_fee = true;

        // keep a copy of gas related request values
        let request_gas = request.gas;
        let request_gas_price = request.gas_price;
        let env_gas_limit = block.gas_limit;

        // get the highest possible gas limit, either the request's set value or the currently
        // configured gas limit
        let mut highest_gas_limit = request.gas.unwrap_or(block.gas_limit);

        // Configure the evm env
        let mut env = build_call_evm_env(cfg, block, request)?;
        let mut db = SubState::new(State::new(state));

        // if the request is a simple transfer we can optimize
        if env.tx.data.is_empty() {
            if let TransactTo::Call(to) = env.tx.transact_to {
                if let Ok(code) = db.db.state().account_code(to) {
                    let no_code_callee = code.map(|code| code.is_empty()).unwrap_or(true);
                    if no_code_callee {
                        // simple transfer, check if caller has sufficient funds
                        let available_funds =
                            db.basic(env.tx.caller)?.map(|acc| acc.balance).unwrap_or_default();
                        if env.tx.value > available_funds {
                            return Err(
                                RpcInvalidTransactionError::InsufficientFundsForTransfer.into()
                            )
                        }
                        return Ok(U256::from(MIN_TRANSACTION_GAS))
                    }
                }
            }
        }

        // check funds of the sender
        if env.tx.gas_price > U256::ZERO {
            let allowance = caller_gas_allowance(&mut db, &env.tx)?;

            if highest_gas_limit > allowance {
                // cap the highest gas limit by max gas caller can afford with given gas price
                highest_gas_limit = allowance;
            }
        }

        // if the provided gas limit is less than computed cap, use that
        let gas_limit = std::cmp::min(U256::from(env.tx.gas_limit), highest_gas_limit);
        env.block.gas_limit = gas_limit;

        trace!(target: "rpc::eth::estimate", ?env, "Starting gas estimation");

        // execute the call without writing to db
        let ethres = transact(&mut db, env.clone());

        // Exceptional case: init used too much gas, we need to increase the gas limit and try
        // again
        if let Err(EthApiError::InvalidTransaction(RpcInvalidTransactionError::GasTooHigh)) = ethres
        {
            // if price or limit was included in the request then we can execute the request
            // again with the block's gas limit to check if revert is gas related or not
            if request_gas.is_some() || request_gas_price.is_some() {
                return Err(map_out_of_gas_err(env_gas_limit, env, &mut db))
            }
        }

        let (res, env) = ethres?;
        match res.result {
            ExecutionResult::Success { .. } => {
                // succeeded
            }
            ExecutionResult::Halt { reason, gas_used } => {
                return Err(RpcInvalidTransactionError::halt(reason, gas_used).into())
            }
            ExecutionResult::Revert { output, .. } => {
                // if price or limit was included in the request then we can execute the request
                // again with the block's gas limit to check if revert is gas related or not
                return if request_gas.is_some() || request_gas_price.is_some() {
                    Err(map_out_of_gas_err(env_gas_limit, env, &mut db))
                } else {
                    // the transaction did revert
                    Err(RpcInvalidTransactionError::Revert(RevertError::new(output)).into())
                }
            }
        }

        // at this point we know the call succeeded but want to find the _best_ (lowest) gas the
        // transaction succeeds with. we  find this by doing a binary search over the
        // possible range NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed
        let gas_used = res.result.gas_used();
        // the lowest value is capped by the gas it takes for a transfer
        let mut lowest_gas_limit =
            if env.tx.transact_to.is_create() { MIN_CREATE_GAS } else { MIN_TRANSACTION_GAS };
        let mut highest_gas_limit: u64 = highest_gas_limit.try_into().unwrap_or(u64::MAX);
        // pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        trace!(target: "rpc::eth::estimate", ?env, ?highest_gas_limit, ?lowest_gas_limit, ?mid_gas_limit, "Starting binary search for gas");

        // binary search
        while (highest_gas_limit - lowest_gas_limit) > 1 {
            let mut env = env.clone();
            env.tx.gas_limit = mid_gas_limit;
            let ethres = transact(&mut db, env);

            // Exceptional case: init used too much gas, we need to increase the gas limit and try
            // again
            if let Err(EthApiError::InvalidTransaction(RpcInvalidTransactionError::GasTooHigh)) =
                ethres
            {
                // increase the lowest gas limit
                lowest_gas_limit = mid_gas_limit;

                // new midpoint
                mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
                continue
            }

            let (res, _) = ethres?;
            match res.result {
                ExecutionResult::Success { .. } => {
                    // cap the highest gas limit with succeeding gas limit
                    highest_gas_limit = mid_gas_limit;
                }
                ExecutionResult::Revert { .. } => {
                    // increase the lowest gas limit
                    lowest_gas_limit = mid_gas_limit;
                }
                ExecutionResult::Halt { reason, .. } => {
                    match reason {
                        Halt::OutOfGas(_) => {
                            // increase the lowest gas limit
                            lowest_gas_limit = mid_gas_limit;
                        }
                        err => {
                            // these should be unreachable because we know the transaction succeeds,
                            // but we consider these cases an error
                            return Err(RpcInvalidTransactionError::EvmHalt(err).into())
                        }
                    }
                }
            }
            // new midpoint
            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }

    pub(crate) async fn create_access_list_at(
        &self,
        request: CallRequest,
        at: Option<BlockId>,
    ) -> EthResult<AccessList> {
        let block_id = at.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));
        let (cfg, block, at) = self.evm_env_at(block_id).await?;
        let state = self.state_at(at)?;

        let mut env = build_call_evm_env(cfg, block, request.clone())?;

        // we want to disable this in eth_createAccessList, since this is common practice used by
        // other node impls and providers <https://github.com/foundry-rs/foundry/issues/4388>
        env.cfg.disable_block_gas_limit = true;

        // The basefee should be ignored for eth_createAccessList
        // See:
        // <https://github.com/ethereum/go-ethereum/blob/8990c92aea01ca07801597b00c0d83d4e2d9b811/internal/ethapi/api.go#L1476-L1476>
        env.cfg.disable_base_fee = true;

        let mut db = SubState::new(State::new(state));

        if request.gas.is_none() && env.tx.gas_price > U256::ZERO {
            // no gas limit was provided in the request, so we need to cap the request's gas limit
            cap_tx_gas_limit_with_caller_allowance(&mut db, &mut env.tx)?;
        }

        let from = request.from.unwrap_or_default();
        let to = if let Some(to) = request.to {
            to
        } else {
            let nonce = db.basic(from)?.unwrap_or_default().nonce;
            get_contract_address(from, nonce).into()
        };

        let initial = request.access_list.clone().unwrap_or_default();

        let precompiles = get_precompiles(&env.cfg.spec_id);
        let mut inspector = AccessListInspector::new(initial, from, to, precompiles);
        let (result, _env) = inspect(&mut db, env, &mut inspector)?;

        match result.result {
            ExecutionResult::Halt { reason, .. } => Err(match reason {
                Halt::NonceOverflow => RpcInvalidTransactionError::NonceMaxValue,
                halt => RpcInvalidTransactionError::EvmHalt(halt),
            }),
            ExecutionResult::Revert { output, .. } => {
                Err(RpcInvalidTransactionError::Revert(RevertError::new(output)))
            }
            ExecutionResult::Success { .. } => Ok(()),
        }?;
        Ok(inspector.into_access_list())
    }
}

/// Executes the requests again after an out of gas error to check if the error is gas related or
/// not
#[inline]
fn map_out_of_gas_err<S>(
    env_gas_limit: U256,
    mut env: Env,
    mut db: &mut CacheDB<State<S>>,
) -> EthApiError
where
    S: StateProvider,
{
    let req_gas_limit = env.tx.gas_limit;
    env.tx.gas_limit = env_gas_limit.try_into().unwrap_or(u64::MAX);
    let (res, _) = match transact(&mut db, env) {
        Ok(res) => res,
        Err(err) => return err,
    };
    match res.result {
        ExecutionResult::Success { .. } => {
            // transaction succeeded by manually increasing the gas limit to
            // highest, which means the caller lacks funds to pay for the tx
            RpcInvalidTransactionError::BasicOutOfGas(U256::from(req_gas_limit)).into()
        }
        ExecutionResult::Revert { output, .. } => {
            // reverted again after bumping the limit
            RpcInvalidTransactionError::Revert(RevertError::new(output)).into()
        }
        ExecutionResult::Halt { reason, .. } => RpcInvalidTransactionError::EvmHalt(reason).into(),
    }
}
