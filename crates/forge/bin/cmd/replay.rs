use std::collections::BTreeMap;

use alloy_network::TransactionResponse;
use alloy_primitives::{Address, Bytes, TxHash, U128, U256};
use alloy_provider::Provider;
use alloy_rpc_types::{Block, BlockTransactions, BlockTransactionsKind, Transaction};
use eyre::{eyre, Context, Result};
use forge::{
    decode::decode_console_logs,
    executors::{DeployResult, EvmError, Executor, ExecutorBuilder, RawCallResult},
    revm::primitives::EnvWithHandlerCfg,
    traces::TraceKind,
    utils::configure_tx_env,
};
use foundry_cli::{
    opts::RpcOpts,
    utils::{handle_traces, init_progress, TraceResult},
};
use foundry_common::{is_known_system_sender, SYSTEM_TRANSACTION_TYPE};
use foundry_compilers::artifacts::EvmVersion;
use foundry_evm::opts::EvmOpts;

use clap::Parser;
use foundry_config::{find_project_root_path, Config};
use foundry_tweak::{build_tweaked_backend, ClonedProject, TweakData, TweakInfo};

/// Replays an on-chain historical transaction locally on a fork of the blockchain with the on-chain
/// contract tweaked by the current cloned project. In other words, `forge replay`:
/// 1. Fork the blockchain, and replace the code of the on-chain contract associated with the
///    current cloned project (address defined in `.clone.meta`).
/// 2. Replays the historical transaction on the forked blockchain.
///
/// NOTE: `forge replay` can only be used on a `forge clone`d project, which contains the metadata
/// of the on-chain instance of the contract.
#[derive(Clone, Debug, Parser, Default)]
pub struct ReplayArgs {
    #[arg()]
    transaction: String,

    /// Executes the transaction only with the state from the previous block.
    /// Note that this also include transactions that are used for tweaking code.
    ///
    /// May result in different results than the live execution!
    #[arg(long, short)]
    quick: bool,

    /// Sets the number of assumed available compute units per second for this provider
    ///
    /// default value: 330
    ///
    /// See also, https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second
    #[arg(long, alias = "cups", value_name = "CUPS")]
    pub compute_units_per_second: Option<u64>,

    /// Disables rate limiting for this node's provider.
    ///
    /// default value: false
    ///
    /// See also, https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second
    #[arg(long, value_name = "NO_RATE_LIMITS", visible_alias = "no-rpc-rate-limit")]
    pub no_rate_limit: bool,

    /// Overrides the gas limit for the transaction.
    #[arg(
        long,
        short,
        value_name = "GAS",
        help = "Override the gas limit for the transaction. If not set, two times of the gas limit from the transaction is used."
    )]
    pub gas: Option<u64>,

    /// Overrides the gas price (or base fee in EIP1559) used for the transaction
    #[arg(
        long,
        value_name = "GAS_PRICE",
        help = "Override the gas price for the transaction. If not set, the gas price from the transaction is used."
    )]
    pub gas_price: Option<U128>,

    /// The EVM version to use.
    ///
    /// Overrides the version specified in the config.
    #[arg(long, short)]
    evm_version: Option<EvmVersion>,

    #[command(flatten)]
    pub rpc: RpcOpts,
}

impl ReplayArgs {
    /// Runs the `forge replay` command.
    /// Logic is akin to the `cast run` command.
    pub async fn run(mut self) -> Result<()> {
        self.rpc.url = self.rpc.url.or(Some("http://localhost:8545".to_string()));
        let root = find_project_root_path(None).unwrap();
        let root = dunce::canonicalize(root).expect("failed to convert to absolute path");
        let cloned_project = ClonedProject::load_with_root(&root)
            .map_err(|e| eyre!("failed to load the cloned project: {}", e))?;
        let tweaked_addr = cloned_project.metadata.address;
        let tweaked_code = cloned_project.tweaked_code(&self.rpc, self.quick).await?;
        let tweaked_abi = cloned_project.get_abi()?;
        let tweaked_name = cloned_project.metadata.target_contract.clone();

        let figment = Config::figment_with_root(&root).merge(&self.rpc);
        let evm_opts = figment.extract::<EvmOpts>()?;
        let mut config = Config::try_from(figment)?.sanitized();
        config.evm_version = self.evm_version.unwrap_or_default();
        info!("config.evm_version: {:?}", config.evm_version);
        let compute_units_per_second =
            if self.no_rate_limit { Some(u64::MAX) } else { self.compute_units_per_second };

        let tx_hash: TxHash = self.transaction.parse().wrap_err("invalid transaction hash")?;
        
        // Build TweakData with bytecode, ABI, and name
        let tweak_info = TweakInfo {
            bytecode: tweaked_code,
            abi: tweaked_abi,
            name: tweaked_name,
        };
        let tweak_data: TweakData = vec![(tweaked_addr, tweak_info)].into_iter().collect();
        
        let r = self
            .replay_tx_hash(
                &config,
                &evm_opts,
                tx_hash,
                &tweak_data,
                compute_units_per_second,
            )
            .await?;
        let console_logs = r.console_logs();
        // print call trace
        let r = r.trace_result()?;
        handle_traces(r, &config, config.chain, vec![], false).await?;

        // print logs if any
        if !console_logs.is_empty() {
            println!("Logs:");
            for log in console_logs {
                println!("  {log}");
            }
            println!();
        }

        Ok(())
    }

    async fn replay_tx_hash(
        &self,
        config: &Config,
        evm_opts: &EvmOpts,
        tx_hash: TxHash,
        tweaks: &TweakData,
        compute_units_per_second: Option<u64>,
    ) -> Result<ExecuteResult> {
        let quick = self.quick;
        let gas = self.gas;
        let gas_price = self.gas_price;

        let mut config = config.clone();

        // construct JSON-RPC provider
        let provider = foundry_common::provider::ProviderBuilder::new(
            &config.get_rpc_url_or_localhost_http()?,
        )
        .compute_units_per_second_opt(compute_units_per_second)
        .build()?;

        // get transactiond data
        let mut tx = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .unwrap()
            .ok_or(eyre::eyre!("no such transactikon"))?;
        let tx_block_number: u64 =
            tx.block_number.ok_or(eyre!("transaction may still be pending"))?;
        // If the gas is not specified, we use 2 times of the gas limit from the transaction
        tx.gas = gas.map(|x| x as u128).unwrap_or(tx.gas.saturating_add(tx.gas));
        // Set the gas price if specified
        tx.gas_price = gas_price.map(|x| x.to()).or(tx.gas_price);

        // get preceeding transactions in the same block
        let block =
            provider.get_block(tx_block_number.into(), BlockTransactionsKind::Full).await.unwrap();
        let block = block.ok_or(eyre::eyre!("block not found"))?;

        config.fork_block_number = Some(tx_block_number - 1); // fork from the previous block

        // build the executor
        let mut evm_opts = evm_opts.clone();
        evm_opts.fork_url = Some(config.get_rpc_url_or_localhost_http()?.into_owned());
        info!("evm_opts.fork_url: {:?}", evm_opts.fork_url);
        evm_opts.fork_block_number = config.fork_block_number;
        info!("evm_opts.fork_block_number: {:?}", evm_opts.fork_block_number);
        let env = evm_opts.evm_env().await?;
        let fork = evm_opts.get_fork(&config, env.clone());
        let backend = build_tweaked_backend(fork, tweaks)?;
        let mut executor = ExecutorBuilder::new()
            .inspectors(|stack| stack.trace(true))
            .spec(config.evm_spec_id())
            .build(env, backend);

        // set the state to the moment right before the transaction
        // we execute all transactions before the target transaction
        let mut env = executor.env_with_handler_cfg().clone();
        adjust_block_env(&mut env, &block);
        env.block.number = U256::from(tx_block_number);
        let BlockTransactions::Full(txs) = block.transactions else {
            return Err(eyre::eyre!("block transactions not found"));
        };
        if !quick {
            trace!("Executing transactions before the target transaction in the same block...");
            let txs = txs
                .into_iter()
                .take_while(|tx: &Transaction| tx.tx_hash() != tx_hash)
                .collect::<Vec<_>>();
            let pb = init_progress(txs.len() as u64, "replaying preceeding txs");
            pb.set_position(0);
            for (index, tx) in txs.into_iter().enumerate() {
                pb.set_position(index as u64 + 1);

                // System transactions such as on L2s don't contain any pricing info so
                // we skip them otherwise this would cause
                // reverts
                if is_known_system_sender(tx.from) ||
                    tx.transaction_type == Some(SYSTEM_TRANSACTION_TYPE)
                {
                    continue;
                }

                if tx.hash == tx_hash {
                    // we reach the target transaction
                    break;
                }

                // execute the transaction
                trace!("Executing transaction: {:?}", tx.hash);
                let _ = execute_tx(&mut executor, env.clone(), &tx)?;
            }
        }

        // execute the target transaction
        trace!("Executing target transaction: {:?}", tx.hash);
        let r = execute_tx(&mut executor, env, &tx)?;
        Ok(r)
    }
}

fn adjust_block_env(env: &mut EnvWithHandlerCfg, block: &Block) {
    env.block.timestamp = U256::from(block.header.timestamp);
    env.block.coinbase = block.header.miner;
    env.block.difficulty = block.header.difficulty;
    env.block.prevrandao = Some(block.header.mix_hash.unwrap_or_default());
    env.block.basefee = U256::from(block.header.base_fee_per_gas.unwrap_or_default());
    env.block.gas_limit = U256::from(block.header.gas_limit);
    if let Some(excess_blob_gas) = block.header.excess_blob_gas {
        env.block.set_blob_excess_gas_and_price(excess_blob_gas as u64);
    }
}

enum ExecuteResult {
    Call(RawCallResult),
    Create(DeployResult),
    Revert(EvmError),
}

impl ExecuteResult {
    pub fn trace_result(self) -> Result<TraceResult> {
        match self {
            Self::Call(r) => Ok(TraceResult::from_raw(r, TraceKind::Execution)),
            Self::Create(r) => Ok(TraceResult::from(r)),
            Self::Revert(e) => TraceResult::try_from(Err(e)).map_err(|e| eyre!("revert: {}", e)),
        }
    }

    pub fn console_logs(&self) -> Vec<String> {
        let raw_logs = match self {
            Self::Call(r) => &r.logs,
            Self::Create(r) => &r.raw.logs,
            Self::Revert(EvmError::Execution(e)) => &e.raw.logs,
            _ => return vec![],
        };
        decode_console_logs(raw_logs)
    }
}

fn execute_tx(
    executor: &mut Executor,
    mut env: EnvWithHandlerCfg,
    tx: &Transaction,
) -> Result<ExecuteResult> {
    configure_tx_env(&mut env, tx);
    // in case users overrides gas price below EIP1559 base fee, we disable base fee for the
    // transaction
    env.cfg.disable_block_gas_limit = true;
    // in case users overrides gas price below the original gas price, we reset the gas priority fee
    if env.tx.gas_price.lt(&env.tx.gas_priority_fee.unwrap_or_default()) {
        env.tx.gas_priority_fee = Some(U256::ZERO);
    }
    // disable base fee when necessary
    if env
        .tx
        .gas_price
        .saturating_sub(env.tx.gas_priority_fee.unwrap_or_default())
        .lt(&env.block.basefee)
    {
        env.cfg.disable_base_fee = true;
    }

    info!("print env details");
    info!("print env.env.cfg.chain_id: {:?}", env.env.cfg.chain_id);
    info!("print env.env.block.number: {:?}", env.env.block.number);
    info!("print env.env.block.timestamp: {:?}", env.env.block.timestamp);
    info!("print env.env.block.difficulty: {:?}", env.env.block.difficulty);
    info!("print env.env.block.prevrandao: {:?}", env.env.block.prevrandao);
    info!("print env.env.block.basefee: {:?}", env.env.block.basefee);
    info!("print env.env.block.gas_limit: {:?}", env.env.block.gas_limit);
    info!("print env.env.block.coinbase: {:?}", env.env.block.coinbase);
    if tx.to.is_some() {
        let r = executor.transact_with_env(env.clone()).wrap_err_with(|| {
            format!("Failed to execute transaction: {:?} in block {}", tx.hash, env.block.number)
        })?;
        Ok(ExecuteResult::Call(r))
    } else {
        let r = executor.deploy_with_env(env, None);
        match r {
            Ok(r) => Ok(ExecuteResult::Create(r)),
            Err(e) => Ok(ExecuteResult::Revert(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Address, Bytes, TxHash};
    use foundry_cli::utils::handle_traces;
    use foundry_compilers::artifacts::EvmVersion;
    use foundry_config::{figment::Figment, find_project_root_path, Config};
    use foundry_evm::opts::EvmOpts;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_replay_tx_hash() {
        let rpc = std::env::var("ETH_RPC_URL").unwrap_or("http://localhost:8545".to_string());
        let tx: TxHash =
            "0xbfa440cd7df20320fe8400e4f61113379a018e3904eef7cf6085cf6cf22bcdb9".parse().unwrap();

        let figment = Config::figment_with_root(find_project_root_path(None).unwrap());
        let evm_opts = figment.extract::<EvmOpts>().unwrap();
        let mut config = Config::try_from(figment).unwrap().sanitized();
        config.eth_rpc_url = Some(rpc);
        config.evm_version = EvmVersion::Shanghai;

        let args =
            super::ReplayArgs { quick: false, gas: None, gas_price: None, ..Default::default() };
        let r =
            args.replay_tx_hash(&config, &evm_opts, tx, &TweakData::default(), None).await.unwrap();
        let super::ExecuteResult::Call(result) = &r else {
            panic!("expected ExecuteResult::Call");
        };
        assert!(!result.reverted);
        assert_eq!(result.gas_used, 163_955);
        let r = r.trace_result().unwrap();
        handle_traces(r, &config, config.chain, vec![], false).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_replay_tx_hash_with_tweak() {
        let rpc = std::env::var("ETH_RPC_URL").unwrap_or("http://localhost:8545".to_string());
        let tx: TxHash =
            "0xbfa440cd7df20320fe8400e4f61113379a018e3904eef7cf6085cf6cf22bcdb9".parse().unwrap();
        let factory: Address = "0x8d1fA935E5e8a5440c9Efc96C0d9fF387eBb179B".parse().unwrap();
        let tweaked_code: Bytes = "0xfe".parse().unwrap();

        let mut config = Config::default();
        let figment: Figment = config.clone().into();
        let evm_opts = figment.extract::<EvmOpts>().unwrap();
        config.eth_rpc_url = Some(rpc);

        let args =
            super::ReplayArgs { quick: false, gas: None, gas_price: None, ..Default::default() };

        // Create TweakData with minimal ABI for testing
        let tweak_info = TweakInfo {
            bytecode: tweaked_code,
            abi: Default::default(), // Empty ABI for test
            name: "TestContract".to_string(),
        };
        let tweak_data: TweakData = BTreeMap::from([(factory, tweak_info)]);

        let r = args
            .replay_tx_hash(
                &config,
                &evm_opts,
                tx,
                &tweak_data,
                None,
            )
            .await
            .unwrap();
        let r = r.trace_result().unwrap();
        assert!(!r.success);
        handle_traces(r, &config, config.chain, vec![], false).await.unwrap();
    }
}
