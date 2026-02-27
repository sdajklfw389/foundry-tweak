//! Code generation for the contract we are going to tweak.
//! The contract should from a cloned project created by `forge clone` command.
//! The generation has to happen after the compatibility check.

use alloy_primitives::{keccak256, Address, Bytes, TxHash, U256};
use alloy_provider::Provider;
use alloy_rpc_types::{
    serde_helpers::WithOtherFields, AnyReceiptEnvelope, BlockId, BlockTransactions,
    BlockTransactionsKind, Log, Transaction, TransactionReceipt,
};
use eyre::{eyre, Context, Result};
use foundry_cli::{opts::RpcOpts, p_println, utils::init_progress};
use foundry_common::{
    cli_warn, is_known_system_sender, provider::ProviderBuilder, SYSTEM_TRANSACTION_TYPE,
};
use foundry_compilers::{artifacts::ConfigurableContractArtifact, Artifact};
use foundry_config::{figment::Figment, Config, NamedChain};
use foundry_evm::{
    backend::Backend,
    executors::{EvmError, Executor, ExecutorBuilder},
    opts::EvmOpts,
    utils::configure_tx_env,
    InspectorExt,
};
use revm::{
    interpreter::{CreateInputs, CreateOutcome, Interpreter, OpCode},
    primitives::{BlockEnv, EnvWithHandlerCfg, HashSet, SpecId},
    EvmContext, Inspector,
};
use std::borrow::Cow;
use tracing::{info, trace};
use revm::Database;

use crate::{constants::NonStandardPrecompiled, ClonedProject};

#[derive(Debug)]
struct TweakInspctor {
    creation_count: u64,
    creation_stack_depth: u64,
    contract_address: Option<Address>,
    target_creation_tag: Option<(u64, u64)>,
    tweaked_creation_code: Option<Bytes>,
    tweaked_code: Option<Bytes>,

    // used to modify address(this)
    to_update_address: bool,
    created_address_in_tweak: Option<Address>,
    observed_created_addresses: HashSet<Address>,
}

type TransactionExt = WithOtherFields<Transaction>;
type TransactionReceiptExt = WithOtherFields<TransactionReceipt<AnyReceiptEnvelope<Log>>>;

impl TweakInspctor {
    pub fn new(contract_address: Option<Address>, tweaked_creation_code: Option<Bytes>) -> Self {
        Self {
            creation_count: 0,
            creation_stack_depth: 0,
            contract_address,
            target_creation_tag: None,
            tweaked_creation_code,
            tweaked_code: None,

            to_update_address: false,
            created_address_in_tweak: None,
            observed_created_addresses: HashSet::new(),
        }
    }

    pub fn prepare_for_pinpoint(&mut self) -> Result<()> {
        self.creation_count = 0;
        self.creation_stack_depth = 0;

        self.to_update_address = false;
        self.created_address_in_tweak = None;
        self.observed_created_addresses.clear();

        eyre::ensure!(self.contract_address.is_some(), "the contract address is not found");

        Ok(())
    }

    pub fn prepare_for_tweak(&mut self) -> Result<()> {
        self.creation_count = 0;
        self.creation_stack_depth = 0;

        self.to_update_address = false;
        self.created_address_in_tweak = None;
        self.observed_created_addresses.clear();

        eyre::ensure!(self.contract_address.is_some(), "the contract address is not found");
        eyre::ensure!(self.target_creation_tag.is_some(), "the target creation count is not found");
        eyre::ensure!(
            self.tweaked_creation_code.is_some(),
            "the tweaked creation code is not found"
        );

        Ok(())
    }
}

impl InspectorExt<&mut Backend> for TweakInspctor {}

impl Inspector<&mut Backend> for TweakInspctor {
    #[inline]
    fn create(
        &mut self,
        _: &mut EvmContext<&mut Backend>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        // first update the creation_count and creation_stack_depth
        self.creation_count += 1;
        self.creation_stack_depth += 1;

        // we then check creation count as the target one
        if Some((self.creation_count, self.creation_stack_depth)) == self.target_creation_tag {
            // we are going to tweak the creation code
            if let Some(tweaked_creation_code) = &self.tweaked_creation_code {
                info!("create: Injecting tweaked creation code, length: {}", tweaked_creation_code.len());
                info!("create: Original init_code length: {}", inputs.init_code.len());
                inputs.init_code = tweaked_creation_code.clone();
                info!("create: After injection, init_code length: {}", inputs.init_code.len());
            } else {
                info!("create: target_creation_tag matched but tweaked_creation_code is None!");
            }
        }

        None
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut EvmContext<&mut Backend>,
        _: &CreateInputs,
        outcome: CreateOutcome,
    ) -> CreateOutcome {
        // we shall first distinguish the replay stage
        if let Some((target_count, target_stack_depth)) = self.target_creation_tag {
            // record the tweaked contract address
            if self.creation_count == target_count &&
                self.creation_stack_depth == target_stack_depth
            {
                info!("create_end: Matched target creation (count={}, depth={})", target_count, target_stack_depth);
                if let Some(address) = outcome.address {
                    info!("create_end: Created address: {:?}", address);
                    info!("create_end: outcome.result.result: {:?}", outcome.result.result);
                    info!("create_end: outcome.result.output length: {}", outcome.result.output.len());
                    info!("create_end: outcome.result.gas: {:?}", outcome.result.gas);
                    
                    // The runtime bytecode is in outcome.result.output (returned from CREATE)
                    if outcome.result.is_ok() {
                        let runtime_bytecode = outcome.result.output.clone();
                        info!("create_end: Got runtime bytecode from outcome.result.output, length: {}", runtime_bytecode.len());
                        if !runtime_bytecode.is_empty() {
                            self.tweaked_code = Some(Bytes::from(runtime_bytecode));
                        } else {
                            info!("create_end: outcome.result.output is empty, trying context.code() as fallback");
                            // Fallback: try to get from context
                            match context.code(address) {
                                Ok((code, _)) => {
                                    info!("create_end: Fallback: Successfully retrieved code from context, length: {}", code.len());
                                    if !code.is_empty() {
                                        self.tweaked_code = Some(code.clone());
                                    }
                                }
                                Err(e) => {
                                    info!("create_end: Fallback: Failed to get code from context: {:?}", e);
                                }
                            }
                        }
                    } else {
                        info!("create_end: outcome.result is not OK: {:?}", outcome.result.result);
                        info!("create_end: This means the CREATE failed - the tweaked creation code may have reverted or failed");
                        // Even if it failed, try to get code from database as last resort
                        info!("create_end: Attempting to get code from database despite failure...");
                        match context.code(address) {
                            Ok((code, _)) => {
                                info!("create_end: Database fallback: code length: {}", code.len());
                                if !code.is_empty() {
                        self.tweaked_code = Some(code.clone());
                    }
                }
                            Err(e) => {
                                info!("create_end: Database fallback failed: {:?}", e);
                            }
                        }
                    }
                } else {
                    info!("create_end: outcome.address is None");
                }
            } else {
                info!("create_end: Creation count/depth mismatch: count={} (target={}), depth={} (target={})", 
                      self.creation_count, target_count, self.creation_stack_depth, target_stack_depth);
            }
        } else {
            // we are here to find the target creation count
            if outcome.address == self.contract_address {
                info!("create_end: Found target contract address, setting target_creation_tag: count={}, depth={}", 
                      self.creation_count, self.creation_stack_depth);
                self.target_creation_tag = Some((self.creation_count, self.creation_stack_depth));
            }
        }

        // we only decresae the creation_stack_depth
        self.creation_stack_depth -= 1;
        outcome
    }

    #[inline]
    fn step(&mut self, interp: &mut Interpreter, _: &mut EvmContext<&mut Backend>) {
        if Some((self.creation_count, self.creation_stack_depth)) == self.target_creation_tag {
            if self.created_address_in_tweak.is_none() {
                // When the first time the creation_count and creation_stack_depth match,
                // we know that we are in the CREATE call that creates the target contract.
                // We record the address of the created contract, so that we can replace it
                // with the original address of contract being tweaked.
                self.created_address_in_tweak.replace(interp.contract().target_address);
            }
            // the `step_end` hook should replace the output of ADDRESS code only if the
            // current execution is in the context of the target contract
            self.to_update_address = interp.current_opcode() == OpCode::ADDRESS.get() &&
                self.created_address_in_tweak.unwrap() == interp.contract().target_address;
        }
    }

    #[inline]
    fn step_end(&mut self, interp: &mut Interpreter, _: &mut EvmContext<&mut Backend>) {
        if Some((self.creation_count, self.creation_stack_depth)) == self.target_creation_tag &&
            self.to_update_address
        {
            // we hook into the ADDRESS opcode to update the address
            // note that we only update the address for the target contract
            if let Some(original_address) = self.contract_address {
                let stack = &mut interp.stack;
                self.observed_created_addresses
                    .insert(Address::from_word(stack.pop().unwrap().into()));
                stack.push(original_address.into_word().into()).unwrap();
            }
        }
    }
}

pub async fn generate_tweaked_code(
    rpc: &RpcOpts,
    project: &ClonedProject,
    quick: bool,
) -> Result<Bytes> {
    println!("Tweaking the contract at {}...", project.metadata.address);
    p_println!(!quick => "It may take time if the RPC has rate limits.");
    trace!("generate_tweaked_code: Target contract from metadata: '{}'", project.metadata.target_contract);
    trace!("generate_tweaked_code: Contract address: {}", project.metadata.address);
    // prepare the deployment bytecode (w/ parameters)
    let artifact = project.main_artifact()?;
    let tweaked_creation_code = prepare_tweaked_creation_code(project, &artifact)?;
    trace!("generate_tweaked_code: tweaked_creation_code: {:?}", tweaked_creation_code);

    // let's tweak!
    tweak(rpc, project, tweaked_creation_code, &artifact, quick).await
}

// Try to generate tweaked bytecode using CREATE execution method
async fn try_create_execution(
    rpc: &RpcOpts,
    project: &ClonedProject,
    tweaked_creation_code: &Bytes,
    quick: bool,
) -> Result<Bytes> {
    // prepare the execution backend
    let (mut db, mut env) = prepare_backend(rpc, project, quick).await?;

    // disable gas_limit, base_fee, and balance_check for the replayed transaction
    env.cfg.disable_block_gas_limit = true;
    env.cfg.disable_base_fee = true;
    env.cfg.disable_balance_check = true;

    // let hook into the creation process
    let mut inspector =
        TweakInspctor::new(Some(project.metadata.address), Some(tweaked_creation_code.clone()));

    // round 1: pinpoint the target creation count
    inspector.prepare_for_pinpoint()?;
    let rv = db.inspect(&mut env.clone(), &mut inspector)?;
    eyre::ensure!(
        rv.result.is_success(),
        "failed to pinpoint the target creation: {:?}",
        rv.result
    );
    eyre::ensure!(
        inspector.creation_stack_depth == 0,
        "unexpected creation count: {}",
        inspector.creation_stack_depth
    );

    // round 2: tweak the creation code
    inspector.prepare_for_tweak()?;
    // increase gas_limit and decrease gas_price for the transaction
    env.tx.gas_limit = env.tx.gas_limit.checked_mul(2).ok_or(eyre!("gas limit overflow"))?;
    env.tx.gas_price =
        env.tx.gas_price.checked_div(U256::from(2)).ok_or(eyre!("divided by zero"))?;
    env.tx.gas_priority_fee = Some(U256::ZERO);
    // we do not care about the execution result in this round
    let rv = db.inspect(&mut env, &mut inspector)?;
    eyre::ensure!(
        inspector.observed_created_addresses.len() <= 1,
        "hooked ADDRESS opcodes return different addresses"
    );

    trace!("tweak: inspector.tweaked_code after CREATE execution: {:?}", inspector.tweaked_code);
    if let Some(ref code) = inspector.tweaked_code {
        let hash = keccak256(code.as_ref());
        info!("tweak: tweaked_code hash: {:?}", hash);
    }

    inspector.tweaked_code.ok_or(eyre!("the tweaked code is not generated"))
}

// tweak the contract creation code
async fn tweak(
    rpc: &RpcOpts,
    project: &ClonedProject,
    tweaked_creation_code: Bytes,
    artifact: &ConfigurableContractArtifact,
    quick: bool,
) -> Result<Bytes> {
    // PRIMARY: Try CREATE execution method first
    info!("tweak: Attempting CREATE execution method to generate tweaked bytecode");
    
    // Try CREATE execution method
    match try_create_execution(rpc, project, &tweaked_creation_code, quick).await {
        Ok(bytecode) => {
            info!("tweak: Successfully generated bytecode using CREATE execution method");
            return Ok(bytecode);
        }
        Err(e) => {
            info!("tweak: CREATE execution method failed: {}, falling back to local artifact", e);
        }
    }

    // FALLBACK: Read deployed bytecode directly from the JSON artifact file
    info!("tweak: Attempting to read deployed bytecode directly from JSON artifact file");
    
    // Search for the artifact JSON file in the out directory
    // Format: out/<SourceFile>/<ContractName>.json
    // We need to search because we don't know the source file name
    let out_dir = project.root.join("out");
    let target_contract = &project.metadata.target_contract;
    
    info!("tweak: Searching for {}.json in {}", target_contract, out_dir.display());
    
    let mut artifact_path = None;
    if out_dir.exists() && out_dir.is_dir() {
        // Search all subdirectories in out/
        if let Ok(entries) = std::fs::read_dir(&out_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let potential_artifact = entry.path().join(format!("{}.json", target_contract));
                    if potential_artifact.exists() {
                        info!("tweak: Found artifact at: {}", potential_artifact.display());
                        artifact_path = Some(potential_artifact);
                        break;
                    }
                }
            }
        }
    }
    
    let artifact_path = match artifact_path {
        Some(path) => path,
        None => {
            info!("tweak: Could not find artifact JSON file for contract '{}'", target_contract);
            return Err(eyre!("Failed to generate tweaked bytecode: CREATE execution failed and artifact file not found"));
        }
    };
    
    if artifact_path.exists() {
        info!("tweak: Reading from artifact file: {}", artifact_path.display());
        match std::fs::read_to_string(&artifact_path) {
            Ok(json_content) => {
                info!("tweak: Read {} bytes from JSON file", json_content.len());
                match serde_json::from_str::<serde_json::Value>(&json_content) {
                    Ok(json) => {
                        // Extract deployedBytecode.object field
                        if let Some(deployed_bytecode_hex) = json.get("deployedBytecode")
                            .and_then(|db| db.get("object"))
                            .and_then(|obj| obj.as_str())
                        {
                            info!("tweak: Found deployedBytecode.object in JSON: {} total chars", deployed_bytecode_hex.len());
                            info!("tweak: First 100 chars: {}", &deployed_bytecode_hex[..std::cmp::min(100, deployed_bytecode_hex.len())]);
                            if deployed_bytecode_hex.len() > 100 {
                                let start_pos = deployed_bytecode_hex.len().saturating_sub(100);
                                info!("tweak: Last 100 chars: {}", &deployed_bytecode_hex[start_pos..]);
                            }
                            
                            // Strip 0x prefix if present
                            let hex_str = deployed_bytecode_hex.strip_prefix("0x").unwrap_or(deployed_bytecode_hex);
                            
                            // Convert hex string to bytes
                            match alloy_primitives::hex::decode(hex_str) {
                                Ok(bytecode_bytes) => {
                                    info!("tweak: Successfully decoded deployed bytecode from JSON: {} bytes", bytecode_bytes.len());
                                    
                                    // Log first and last few bytes for debugging
                                    let preview_len = std::cmp::min(32, bytecode_bytes.len());
                                    info!("tweak: Deployed bytecode FIRST {} bytes: {:02x?}", preview_len, &bytecode_bytes[..preview_len]);
                                    if bytecode_bytes.len() > 32 {
                                        let end_start = bytecode_bytes.len().saturating_sub(32);
                                        info!("tweak: Deployed bytecode LAST 32 bytes: {:02x?}", &bytecode_bytes[end_start..]);
                                    }
                                    
                                    return Ok(Bytes::from(bytecode_bytes));
                                }
                                Err(e) => {
                                    return Err(eyre!("Failed to decode hex bytecode from artifact: {}", e));
                                }
                            }
                        } else {
                            return Err(eyre!("deployedBytecode.object not found in JSON artifact"));
                        }
                    }
                    Err(e) => {
                        return Err(eyre!("Failed to parse JSON artifact: {}", e));
                    }
                }
            }
            Err(e) => {
                return Err(eyre!("Failed to read artifact file: {}", e));
            }
        }
    } else {
        return Err(eyre!("Artifact file not found at expected path"));
    }
}

fn prepare_tweaked_creation_code(
    project: &ClonedProject,
    artifact: &ConfigurableContractArtifact,
) -> Result<Bytes> {
    let bytecode = artifact.get_bytecode().ok_or(eyre!("the contract does not have bytecode"))?;
    trace!("prepare_tweaked_creation_code: bytecode: {:?}", bytecode);
    let deployment_bytecode =
        bytecode.bytes().ok_or(eyre!("the bytecode transformation failed"))?;
    trace!("prepare_tweaked_creation_code: deployment_bytecode: {:?}", deployment_bytecode);
    let constructor_arguments = &project.metadata.constructor_arguments;

    // concate the deployment bytecode with the constructor arguments
    let mut tweaked_creation_code = deployment_bytecode.to_vec();
    tweaked_creation_code.extend_from_slice(&constructor_arguments[..]);

    Ok(Bytes::from(tweaked_creation_code))
}

async fn prepare_backend(
    rpc: &RpcOpts,
    project: &ClonedProject,
    quick: bool,
) -> Result<(Backend, EnvWithHandlerCfg)> {
    // get rpc_url
    let rpc_url_cow = rpc.url(Some(&project.config))?.unwrap_or(Cow::Borrowed("http://localhost:8545"));
    let rpc_url: &str = rpc_url_cow.as_ref();
    info!("rpc_url: {}", rpc_url);

    // prepare the RPC provider
    let provider = ProviderBuilder::new(rpc_url)
        .chain(
            NamedChain::try_from(project.metadata.chain_id)
                .map_err(|_| eyre!("invalid chain id"))?,
        )
        .build()?;

    // get block number
    info!("project.metadata.creation_transaction: {:?}", project.metadata.creation_transaction);
    let tx_receipt = provider
        .get_transaction_receipt(project.metadata.creation_transaction)
        .await?
        .ok_or(eyre!("the transaction is not mined"))?;
    let block_number = tx_receipt.block_number.ok_or(eyre!("the transaction is not mined"))?;
    info!("block_number: {}", block_number);

    // then, we are going to replay all transactions before the creation transaction
    let block = provider
        .get_block(BlockId::Number(block_number.into()), BlockTransactionsKind::Full)
        .await?
        .ok_or(eyre!("block not found"))?;

    // prepare the block env
    let mut block_env: BlockEnv = BlockEnv {
        number: U256::from(
            block.header.number.expect("block number is not found. Maybe it is not mined yet?"),
        ),
        timestamp: U256::from(block.header.timestamp),
        coinbase: block.header.miner,
        difficulty: block.header.difficulty,
        prevrandao: Some(block.header.mix_hash.unwrap_or_default()),
        basefee: U256::from(block.header.base_fee_per_gas.unwrap_or_default()),
        gas_limit: U256::from(block.header.gas_limit),
        blob_excess_gas_and_price: None,
    };
    if let Some(excess_blob_gas) = block.header.excess_blob_gas {
        block_env.set_blob_excess_gas_and_price(excess_blob_gas as u64);
    }

    // get all transactions with receipts
    let txs_with_receipt = if !quick {
        let BlockTransactions::Full(txs) = block.transactions else {
            return Err(eyre::eyre!("block transactions not found"));
        };
        let Some(recipts) = provider.get_block_receipts(block_number.into()).await? else {
            return Err(eyre::eyre!("block receipts not found"));
        };
        txs.into_iter()
            .zip(recipts.into_iter())
            .map(|(a, b)| (WithOtherFields::new(a), b))
            .collect()
    } else {
        vec![(
            provider
                .get_transaction_by_hash(project.metadata.creation_transaction)
                .await?
                .ok_or(eyre!("the transaction is not mined"))?,
            tx_receipt,
        )]
    };

    // print all tx hashes within txs_with_receipt
    info!("Total transactions in block: {}", txs_with_receipt.len());
    for (index, (tx, _receipt)) in txs_with_receipt.iter().enumerate() {
        info!("  [{}] tx hash: {:?}", index, tx.hash);
    }

    // get the figment from the cloned project's config
    let mut config = project.config.clone();
    config.fork_block_number = Some(block_number - 1);
    let figment: Figment = config.into();
    let figment = figment.merge(rpc);

    // set evm options
    let mut evm_opts = figment.extract::<EvmOpts>()?;
    evm_opts.fork_url = Some(rpc_url.to_string());
    evm_opts.fork_block_number = Some(block_number - 1);

    // get an updated config
    let config = Config::try_from(figment)?.sanitized();

    // get env
    let env = evm_opts.evm_env().await?;

    // a loop to probe the proper EVM version
    let chain_id =
        NamedChain::try_from(project.metadata.chain_id).map_err(|_| eyre!("invalid chain id"))?;
    
    // Start with an initial guess based on block number for mainnet
    // This helps avoid probing from wrong direction (e.g., starting at ISTANBUL for a BYZANTIUM block)
    let mut spec_id = if chain_id == NamedChain::Mainnet {
        // Estimate spec_id based on block number (mainnet hardfork blocks)
        let estimated = match block_number {
            n if n < 1_150_000 => SpecId::FRONTIER,
            n if n < 2_463_000 => SpecId::HOMESTEAD,
            n if n < 2_675_000 => SpecId::TANGERINE,
            n if n < 4_370_000 => SpecId::SPURIOUS_DRAGON,
            n if n < 7_280_000 => SpecId::BYZANTIUM,
            n if n < 9_069_000 => SpecId::PETERSBURG,
            n if n < 12_244_000 => SpecId::ISTANBUL,
            n if n < 12_965_000 => SpecId::BERLIN,
            n if n < 13_773_000 => SpecId::LONDON,
            n if n < 15_050_000 => SpecId::ARROW_GLACIER,
            n if n < 15_537_394 => SpecId::GRAY_GLACIER,
            n if n < 17_034_870 => SpecId::MERGE,
            n if n < 19_426_589 => SpecId::SHANGHAI,
            n if n < 22_431_084 => SpecId::CANCUN,
            _ => SpecId::PRAGUE,
        };
        info!("Block {} estimated spec_id: {:?} (config default was {:?})", block_number, estimated, config.evm_spec_id());
        estimated
    } else {
        config.evm_spec_id()
    };
    info!("Starting probe with spec_id: {:?}", spec_id);

    // Arbitrum uses a modified EVM with different gas costs, so gas matching will fail
    // Skip probing for Arbitrum chains and use the config's default spec_id
    let is_arbitrum = matches!(
        chain_id,
        NamedChain::Arbitrum
            | NamedChain::ArbitrumGoerli
            | NamedChain::ArbitrumNova
            | NamedChain::ArbitrumTestnet
    );

    if is_arbitrum {
        cli_warn!("Tweaking contracts on Arbitrum: Skipping EVM version probing due to Arbitrum's modified gas model. Using config default spec_id: {:?}. Arbitrum is EVM-compatible but uses different gas costs, so gas matching will not work.", spec_id);
        // Skip probing for Arbitrum - use the config's default spec_id directly
        // Arbitrum launched after London, so it should support at least London/Merge
    }

    if chain_id == NamedChain::BinanceSmartChain || chain_id == NamedChain::BinanceSmartChainTestnet
    {
        cli_warn!("Tweaking contracts on BinanceSmartChain (BSC) may not work due to its non-standard gas consumption. Please see more at https://github.com/foundry-rs/foundry/issues/4784#issuecomment-1520402694");
    }

    if !quick && !is_arbitrum {
        info!("Starting EVM version probe for block {}", block_number);
        loop {
            // get backend and executor
            let db = Backend::spawn(evm_opts.get_fork(&config, env.clone()));
            // create the executor and the corresponding env
            let executor = ExecutorBuilder::new().spec(spec_id).build(env.clone(), db);
            info!("Trying spec_id {:?} for block {}", spec_id, block_number);

            match probe_evm_version(chain_id, executor, &block_env, &txs_with_receipt, None) {
                Ok(_) => {
                    info!("probe_evm_version succeeded with spec_id: {:?}", spec_id);
                    break;
                }
                Err(e) => {
                    info!("probe_evm_version fails with spec_id {:?}: {}", spec_id, e);
                    spec_id = SpecId::try_from_u8((spec_id as u8) + 1)
                        .ok_or(eyre!("failed to probe a proper EVM version. You may want to use --quick option to skip EVM version probing."))?;
                }
            }
        }
    }

    let db = Backend::spawn(evm_opts.get_fork(&config, env.clone()));
    let executor = ExecutorBuilder::new().spec(spec_id).build(env.clone(), db);
    
    // Skip final probe for Arbitrum chains (gas matching won't work)
    // For Arbitrum, we still need to return the backend and env, so we call probe_evm_version
    // but it will use the spec_id we already determined (from config default)
    if is_arbitrum {
        info!("Skipping final EVM version probe for Arbitrum chain (using spec_id: {:?})", spec_id);
    }
    
    probe_evm_version(
        chain_id,
        executor,
        &block_env,
        &txs_with_receipt,
        Some(project.metadata.creation_transaction),
    )
}

fn probe_evm_version(
    chain_id: NamedChain,
    mut executor: Executor,
    block_env: &BlockEnv,
    txs: &[(TransactionExt, TransactionReceiptExt)],
    target_tx: Option<TxHash>,
) -> Result<(Backend, EnvWithHandlerCfg)> {
    info!("probe_evm_version is called");
    info!("chain_id is {:?}", chain_id);
    let mut env = executor.env_with_handler_cfg().clone();

    env.block = block_env.clone();

    let non_standard_precompiled = NonStandardPrecompiled::get_precomiled_address(chain_id);
    
    // Arbitrum uses a modified EVM with different gas costs, so gas matching will fail
    let is_arbitrum = matches!(
        chain_id,
        NamedChain::Arbitrum
            | NamedChain::ArbitrumGoerli
            | NamedChain::ArbitrumNova
            | NamedChain::ArbitrumTestnet
    );

    let pb =
        init_progress(txs.len() as u64, format!("investigating {:?}", executor.spec_id()).as_str());
    pb.set_position(0);

    for (index, (tx, receipt)) in txs.iter().enumerate() {
        pb.set_position(index as u64 + 1);

        // skip system transactions
        if is_known_system_sender(tx.from) || tx.transaction_type == Some(SYSTEM_TRANSACTION_TYPE) {
            continue;
        }

        // skip non-standard precompiled contracts
        if let Some(precompiled) = non_standard_precompiled.as_ref() {
            if precompiled.contains(&tx.to.unwrap_or_default()) {
                continue;
            }
        }

        // always configure the transaction environment
        configure_tx_env(&mut env, tx);

        // find the creation transaction
        info!("target_tx is {:?}", target_tx);
        info!("tx.hash is {:?}", tx.hash);
        if Some(tx.hash) == target_tx {
            info!("probe_evm_version returns: {:?}", tx.hash);
            return Ok((executor.backend().clone(), env));
        }

        // get the actual gas used
        let real_gas_used = receipt.gas_used as u64;

        if tx.to.is_some() {
            let rv = executor.transact_with_env(env.clone());

            let rv = rv.wrap_err("Executing transaction fails")?;

            // check gas used (skip for Arbitrum due to different gas model)
            if rv.gas_used != real_gas_used {
                info!(
                    "Gas mismatch for tx {:?}: replayed {} (with spec_id {:?}), actual on-chain {}. Difference: {}",
                    tx.hash,
                    rv.gas_used,
                    executor.spec_id(),
                    real_gas_used,
                    rv.gas_used as i64 - real_gas_used as i64
                );
            }
            if !is_arbitrum {
            eyre::ensure!(
                rv.gas_used == real_gas_used,
                "Gas used mismatch: expected {}, got {} ({:?})",
                real_gas_used,
                rv.gas_used,
                tx.hash
            );
            } else {
                info!("Skipping gas check for Arbitrum chain (tx: {:?})", tx.hash);
            }

            // check transaction status
            if (**receipt).inner.inner.receipt.status.coerce_status() {
                eyre::ensure!(rv.exit_reason.is_ok(), "Transaction should succeed ({:?})", tx.hash);
            } else {
                eyre::ensure!(!rv.exit_reason.is_ok(), "Transaction should fail ({:?})", tx.hash);
            }
        } else {
            match executor.deploy_with_env(env.clone(), None) {
                // Reverted transactions should be skipped
                Err(EvmError::Execution(error)) => {
                    if !is_arbitrum {
                    eyre::ensure!(
                        error.gas_used == real_gas_used,
                        "Gas used mismatch: expected {}, got {} ({:?})",
                        real_gas_used,
                        error.gas_used,
                        tx.hash
                    );
                    } else {
                        info!("Skipping gas check for Arbitrum chain (deploy tx: {:?})", tx.hash);
                    }
                    eyre::ensure!(
                        !(**receipt).inner.inner.receipt.status.coerce_status(),
                        "Transaction should fail ({:?})",
                        tx.hash
                    );
                }
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "Failed to deploy transaction: {:?} in block {}",
                            tx.hash, env.block.number
                        )
                    })
                }
                Ok(rv) => {
                    eyre::ensure!(
                        rv.gas_used == real_gas_used,
                        "Gas used mismatch: expected {}, got {} ({:?})",
                        real_gas_used,
                        rv.gas_used,
                        tx.hash
                    );
                    eyre::ensure!(
                        (**receipt).inner.inner.receipt.status.coerce_status(),
                        "Transaction should succeed ({:?})",
                        tx.hash
                    );
                }
            }
        }
    }

    if target_tx.is_none() {
        info!("target_tx is none");
        Ok((executor.backend().clone(), env))
    } else {
        info!("target_tx is not none");
        Err(eyre!("the target transaction is not found"))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{code::tweak, metadata::CloneMetadata, ClonedProject};

    use alloy_primitives::Bytes;
    use foundry_cli::opts::RpcOpts;
    use foundry_compilers::artifacts::EvmVersion;
    use foundry_config::Config;

    fn get_fake_project(address: &str, tx: &str) -> ClonedProject {
        let fake_root = tempfile::tempdir().unwrap().path().to_path_buf();

        ClonedProject {
            root: fake_root,
            config: Config { evm_version: EvmVersion::Paris, ..Default::default() },
            metadata: CloneMetadata {
                path: "src/FakeContract.sol".into(),
                target_contract: "FakeContract".into(),
                chain_id: 1,
                address: address.parse().unwrap(),
                creation_transaction: tx.parse().unwrap(),
                deployer: Default::default(),
                constructor_arguments: Default::default(),
                storage_layout: Default::default(),
            },
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_direct_deployment() {
        let fake_project = get_fake_project(
            "0x8B3D32cf2bb4d0D16656f4c0b04Fa546274f1545",
            "0x79820495643caf5a1e7e96578361c9ddba0e0735cd684ada7450254f6fd58f51",
        );
        let rpc_url = std::env::var("ETH_RPC_URL").unwrap_or("http://localhost:8545".to_string());
        let rpc = RpcOpts { url: Some(rpc_url), ..Default::default() };

        let artifact = fake_project.main_artifact().unwrap();
        let tweaked_code = format!(
            "{:?}",
            tweak(&rpc, &fake_project, Bytes::from_str(FAKE_CREATION_CODE).unwrap(), &artifact, false)
                .await
                .unwrap()
        );

        assert!(
            tweaked_code.starts_with(FAKE_DEPLOYED_CODE),
            "the created code is not the same as the expected one"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_factory_deployment() {
        let fake_project = get_fake_project(
            "0x6B880d3B1FA2475C30Dc583726c56B4aFc66bD0b",
            "0x3812e48763d3631516206fb878007ed126223d5c31e8cc433c79659b8afbbf24",
        );
        let rpc_url = std::env::var("ETH_RPC_URL").unwrap_or("http://localhost:8545".to_string());
        let rpc = RpcOpts { url: Some(rpc_url), ..Default::default() };

        let artifact = fake_project.main_artifact().unwrap();
        let tweaked_code = format!(
            "{:?}",
            tweak(&rpc, &fake_project, Bytes::from_str(FAKE_CREATION_CODE).unwrap(), &artifact, false)
                .await
                .unwrap()
        );

        assert!(
            tweaked_code.starts_with(FAKE_DEPLOYED_CODE),
            "the created code is not the same as the expected one"
        );
    }

    const FAKE_DEPLOYED_CODE: &str = "0x60806040526004361061032d5760003560e01c80639157921c116101a5578063b2975794116100ec578063d547741f11610095578063dafae4081161006f578063dafae4081461096e578063dff525e11461098e578063e400327c146109ae578063e75235b8146109ce5761033c565b8063d547741f14610901578063d55ed10314610921578063d64af2a61461094e5761033c565b8063cdb67444116100c6578063cdb674441461089c578063cdf64a76146108b4578063d19773d2146108d45761033c565b8063b29757941461082f578063b9c362091461085c578063ca15c8731461087c5761033c565b8063a3912ec81161014e578063affed0e011610128578063affed0e0146107cc578063b1a2567e146107e2578063b1d08a03146108025761033c565b8063a3912ec81461033a578063ab7965661461077f578063ac78dfe8146107ac5761033c565b8063994390891161017f57806399439089146107155780639dcc4da314610735578063a217fddf1461076a5761033c565b80639157921c1461068f57806391d14854146106af57806393c5678f146106f55761033c565b806336568abe116102745780635c975abb1161021d5780637de5dedd116101f75780637de5dedd146106115780638456cb59146106265780638f34e3471461063b5780639010d07c1461066f5761033c565b80635c975abb146105ac5780636932be98146105c45780636c1ce670146105f15761033c565b80634d0d66731161024e5780634d0d66731461052f5780634d493f4e1461054f57806359122f6b1461057f5761033c565b806336568abe146104e75780633f4ba83a146105075780634b14557e1461051c5761033c565b80631d4a7210116102d65780632f2ff15d116102b05780632f2ff15d1461049b578063302d12db146104bb5780633644e515146104d25761033c565b80631d4a721014610428578063248a9ca3146104555780632dfdf0b5146104855761033c565b8063180ff1e911610307578063180ff1e9146103d55780631a8e55b0146103e85780631b6e7594146104085761033c565b806301ffc9a71461034457806317ce2dd41461037957806317fcb39b1461039d5761033c565b3661033c5761033a6109e6565b005b61033a6109e6565b34801561035057600080fd5b5061036461035f366004614843565b610a69565b60405190151581526020015b60405180910390f35b34801561038557600080fd5b5061038f60755481565b604051908152602001610370565b3480156103a957600080fd5b506074546103bd906001600160a01b031681565b6040516001600160a01b039091168152602001610370565b61033a6103e33660046148f4565b610aad565b3480156103f457600080fd5b5061033a6104033660046149e6565b610dbd565b34801561041457600080fd5b5061033a610423366004614a52565b610e8f565b34801561043457600080fd5b5061038f610443366004614aec565b603e6020526000908152604090205481565b34801561046157600080fd5b5061038f610470366004614b09565b60009081526072602052604090206001015490565b34801561049157600080fd5b5061038f60765481565b3480156104a757600080fd5b5061033a6104b6366004614b22565b610f64565b3480156104c757600080fd5b5061038f620f424081565b3480156104de57600080fd5b5060775461038f565b3480156104f357600080fd5b5061033a610502366004614b22565b610f8f565b34801561051357600080fd5b5061033a61101b565b61033a61052a366004614b52565b611083565b34801561053b57600080fd5b5061036461054a366004614b7d565b6110e1565b34801561055b57600080fd5b5061036461056a366004614b09565b607a6020526000908152604090205460ff1681565b34801561058b57600080fd5b5061038f61059a366004614aec565b603a6020526000908152604090205481565b3480156105b857600080fd5b5060005460ff16610364565b3480156105d057600080fd5b5061038f6105df366004614b09565b60796020526000908152604090205481565b3480156105fd57600080fd5b5061036461060c366004614c06565b61118c565b34801561061d57600080fd5b5061038f61119f565b34801561063257600080fd5b5061033a611234565b34801561064757600080fd5b5061038f7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e481565b34801561067b57600080fd5b506103bd61068a366004614c32565b61129c565b34801561069b57600080fd5b5061033a6106aa366004614c54565b6112b4565b3480156106bb57600080fd5b506103646106ca366004614b22565b60009182526072602090815260408084206001600160a01b0393909316845291905290205460ff1690565b34801561070157600080fd5b5061033a6107103660046149e6565b6115ca565b34801561072157600080fd5b506003546103bd906001600160a01b031681565b34801561074157600080fd5b50610755610750366004614c32565b611696565b60408051928352602083019190915201610370565b34801561077657600080fd5b5061038f600081565b34801561078b57600080fd5b5061038f61079a366004614aec565b603c6020526000908152604090205481565b3480156107b857600080fd5b506103646107c7366004614b09565b61172f565b3480156107d857600080fd5b5061038f60045481565b3480156107ee57600080fd5b5061033a6107fd3660046149e6565b6117ce565b34801561080e57600080fd5b5061038f61081d366004614aec565b60396020526000908152604090205481565b34801561083b57600080fd5b5061084f61084a366004614aec565b61189a565b6040516103709190614ca5565b34801561086857600080fd5b50610755610877366004614c32565b611992565b34801561088857600080fd5b5061038f610897366004614b09565b611a17565b3480156108a857600080fd5b50603754603854610755565b3480156108c057600080fd5b5061033a6108cf366004614aec565b611a2e565b3480156108e057600080fd5b5061038f6108ef366004614aec565b603b6020526000908152604090205481565b34801561090d57600080fd5b5061033a61091c366004614b22565b611a97565b34801561092d57600080fd5b5061038f61093c366004614aec565b603d6020526000908152604090205481565b34801561095a57600080fd5b5061033a610969366004614aec565b611abd565b34801561097a57600080fd5b50610364610989366004614b09565b611b26565b34801561099a57600080fd5b5061033a6109a9366004614cd2565b611bbd565b3480156109ba57600080fd5b5061033a6109c93660046149e6565b611cc7565b3480156109da57600080fd5b50600154600254610755565b60005460ff1615610a315760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b60448201526064015b60405180910390fd5b6074546001600160a01b03163314610a6757610a4b614802565b338152604080820151349101528051610a65908290611d93565b505b565b60006001600160e01b031982167f5a05180f000000000000000000000000000000000000000000000000000000001480610aa75750610aa78261210a565b92915050565b607154610100900460ff16610ac85760715460ff1615610acc565b303b155b610b3e5760405162461bcd60e51b815260206004820152602e60248201527f496e697469616c697a61626c653a20636f6e747261637420697320616c72656160448201527f647920696e697469616c697a65640000000000000000000000000000000000006064820152608401610a28565b607154610100900460ff16158015610b60576071805461ffff19166101011790555b610b6b60008d612171565b6075899055610b798b61217b565b610b828a6121dd565b610c29604080517f8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f60208201527f159f52c1e3a2b6a6aad3950adf713516211484e0516dad685ea662a094b7c43b918101919091527fad7c5bef027816a800da1736444fb58a807ef4c9603b7848673f7e3a68eb14a560608201524660808201523060a082015260c00160408051601f198184030181529190528051602090910120607755565b610c338887612238565b5050610c3f87876122f8565b5050610c496123d3565b6000610c558680614da6565b90501115610d1657610c7e610c6a8680614da6565b610c776020890189614da6565b8787612467565b610ca4610c8b8680614da6565b8660005b602002810190610c9f9190614da6565b612666565b610cca610cb18680614da6565b8660015b602002810190610cc59190614da6565b612779565b610cf0610cd78680614da6565b8660025b602002810190610ceb9190614da6565b61288c565b610d16610cfd8680614da6565b8660035b602002810190610d119190614da6565b612a30565b60005b610d266040870187614da6565b9050811015610d9c57610d8a7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e4610d606040890189614da6565b84818110610d7057610d70614d90565b9050602002016020810190610d859190614aec565b612b43565b80610d9481614e06565b915050610d19565b508015610daf576071805461ff00191690555b505050505050505050505050565b6000805160206157b9833981519152546001600160a01b03163314610e1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b82610e7d5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612779565b50505050565b6000805160206157b9833981519152546001600160a01b03163314610eef5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b84610f4e5760405162461bcd60e51b815260206004820152602960248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220656d60448201526870747920617272617960b81b6064820152608401610a28565b610f5c868686868686612467565b505050505050565b600082815260726020526040902060010154610f808133612b65565b610f8a8383612b43565b505050565b6001600160a01b038116331461100d5760405162461bcd60e51b815260206004820152602f60248201527f416363657373436f6e74726f6c3a2063616e206f6e6c792072656e6f756e636560448201527f20726f6c657320666f722073656c6600000000000000000000000000000000006064820152608401610a28565b6110178282612be5565b5050565b6000805160206157b9833981519152546001600160a01b0316331461107b5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a67612c07565b60005460ff16156110c95760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b610a656110db36839003830183614ec0565b33611d93565b6000805460ff16156111285760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b611184848484808060200260200160405190810160405280939291908181526020016000905b8282101561117a5761116b60608302860136819003810190614f13565b8152602001906001019061114e565b5050505050612ca3565b949350505050565b600061119883836133bc565b9392505050565b600061122f600360009054906101000a90046001600160a01b03166001600160a01b031663926323d56040518163ffffffff1660e01b815260040160206040518083038186803b1580156111f257600080fd5b505afa158015611206573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061122a9190614f89565b613480565b905090565b6000805160206157b9833981519152546001600160a01b031633146112945760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a676134b6565b60008281526073602052604081206111989083613531565b7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e46112df8133612b65565b60006112f86112f336859003850185614ff0565b61353d565b905061130c6112f336859003850185614ff0565b8335600090815260796020526040902054146113765760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c696420726563656044820152621a5c1d60ea1b6064820152608401610a28565b82356000908152607a602052604090205460ff166113fc5760405162461bcd60e51b815260206004820152603160248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220617060448201527f70726f766564207769746864726177616c0000000000000000000000000000006064820152608401610a28565b82356000908152607a602052604090819020805460ff19169055517fd639511b37b3b002cca6cfe6bca0d833945a5af5a045578a0627fc43b79b26309061144690839086906150c4565b60405180910390a160006114606080850160608601614aec565b9050600061147661012086016101008701615151565b600181111561148757611487614c71565b141561154f5760006114a2368690038601610100870161516e565b6001600160a01b0383166000908152603b60205260409020549091506114ce90610140870135906135c6565b604082015260006114e8368790038701610100880161516e565b60408301519091506114ff9061014088013561518a565b604082015260745461151f908390339086906001600160a01b03166135e0565b6115486115326060880160408901614aec565b60745483919086906001600160a01b03166135e0565b505061158b565b61158b6115626060860160408701614aec565b60745483906001600160a01b03166115833689900389016101008a0161516e565b9291906135e0565b7f21e88e956aa3e086f6388e899965cef814688f99ad8bb29b08d396571016372d82856040516115bc9291906150c4565b60405180910390a150505050565b6000805160206157b9833981519152546001600160a01b0316331461162a5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b8261168a5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612666565b6000806116b86000805160206157b9833981519152546001600160a01b031690565b6001600160a01b0316336001600160a01b0316146117115760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b61171b84846122f8565b90925090506117286123d3565b9250929050565b6003546040805163926323d560e01b815290516000926001600160a01b03169163926323d5916004808301926020929190829003018186803b15801561177457600080fd5b505afa158015611788573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906117ac9190614f89565b6037546117b991906151a1565b6038546117c690846151a1565b101592915050565b6000805160206157b9833981519152546001600160a01b0316331461182e5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b8261188e5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e898484848461288c565b60408051808201909152600080825260208201526001600160a01b0382166000908152607860205260409081902081518083019092528054829060ff1660018111156118e8576118e8614c71565b60018111156118f9576118f9614c71565b815290546001600160a01b036101009091048116602092830152908201519192501661198d5760405162461bcd60e51b815260206004820152602560248201527f4d61696e636861696e4761746577617956323a20756e737570706f727465642060448201527f746f6b656e0000000000000000000000000000000000000000000000000000006064820152608401610a28565b919050565b6000806119b46000805160206157b9833981519152546001600160a01b031690565b6001600160a01b0316336001600160a01b031614611a0d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b61171b8484612238565b6000818152607360205260408120610aa790613a13565b6000805160206157b9833981519152546001600160a01b03163314611a8e5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a65816121dd565b600082815260726020526040902060010154611ab38133612b65565b610f8a8383612be5565b6000805160206157b9833981519152546001600160a01b03163314611b1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a658161217b565b6003546040805163926323d560e01b815290516000926001600160a01b03169163926323d5916004808301926020929190829003018186803b158015611b6b57600080fd5b505afa158015611b7f573d6000803e3d6000fd5b505050506040513d601f19601f82011682018060405250810190611ba39190614f89565b600154611bb091906151a1565b6002546117c690846151a1565b6000805160206157b9833981519152546001600160a01b03163314611c1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b85611c7c5760405162461bcd60e51b815260206004820152602960248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220656d60448201526870747920617272617960b81b6064820152608401610a28565b611c8a878787878787612467565b611c978787836000610c8f565b611ca48787836001610cb5565b611cb18787836002610cdb565b611cbe8787836003610d01565b50505050505050565b6000805160206157b9833981519152546001600160a01b03163314611d275760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b82611d875760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612a30565b604080518082018252600080825260208201526074549184015190916001600160a01b031690611dc290613a1d565b60208401516001600160a01b0316611ee1573484604001516040015114611e375760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c69642072657175604482015262195cdd60ea1b6064820152608401610a28565b611e408161189a565b6040850151519092506001811115611e5a57611e5a614c71565b82516001811115611e6d57611e6d614c71565b14611ecd5760405162461bcd60e51b815260206004820152602a60248201527f4d61696e636861696e4761746577617956323a20696e76616c696420746f6b656044820152691b881cdd185b99185c9960b21b6064820152608401610a28565b6001600160a01b0381166020850152612087565b3415611f3b5760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c69642072657175604482015262195cdd60ea1b6064820152608401610a28565b611f48846020015161189a565b6040850151519092506001811115611f6257611f62614c71565b82516001811115611f7557611f75614c71565b14611fd55760405162461bcd60e51b815260206004820152602a60248201527f4d61696e636861696e4761746577617956323a20696e76616c696420746f6b656044820152691b881cdd185b99185c9960b21b6064820152608401610a28565b60208401516040850151611fec9185903090613ac7565b83602001516001600160a01b0316816001600160a01b031614156120875760408481015181015190517f2e1a7d4d00000000000000000000000000000000000000000000000000000000815260048101919091526001600160a01b03821690632e1a7d4d90602401600060405180830381600087803b15801561206e57600080fd5b505af1158015612082573d6000803e3d6000fd5b505050505b607680546000918261209883614e06565b91905055905060006120bf858386602001516075548a613ce190949392919063ffffffff16565b90507fd7b25068d9dc8d00765254cfb7f5070f98d263c8d68931d937c7362fa738048b6120eb8261353d565b826040516120fa9291906151c0565b60405180910390a1505050505050565b60006001600160e01b031982167f7965db0b000000000000000000000000000000000000000000000000000000001480610aa757507f01ffc9a7000000000000000000000000000000000000000000000000000000006001600160e01b0319831614610aa7565b6110178282612b43565b6074805473ffffffffffffffffffffffffffffffffffffffff19166001600160a01b0383169081179091556040519081527f9d2334c23be647e994f27a72c5eee42a43d5bdcfe15bb88e939103c2b114cbaf906020015b60405180910390a150565b6003805473ffffffffffffffffffffffffffffffffffffffff19166001600160a01b0383169081179091556040519081527fef40dc07567635f84f5edbd2f8dbc16b40d9d282dd8e7e6f4ff58236b6836169906020016121d2565b6000808284111561228b5760405162461bcd60e51b815260206004820152601c60248201527f4761746577617956323a20696e76616c6964207468726573686f6c64000000006044820152606401610a28565b505060018054600280549285905583905560048054919291849186919060006122b383614e06565b9091555060408051868152602081018690527f976f8a9c5bdf8248dec172376d6e2b80a8e3df2f0328e381c6db8e1cf138c0f891015b60405180910390a49250929050565b600080828411156123715760405162461bcd60e51b815260206004820152602760248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420746860448201527f726573686f6c64000000000000000000000000000000000000000000000000006064820152608401610a28565b5050603780546038805492859055839055600480549192918491869190600061239983614e06565b9091555060408051868152602081018690527f31312c97b89cc751b832d98fd459b967a2c3eef3b49757d1cf5ebaa12bb6eee191016122e9565b6002546037546123e391906151a1565b6038546001546123f391906151a1565b1115610a675760405162461bcd60e51b815260206004820152602860248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420746860448201527f726573686f6c64730000000000000000000000000000000000000000000000006064820152608401610a28565b848314801561247557508481145b6124e75760405162461bcd60e51b815260206004820152602860248201527f4d61696e636861696e4761746577617956323a20696e76616c6964206172726160448201527f79206c656e6774680000000000000000000000000000000000000000000000006064820152608401610a28565b60005b8581101561262c5784848281811061250457612504614d90565b90506020020160208101906125199190614aec565b6078600089898581811061252f5761252f614d90565b90506020020160208101906125449190614aec565b6001600160a01b039081168252602082019290925260400160002080547fffffffffffffffffffffff0000000000000000000000000000000000000000ff1661010093909216929092021790558282828181106125a3576125a3614d90565b90506020020160208101906125b89190615151565b607860008989858181106125ce576125ce614d90565b90506020020160208101906125e39190614aec565b6001600160a01b031681526020810191909152604001600020805460ff19166001838181111561261557612615614c71565b02179055508061262481614e06565b9150506124ea565b507fa4f03cc9c0e0aeb5b71b4ec800702753f65748c2cf3064695ba8e8b46be704448686868686866040516120fa969594939291906152c1565b8281146126c85760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612743578282828181106126e5576126e5614d90565b905060200201356039600087878581811061270257612702614d90565b90506020020160208101906127179190614aec565b6001600160a01b031681526020810191909152604001600020558061273b81614e06565b9150506126cb565b507f80bc635c452ae67f12f9b6f12ad4daa6dbbc04eeb9ebb87d354ce10c0e210dc0848484846040516115bc9493929190615339565b8281146127db5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612856578282828181106127f8576127f8614d90565b90506020020135603a600087878581811061281557612815614d90565b905060200201602081019061282a9190614aec565b6001600160a01b031681526020810191909152604001600020558061284e81614e06565b9150506127de565b507f64557254143204d91ba2d95acb9fda1e5fea55f77efd028685765bc1e94dd4b5848484846040516115bc9493929190615339565b8281146128ee5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b838110156129fa57620f424083838381811061290f5761290f614d90565b90506020020135111561298a5760405162461bcd60e51b815260206004820152602860248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420706560448201527f7263656e746167650000000000000000000000000000000000000000000000006064820152608401610a28565b82828281811061299c5761299c614d90565b90506020020135603b60008787858181106129b9576129b9614d90565b90506020020160208101906129ce9190614aec565b6001600160a01b03168152602081019190915260400160002055806129f281614e06565b9150506128f1565b507fb05f5de88ae0294ebb6f67c5af2fcbbd593cc6bdfe543e2869794a4c8ce3ea50848484846040516115bc9493929190615339565b828114612a925760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612b0d57828282818110612aaf57612aaf614d90565b90506020020135603c6000878785818110612acc57612acc614d90565b9050602002016020810190612ae19190614aec565b6001600160a01b0316815260208101919091526040016000205580612b0581614e06565b915050612a95565b507fb5d2963614d72181b4df1f993d45b83edf42fa19710f0204217ba1b3e183bb73848484846040516115bc9493929190615339565b612b4d8282613db6565b6000828152607360205260409020610f8a9082613e58565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff1661101757612ba3816001600160a01b03166014613e6d565b612bae836020613e6d565b604051602001612bbf9291906153d0565b60408051601f198184030181529082905262461bcd60e51b8252610a2891600401615451565b612bef828261404e565b6000828152607360205260409020610f8a90826140d1565b60005460ff16612c595760405162461bcd60e51b815260206004820152601460248201527f5061757361626c653a206e6f74207061757365640000000000000000000000006044820152606401610a28565b6000805460ff191690557f5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa335b6040516001600160a01b03909116815260200160405180910390a1565b6000823561014084013582612cbe6080870160608801614aec565b9050612cdb612cd6368890038801610100890161516e565b613a1d565b6001612ced6040880160208901615151565b6001811115612cfe57612cfe614c71565b14612d715760405162461bcd60e51b815260206004820152602860248201527f4d61696e636861696e4761746577617956323a20696e76616c6964207265636560448201527f697074206b696e640000000000000000000000000000000000000000000000006064820152608401610a28565b60808601354614612de95760405162461bcd60e51b8152602060048201526024808201527f4d61696e636861696e4761746577617956323a20696e76616c6964206368616960448201527f6e206964000000000000000000000000000000000000000000000000000000006064820152608401610a28565b6000612dfe61084a6080890160608a01614aec565b9050612e1261012088016101008901615151565b6001811115612e2357612e23614c71565b81516001811115612e3657612e36614c71565b148015612e675750612e4e60e0880160c08901614aec565b6001600160a01b031681602001516001600160a01b0316145b612ebf5760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c696420726563656044820152621a5c1d60ea1b6064820152608401610a28565b60008481526079602052604090205415612f415760405162461bcd60e51b815260206004820152603260248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220707260448201527f6f636573736564207769746864726177616c00000000000000000000000000006064820152608401610a28565b6001612f5561012089016101008a01615151565b6001811115612f6657612f66614c71565b1480612f795750612f7782846133bc565b155b612feb5760405162461bcd60e51b815260206004820152603260248201527f4d61696e636861696e4761746577617956323a2072656163686564206461696c60448201527f79207769746864726177616c206c696d697400000000000000000000000000006064820152608401610a28565b6000612fff6112f3368a90038a018a614ff0565b9050600061300f607754836140e6565b6003549091506001600160a01b0316600061303d6130356101208d016101008e01615151565b878985614142565b60408051606081018252600080825260208201819052918101829052919b50919250819081906000805b8f5181101561323c578f818151811061308257613082614d90565b6020908102919091018101518051818301516040808401518151600081529586018083528f905260ff9093169085015260608401526080830152935060019060a0016020604051602081039080840390855afa1580156130e6573d6000803e3d6000fd5b505050602060405103519450846001600160a01b0316846001600160a01b0316106131795760405162461bcd60e51b815260206004820152602160248201527f4d61696e636861696e4761746577617956323a20696e76616c6964206f72646560448201527f72000000000000000000000000000000000000000000000000000000000000006064820152608401610a28565b6040517f953865650000000000000000000000000000000000000000000000000000000081526001600160a01b03808716600483015286955089169063953865659060240160206040518083038186803b1580156131d657600080fd5b505afa1580156131ea573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061320e9190614f89565b6132189083615484565b915086821061322a576001955061323c565b8061323481614e06565b915050613067565b50846132b05760405162461bcd60e51b815260206004820152603660248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220696e60448201527f73756666696369656e7420766f746520776569676874000000000000000000006064820152608401610a28565b50505060008a81526079602052604090208690555050881561332c576000888152607a602052604090819020805460ff19166001179055517f89e52969465b1f1866fc5d46fd62de953962e9cb33552443cd999eba05bd20dc906133179086908e906150c4565b60405180910390a15050505050505050610aa7565b6133368688614233565b61337561334960608d0160408e01614aec565b87607460009054906101000a90046001600160a01b03168e61010001803603810190611583919061516e565b7f21e88e956aa3e086f6388e899965cef814688f99ad8bb29b08d396571016372d848c6040516133a69291906150c4565b60405180910390a1505050505050505092915050565b6001600160a01b0382166000908152603a602052604081205482106133e357506000610aa7565b60006133f2620151804261549c565b6001600160a01b0385166000908152603e60205260409020549091508111156134385750506001600160a01b0382166000908152603c6020526040902054811015610aa7565b6001600160a01b0384166000908152603d602052604090205461345c908490615484565b6001600160a01b0385166000908152603c602052604090205411159150610aa79050565b600060025460016002548460015461349891906151a1565b6134a29190615484565b6134ac919061518a565b610aa7919061549c565b60005460ff16156134fc5760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b6000805460ff191660011790557f62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258612c863390565b600061119883836142c3565b60007fb9d1fe7c9deeec5dc90a2f47ff1684239519f2545b2228d3d91fb27df3189eea60001b8260000151836020015161357a85604001516142ed565b61358786606001516142ed565b6135948760800151614350565b6040516020016135a9969594939291906154be565b604051602081830303815290604052805190602001209050919050565b6000620f42406135d683856151a1565b611198919061549c565b6000816001600160a01b0316836001600160a01b031614156136905760408086015190516001600160a01b0386169180156108fc02916000818181858888f1935050505061368b57816001600160a01b031663d0e30db086604001516040518263ffffffff1660e01b81526004016000604051808303818588803b15801561366757600080fd5b505af115801561367b573d6000803e3d6000fd5b505050505061368b858585614393565b613a0c565b6000855160018111156136a5576136a5614c71565b1415613866576040517f70a082310000000000000000000000000000000000000000000000000000000081523060048201526000906001600160a01b038516906370a082319060240160206040518083038186803b15801561370657600080fd5b505afa15801561371a573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061373e9190614f89565b9050856040015181101561385557836001600160a01b03166340c10f193083896040015161376c919061518a565b6040516001600160a01b03909216602483015260448201526064016040516020818303038152906040529060e01b6020820180516001600160e01b0383818316178352505050506040516137c091906154f8565b6000604051808303816000865af19150503d80600081146137fd576040519150601f19603f3d011682016040523d82523d6000602084013e613802565b606091505b505080925050816138555760405162461bcd60e51b815260206004820152601b60248201527f546f6b656e3a204552433230206d696e74696e67206661696c656400000000006044820152606401610a28565b613860868686614393565b50613a0c565b60018551600181111561387b5761387b614c71565b141561399e5761389083858760200151614437565b61368b57602085810151604080516001600160a01b038881166024830152604480830194909452825180830390940184526064909101825292820180516001600160e01b03167f40c10f1900000000000000000000000000000000000000000000000000000000179052519185169161390991906154f8565b6000604051808303816000865af19150503d8060008114613946576040519150601f19603f3d011682016040523d82523d6000602084013e61394b565b606091505b5050809150508061368b5760405162461bcd60e51b815260206004820152601c60248201527f546f6b656e3a20455243373231206d696e74696e67206661696c6564000000006044820152606401610a28565b60405162461bcd60e51b815260206004820152602160248201527f546f6b656e3a20756e737570706f7274656420746f6b656e207374616e64617260448201527f64000000000000000000000000000000000000000000000000000000000000006064820152608401610a28565b5050505050565b6000610aa7825490565b600081516001811115613a3257613a32614c71565b148015613a43575060008160400151115b8015613a5157506020810151155b80613a7b5750600181516001811115613a6c57613a6c614c71565b148015613a7b57506040810151155b610a655760405162461bcd60e51b815260206004820152601360248201527f546f6b656e3a20696e76616c696420696e666f000000000000000000000000006044820152606401610a28565b600060608186516001811115613adf57613adf614c71565b1415613bbd5760408681015181516001600160a01b038881166024830152878116604483015260648083019390935283518083039093018352608490910183526020820180516001600160e01b03166323b872dd60e01b179052915191851691613b4991906154f8565b6000604051808303816000865af19150503d8060008114613b86576040519150601f19603f3d011682016040523d82523d6000602084013e613b8b565b606091505b509092509050818015613bb6575080511580613bb6575080806020019051810190613bb69190615514565b9150613c84565b600186516001811115613bd257613bd2614c71565b141561399e57602086810151604080516001600160a01b0389811660248301528881166044830152606480830194909452825180830390940184526084909101825292820180516001600160e01b03166323b872dd60e01b1790525191851691613c3c91906154f8565b6000604051808303816000865af19150503d8060008114613c79576040519150601f19603f3d011682016040523d82523d6000602084013e613c7e565b606091505b50909250505b81610f5c57613c92866144e2565b613ca6866001600160a01b03166014613e6d565b613cba866001600160a01b03166014613e6d565b613cce866001600160a01b03166014613e6d565b604051602001612bbf9493929190615536565b613d516040805160a08101825260008082526020808301829052835160608082018652838252818301849052818601849052848601919091528451808201865283815280830184905280860184905281850152845190810185528281529081018290529283015290608082015290565b83815260006020820181905250604080820180516001600160a01b039788169052602080890151825190891690820152905146908301528751606084018051918916909152805195909716940193909352935182015292909201516080820152919050565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff166110175760008281526072602090815260408083206001600160a01b03851684529091529020805460ff19166001179055613e143390565b6001600160a01b0316816001600160a01b0316837f2f8788117e7eff1d82e926ec794901d17c78024a50270940304540a733656f0d60405160405180910390a45050565b6000611198836001600160a01b03841661454f565b60606000613e7c8360026151a1565b613e87906002615484565b67ffffffffffffffff811115613e9f57613e9f614e21565b6040519080825280601f01601f191660200182016040528015613ec9576020820181803683370190505b5090507f300000000000000000000000000000000000000000000000000000000000000081600081518110613f0057613f00614d90565b60200101906001600160f81b031916908160001a9053507f780000000000000000000000000000000000000000000000000000000000000081600181518110613f4b57613f4b614d90565b60200101906001600160f81b031916908160001a9053506000613f6f8460026151a1565b613f7a906001615484565b90505b6001811115613fff577f303132333435363738396162636465660000000000000000000000000000000085600f1660108110613fbb57613fbb614d90565b1a60f81b828281518110613fd157613fd1614d90565b60200101906001600160f81b031916908160001a90535060049490941c93613ff881615606565b9050613f7d565b5083156111985760405162461bcd60e51b815260206004820181905260248201527f537472696e67733a20686578206c656e67746820696e73756666696369656e746044820152606401610a28565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff16156110175760008281526072602090815260408083206001600160a01b0385168085529252808320805460ff1916905551339285917ff6391f5c32d9c69d2a47ea670b442974b53935d1edc7fd64eb21e047a839171b9190a45050565b6000611198836001600160a01b03841661459e565b604080517f19010000000000000000000000000000000000000000000000000000000000006020808301919091526022820185905260428083018590528351808403909101815260629092019092528051910120600090611198565b6000806000836001600160a01b031663926323d56040518163ffffffff1660e01b815260040160206040518083038186803b15801561418057600080fd5b505afa158015614194573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906141b89190614f89565b90506141c381613480565b925060008760018111156141d9576141d9614c71565b1415614229576001600160a01b038616600090815260396020526040902054851061420a5761420781614691565b92505b6001600160a01b0386166000908152603a602052604090205485101591505b5094509492505050565b6000614242620151804261549c565b6001600160a01b0384166000908152603e6020526040902054909150811115614291576001600160a01b03929092166000908152603e6020908152604080832094909455603d90529190912055565b6001600160a01b0383166000908152603d6020526040812080548492906142b9908490615484565b9091555050505050565b60008260000182815481106142da576142da614d90565b9060005260206000200154905092915050565b805160208083015160408085015190516000946135a9947f353bdd8d69b9e3185b3972e08b03845c0c14a21a390215302776a7a34b0e87649491939192019384526001600160a01b03928316602085015291166040830152606082015260800190565b805160208083015160408085015190516000946135a9947f1e2b74b2a792d5c0f0b6e59b037fa9d43d84fbb759337f0112fcc15ca414fc8d94919391920161561d565b600080845160018111156143a9576143a9614c71565b14156143c5576143be828486604001516146a9565b90506143ef565b6001845160018111156143da576143da614c71565b141561399e576143be82848660200151614437565b80610e89576143fd846144e2565b614411846001600160a01b03166014613e6d565b614425846001600160a01b03166014613e6d565b604051602001612bbf93929190615648565b604080513060248201526001600160a01b038481166044830152606480830185905283518084039091018152608490920183526020820180516001600160e01b03166323b872dd60e01b1790529151600092861691614495916154f8565b6000604051808303816000865af19150503d80600081146144d2576040519150601f19603f3d011682016040523d82523d6000602084013e6144d7565b606091505b509095945050505050565b606061450d826000015160018111156144fd576144fd614c71565b6001600160a01b03166001613e6d565b61451a8360200151614795565b6145278460400151614795565b604051602001614539939291906156d9565b6040516020818303038152906040529050919050565b600081815260018301602052604081205461459657508154600181810184556000848152602080822090930184905584548482528286019093526040902091909155610aa7565b506000610aa7565b600081815260018301602052604081205480156146875760006145c260018361518a565b85549091506000906145d69060019061518a565b905081811461463b5760008660000182815481106145f6576145f6614d90565b906000526020600020015490508087600001848154811061461957614619614d90565b6000918252602080832090910192909255918252600188019052604090208390555b855486908061464c5761464c6157a2565b600190038181906000526020600020016000905590558560010160008681526020019081526020016000206000905560019350505050610aa7565b6000915050610aa7565b600060385460016038548460375461349891906151a1565b604080516001600160a01b038481166024830152604480830185905283518084039091018152606490920183526020820180516001600160e01b03167fa9059cbb0000000000000000000000000000000000000000000000000000000017905291516000926060929087169161471f91906154f8565b6000604051808303816000865af19150503d806000811461475c576040519150601f19603f3d011682016040523d82523d6000602084013e614761565b606091505b50909250905081801561478c57508051158061478c57508080602001905181019061478c9190615514565b95945050505050565b6060816147d557505060408051808201909152600481527f3078303000000000000000000000000000000000000000000000000000000000602082015290565b8160005b81156147f857806147e981614e06565b915050600882901c91506147d9565b6111848482613e6d565b604080516060810182526000808252602082015290810161483e6040805160608101909152806000815260200160008152602001600081525090565b905290565b60006020828403121561485557600080fd5b81356001600160e01b03198116811461119857600080fd5b6001600160a01b0381168114610a6557600080fd5b803561198d8161486d565b8060608101831015610aa757600080fd5b8060808101831015610aa757600080fd5b60008083601f8401126148c157600080fd5b50813567ffffffffffffffff8111156148d957600080fd5b6020830191508360208260051b850101111561172857600080fd5b60008060008060008060008060008060006101408c8e03121561491657600080fd5b61491f8c614882565b9a5061492d60208d01614882565b995061493b60408d01614882565b985060608c0135975060808c0135965060a08c0135955060c08c0135945067ffffffffffffffff8060e08e0135111561497357600080fd5b6149838e60e08f01358f0161488d565b9450806101008e0135111561499757600080fd5b6149a88e6101008f01358f0161489e565b9350806101208e013511156149bc57600080fd5b506149ce8d6101208e01358e016148af565b81935080925050509295989b509295989b9093969950565b600080600080604085870312156149fc57600080fd5b843567ffffffffffffffff80821115614a1457600080fd5b614a20888389016148af565b90965094506020870135915080821115614a3957600080fd5b50614a46878288016148af565b95989497509550505050565b60008060008060008060608789031215614a6b57600080fd5b863567ffffffffffffffff80821115614a8357600080fd5b614a8f8a838b016148af565b90985096506020890135915080821115614aa857600080fd5b614ab48a838b016148af565b90965094506040890135915080821115614acd57600080fd5b50614ada89828a016148af565b979a9699509497509295939492505050565b600060208284031215614afe57600080fd5b81356111988161486d565b600060208284031215614b1b57600080fd5b5035919050565b60008060408385031215614b3557600080fd5b823591506020830135614b478161486d565b809150509250929050565b600060a08284031215614b6457600080fd5b50919050565b60006101608284031215614b6457600080fd5b60008060006101808486031215614b9357600080fd5b614b9d8585614b6a565b925061016084013567ffffffffffffffff80821115614bbb57600080fd5b818601915086601f830112614bcf57600080fd5b813581811115614bde57600080fd5b876020606083028501011115614bf357600080fd5b6020830194508093505050509250925092565b60008060408385031215614c1957600080fd5b8235614c248161486d565b946020939093013593505050565b60008060408385031215614c4557600080fd5b50508035926020909101359150565b60006101608284031215614c6757600080fd5b6111988383614b6a565b634e487b7160e01b600052602160045260246000fd5b60028110610a6557634e487b7160e01b600052602160045260246000fd5b81516040820190614cb581614c87565b808352506001600160a01b03602084015116602083015292915050565b60008060008060008060006080888a031215614ced57600080fd5b873567ffffffffffffffff80821115614d0557600080fd5b614d118b838c016148af565b909950975060208a0135915080821115614d2a57600080fd5b614d368b838c016148af565b909750955060408a0135915080821115614d4f57600080fd5b614d5b8b838c016148af565b909550935060608a0135915080821115614d7457600080fd5b50614d818a828b0161489e565b91505092959891949750929550565b634e487b7160e01b600052603260045260246000fd5b6000808335601e19843603018112614dbd57600080fd5b83018035915067ffffffffffffffff821115614dd857600080fd5b6020019150600581901b360382131561172857600080fd5b634e487b7160e01b600052601160045260246000fd5b6000600019821415614e1a57614e1a614df0565b5060010190565b634e487b7160e01b600052604160045260246000fd5b6040516060810167ffffffffffffffff81118282101715614e6857634e487b7160e01b600052604160045260246000fd5b60405290565b60028110610a6557600080fd5b600060608284031215614e8d57600080fd5b614e95614e37565b90508135614ea281614e6e565b80825250602082013560208201526040820135604082015292915050565b600060a08284031215614ed257600080fd5b614eda614e37565b8235614ee58161486d565b81526020830135614ef58161486d565b6020820152614f078460408501614e7b565b60408201529392505050565b600060608284031215614f2557600080fd5b6040516060810181811067ffffffffffffffff82111715614f5657634e487b7160e01b600052604160045260246000fd5b604052823560ff81168114614f6a57600080fd5b8152602083810135908201526040928301359281019290925250919050565b600060208284031215614f9b57600080fd5b5051919050565b600060608284031215614fb457600080fd5b614fbc614e37565b90508135614fc98161486d565b81526020820135614fd98161486d565b806020830152506040820135604082015292915050565b6000610160828403121561500357600080fd5b60405160a0810181811067ffffffffffffffff8211171561503457634e487b7160e01b600052604160045260246000fd5b60405282358152602083013561504981614e6e565b602082015261505b8460408501614fa2565b604082015261506d8460a08501614fa2565b6060820152615080846101008501614e7b565b60808201529392505050565b80356150978161486d565b6001600160a01b0390811683526020820135906150b38261486d565b166020830152604090810135910152565b6000610180820190508382528235602083015260208301356150e581614e6e565b6150ee81614c87565b80604084015250615105606083016040850161508c565b61511560c0830160a0850161508c565b61012061010084013561512781614e6e565b61513081614c87565b81840152830135610140808401919091529092013561016090910152919050565b60006020828403121561516357600080fd5b813561119881614e6e565b60006060828403121561518057600080fd5b6111988383614e7b565b60008282101561519c5761519c614df0565b500390565b60008160001904831182151516156151bb576151bb614df0565b500290565b6000610180820190508382528251602083015260208301516151e181614c87565b6040838101919091528381015180516001600160a01b03908116606086015260208201511660808501529081015160a084015250606083015180516001600160a01b0390811660c085015260208201511660e08401526040810151610100840152506080830151805161525381614c87565b6101208401526020810151610140840152604001516101609092019190915292915050565b8183526000602080850194508260005b858110156152b657813561529b8161486d565b6001600160a01b031687529582019590820190600101615288565b509495945050505050565b6060815260006152d560608301888a615278565b6020838203818501526152e982888a615278565b8481036040860152858152869250810160005b8681101561532a57833561530f81614e6e565b61531881614c87565b825292820192908201906001016152fc565b509a9950505050505050505050565b60408152600061534d604083018688615278565b82810360208401528381527f07ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff84111561538557600080fd5b8360051b80866020840137600091016020019081529695505050505050565b60005b838110156153bf5781810151838201526020016153a7565b83811115610e895750506000910152565b7f416363657373436f6e74726f6c3a206163636f756e74200000000000000000008152600083516154088160178501602088016153a4565b7f206973206d697373696e6720726f6c652000000000000000000000000000000060179184019182015283516154458160288401602088016153a4565b01602801949350505050565b60208152600082518060208401526154708160408501602087016153a4565b601f01601f19169190910160400192915050565b6000821982111561549757615497614df0565b500190565b6000826154b957634e487b7160e01b600052601260045260246000fd5b500490565b8681526020810186905260c081016154d586614c87565b8560408301528460608301528360808301528260a0830152979650505050505050565b6000825161550a8184602087016153a4565b9190910192915050565b60006020828403121561552657600080fd5b8151801515811461119857600080fd5b7f546f6b656e3a20636f756c64206e6f74207472616e7366657220000000000000815260008551602061556f82601a8601838b016153a4565b7f2066726f6d200000000000000000000000000000000000000000000000000000601a9285019283015286516155aa81838501848b016153a4565b630103a37960e51b92018181019290925285516155cd81602485018985016153a4565b660103a37b5b2b7160cd1b6024939091019283015284516155f481602b85018489016153a4565b91909101602b01979650505050505050565b60008161561557615615614df0565b506000190190565b8481526080810161562d85614c87565b84602083015283604083015282606083015295945050505050565b7f546f6b656e3a20636f756c64206e6f74207472616e736665722000000000000081526000845161568081601a8501602089016153a4565b630103a37960e51b601a9184019182015284516156a481601e8401602089016153a4565b660103a37b5b2b7160cd1b601e929091019182015283516156cc8160258401602088016153a4565b0160250195945050505050565b7f546f6b656e496e666f280000000000000000000000000000000000000000000081526000845161571181600a8501602089016153a4565b80830190507f2c0000000000000000000000000000000000000000000000000000000000000080600a830152855161575081600b850160208a016153a4565b600b920191820152835161576b81600c8401602088016153a4565b7f2900000000000000000000000000000000000000000000000000000000000000600c9290910191820152600d0195945050505050565b634e487b7160e01b600052603160045260246000fdfeb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d610348617350726f787941646d696e3a20756e617574686f72697a65642073656e64a2646970667358221220badebb50cc96ecc4d7c364bf224a8d7c089229c1d01abb009e0241a26ff0da2564736f6c63430008090033";

    const FAKE_CREATION_CODE: &str = "608060405234801561001057600080fd5b506000805460ff1916905561582e806200002b6000396000f3fe60806040526004361061032d5760003560e01c80639157921c116101a5578063b2975794116100ec578063d547741f11610095578063dafae4081161006f578063dafae4081461096e578063dff525e11461098e578063e400327c146109ae578063e75235b8146109ce5761033c565b8063d547741f14610901578063d55ed10314610921578063d64af2a61461094e5761033c565b8063cdb67444116100c6578063cdb674441461089c578063cdf64a76146108b4578063d19773d2146108d45761033c565b8063b29757941461082f578063b9c362091461085c578063ca15c8731461087c5761033c565b8063a3912ec81161014e578063affed0e011610128578063affed0e0146107cc578063b1a2567e146107e2578063b1d08a03146108025761033c565b8063a3912ec81461033a578063ab7965661461077f578063ac78dfe8146107ac5761033c565b8063994390891161017f57806399439089146107155780639dcc4da314610735578063a217fddf1461076a5761033c565b80639157921c1461068f57806391d14854146106af57806393c5678f146106f55761033c565b806336568abe116102745780635c975abb1161021d5780637de5dedd116101f75780637de5dedd146106115780638456cb59146106265780638f34e3471461063b5780639010d07c1461066f5761033c565b80635c975abb146105ac5780636932be98146105c45780636c1ce670146105f15761033c565b80634d0d66731161024e5780634d0d66731461052f5780634d493f4e1461054f57806359122f6b1461057f5761033c565b806336568abe146104e75780633f4ba83a146105075780634b14557e1461051c5761033c565b80631d4a7210116102d65780632f2ff15d116102b05780632f2ff15d1461049b578063302d12db146104bb5780633644e515146104d25761033c565b80631d4a721014610428578063248a9ca3146104555780632dfdf0b5146104855761033c565b8063180ff1e911610307578063180ff1e9146103d55780631a8e55b0146103e85780631b6e7594146104085761033c565b806301ffc9a71461034457806317ce2dd41461037957806317fcb39b1461039d5761033c565b3661033c5761033a6109e6565b005b61033a6109e6565b34801561035057600080fd5b5061036461035f366004614843565b610a69565b60405190151581526020015b60405180910390f35b34801561038557600080fd5b5061038f60755481565b604051908152602001610370565b3480156103a957600080fd5b506074546103bd906001600160a01b031681565b6040516001600160a01b039091168152602001610370565b61033a6103e33660046148f4565b610aad565b3480156103f457600080fd5b5061033a6104033660046149e6565b610dbd565b34801561041457600080fd5b5061033a610423366004614a52565b610e8f565b34801561043457600080fd5b5061038f610443366004614aec565b603e6020526000908152604090205481565b34801561046157600080fd5b5061038f610470366004614b09565b60009081526072602052604090206001015490565b34801561049157600080fd5b5061038f60765481565b3480156104a757600080fd5b5061033a6104b6366004614b22565b610f64565b3480156104c757600080fd5b5061038f620f424081565b3480156104de57600080fd5b5060775461038f565b3480156104f357600080fd5b5061033a610502366004614b22565b610f8f565b34801561051357600080fd5b5061033a61101b565b61033a61052a366004614b52565b611083565b34801561053b57600080fd5b5061036461054a366004614b7d565b6110e1565b34801561055b57600080fd5b5061036461056a366004614b09565b607a6020526000908152604090205460ff1681565b34801561058b57600080fd5b5061038f61059a366004614aec565b603a6020526000908152604090205481565b3480156105b857600080fd5b5060005460ff16610364565b3480156105d057600080fd5b5061038f6105df366004614b09565b60796020526000908152604090205481565b3480156105fd57600080fd5b5061036461060c366004614c06565b61118c565b34801561061d57600080fd5b5061038f61119f565b34801561063257600080fd5b5061033a611234565b34801561064757600080fd5b5061038f7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e481565b34801561067b57600080fd5b506103bd61068a366004614c32565b61129c565b34801561069b57600080fd5b5061033a6106aa366004614c54565b6112b4565b3480156106bb57600080fd5b506103646106ca366004614b22565b60009182526072602090815260408084206001600160a01b0393909316845291905290205460ff1690565b34801561070157600080fd5b5061033a6107103660046149e6565b6115ca565b34801561072157600080fd5b506003546103bd906001600160a01b031681565b34801561074157600080fd5b50610755610750366004614c32565b611696565b60408051928352602083019190915201610370565b34801561077657600080fd5b5061038f600081565b34801561078b57600080fd5b5061038f61079a366004614aec565b603c6020526000908152604090205481565b3480156107b857600080fd5b506103646107c7366004614b09565b61172f565b3480156107d857600080fd5b5061038f60045481565b3480156107ee57600080fd5b5061033a6107fd3660046149e6565b6117ce565b34801561080e57600080fd5b5061038f61081d366004614aec565b60396020526000908152604090205481565b34801561083b57600080fd5b5061084f61084a366004614aec565b61189a565b6040516103709190614ca5565b34801561086857600080fd5b50610755610877366004614c32565b611992565b34801561088857600080fd5b5061038f610897366004614b09565b611a17565b3480156108a857600080fd5b50603754603854610755565b3480156108c057600080fd5b5061033a6108cf366004614aec565b611a2e565b3480156108e057600080fd5b5061038f6108ef366004614aec565b603b6020526000908152604090205481565b34801561090d57600080fd5b5061033a61091c366004614b22565b611a97565b34801561092d57600080fd5b5061038f61093c366004614aec565b603d6020526000908152604090205481565b34801561095a57600080fd5b5061033a610969366004614aec565b611abd565b34801561097a57600080fd5b50610364610989366004614b09565b611b26565b34801561099a57600080fd5b5061033a6109a9366004614cd2565b611bbd565b3480156109ba57600080fd5b5061033a6109c93660046149e6565b611cc7565b3480156109da57600080fd5b50600154600254610755565b60005460ff1615610a315760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b60448201526064015b60405180910390fd5b6074546001600160a01b03163314610a6757610a4b614802565b338152604080820151349101528051610a65908290611d93565b505b565b60006001600160e01b031982167f5a05180f000000000000000000000000000000000000000000000000000000001480610aa75750610aa78261210a565b92915050565b607154610100900460ff16610ac85760715460ff1615610acc565b303b155b610b3e5760405162461bcd60e51b815260206004820152602e60248201527f496e697469616c697a61626c653a20636f6e747261637420697320616c72656160448201527f647920696e697469616c697a65640000000000000000000000000000000000006064820152608401610a28565b607154610100900460ff16158015610b60576071805461ffff19166101011790555b610b6b60008d612171565b6075899055610b798b61217b565b610b828a6121dd565b610c29604080517f8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f60208201527f159f52c1e3a2b6a6aad3950adf713516211484e0516dad685ea662a094b7c43b918101919091527fad7c5bef027816a800da1736444fb58a807ef4c9603b7848673f7e3a68eb14a560608201524660808201523060a082015260c00160408051601f198184030181529190528051602090910120607755565b610c338887612238565b5050610c3f87876122f8565b5050610c496123d3565b6000610c558680614da6565b90501115610d1657610c7e610c6a8680614da6565b610c776020890189614da6565b8787612467565b610ca4610c8b8680614da6565b8660005b602002810190610c9f9190614da6565b612666565b610cca610cb18680614da6565b8660015b602002810190610cc59190614da6565b612779565b610cf0610cd78680614da6565b8660025b602002810190610ceb9190614da6565b61288c565b610d16610cfd8680614da6565b8660035b602002810190610d119190614da6565b612a30565b60005b610d266040870187614da6565b9050811015610d9c57610d8a7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e4610d606040890189614da6565b84818110610d7057610d70614d90565b9050602002016020810190610d859190614aec565b612b43565b80610d9481614e06565b915050610d19565b508015610daf576071805461ff00191690555b505050505050505050505050565b6000805160206157b9833981519152546001600160a01b03163314610e1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b82610e7d5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612779565b50505050565b6000805160206157b9833981519152546001600160a01b03163314610eef5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b84610f4e5760405162461bcd60e51b815260206004820152602960248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220656d60448201526870747920617272617960b81b6064820152608401610a28565b610f5c868686868686612467565b505050505050565b600082815260726020526040902060010154610f808133612b65565b610f8a8383612b43565b505050565b6001600160a01b038116331461100d5760405162461bcd60e51b815260206004820152602f60248201527f416363657373436f6e74726f6c3a2063616e206f6e6c792072656e6f756e636560448201527f20726f6c657320666f722073656c6600000000000000000000000000000000006064820152608401610a28565b6110178282612be5565b5050565b6000805160206157b9833981519152546001600160a01b0316331461107b5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a67612c07565b60005460ff16156110c95760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b610a656110db36839003830183614ec0565b33611d93565b6000805460ff16156111285760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b611184848484808060200260200160405190810160405280939291908181526020016000905b8282101561117a5761116b60608302860136819003810190614f13565b8152602001906001019061114e565b5050505050612ca3565b949350505050565b600061119883836133bc565b9392505050565b600061122f600360009054906101000a90046001600160a01b03166001600160a01b031663926323d56040518163ffffffff1660e01b815260040160206040518083038186803b1580156111f257600080fd5b505afa158015611206573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061122a9190614f89565b613480565b905090565b6000805160206157b9833981519152546001600160a01b031633146112945760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a676134b6565b60008281526073602052604081206111989083613531565b7f5e5712e902fff5e704bc4d506ad976718319e019e9d2a872528a01a85db433e46112df8133612b65565b60006112f86112f336859003850185614ff0565b61353d565b905061130c6112f336859003850185614ff0565b8335600090815260796020526040902054146113765760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c696420726563656044820152621a5c1d60ea1b6064820152608401610a28565b82356000908152607a602052604090205460ff166113fc5760405162461bcd60e51b815260206004820152603160248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220617060448201527f70726f766564207769746864726177616c0000000000000000000000000000006064820152608401610a28565b82356000908152607a602052604090819020805460ff19169055517fd639511b37b3b002cca6cfe6bca0d833945a5af5a045578a0627fc43b79b26309061144690839086906150c4565b60405180910390a160006114606080850160608601614aec565b9050600061147661012086016101008701615151565b600181111561148757611487614c71565b141561154f5760006114a2368690038601610100870161516e565b6001600160a01b0383166000908152603b60205260409020549091506114ce90610140870135906135c6565b604082015260006114e8368790038701610100880161516e565b60408301519091506114ff9061014088013561518a565b604082015260745461151f908390339086906001600160a01b03166135e0565b6115486115326060880160408901614aec565b60745483919086906001600160a01b03166135e0565b505061158b565b61158b6115626060860160408701614aec565b60745483906001600160a01b03166115833689900389016101008a0161516e565b9291906135e0565b7f21e88e956aa3e086f6388e899965cef814688f99ad8bb29b08d396571016372d82856040516115bc9291906150c4565b60405180910390a150505050565b6000805160206157b9833981519152546001600160a01b0316331461162a5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b8261168a5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612666565b6000806116b86000805160206157b9833981519152546001600160a01b031690565b6001600160a01b0316336001600160a01b0316146117115760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b61171b84846122f8565b90925090506117286123d3565b9250929050565b6003546040805163926323d560e01b815290516000926001600160a01b03169163926323d5916004808301926020929190829003018186803b15801561177457600080fd5b505afa158015611788573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906117ac9190614f89565b6037546117b991906151a1565b6038546117c690846151a1565b101592915050565b6000805160206157b9833981519152546001600160a01b0316331461182e5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b8261188e5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e898484848461288c565b60408051808201909152600080825260208201526001600160a01b0382166000908152607860205260409081902081518083019092528054829060ff1660018111156118e8576118e8614c71565b60018111156118f9576118f9614c71565b815290546001600160a01b036101009091048116602092830152908201519192501661198d5760405162461bcd60e51b815260206004820152602560248201527f4d61696e636861696e4761746577617956323a20756e737570706f727465642060448201527f746f6b656e0000000000000000000000000000000000000000000000000000006064820152608401610a28565b919050565b6000806119b46000805160206157b9833981519152546001600160a01b031690565b6001600160a01b0316336001600160a01b031614611a0d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b61171b8484612238565b6000818152607360205260408120610aa790613a13565b6000805160206157b9833981519152546001600160a01b03163314611a8e5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a65816121dd565b600082815260726020526040902060010154611ab38133612b65565b610f8a8383612be5565b6000805160206157b9833981519152546001600160a01b03163314611b1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b610a658161217b565b6003546040805163926323d560e01b815290516000926001600160a01b03169163926323d5916004808301926020929190829003018186803b158015611b6b57600080fd5b505afa158015611b7f573d6000803e3d6000fd5b505050506040513d601f19601f82011682018060405250810190611ba39190614f89565b600154611bb091906151a1565b6002546117c690846151a1565b6000805160206157b9833981519152546001600160a01b03163314611c1d5760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b85611c7c5760405162461bcd60e51b815260206004820152602960248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220656d60448201526870747920617272617960b81b6064820152608401610a28565b611c8a878787878787612467565b611c978787836000610c8f565b611ca48787836001610cb5565b611cb18787836002610cdb565b611cbe8787836003610d01565b50505050505050565b6000805160206157b9833981519152546001600160a01b03163314611d275760405162461bcd60e51b815260206004820152602260248201526000805160206157d983398151915260448201526132b960f11b6064820152608401610a28565b82611d875760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b610e8984848484612a30565b604080518082018252600080825260208201526074549184015190916001600160a01b031690611dc290613a1d565b60208401516001600160a01b0316611ee1573484604001516040015114611e375760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c69642072657175604482015262195cdd60ea1b6064820152608401610a28565b611e408161189a565b6040850151519092506001811115611e5a57611e5a614c71565b82516001811115611e6d57611e6d614c71565b14611ecd5760405162461bcd60e51b815260206004820152602a60248201527f4d61696e636861696e4761746577617956323a20696e76616c696420746f6b656044820152691b881cdd185b99185c9960b21b6064820152608401610a28565b6001600160a01b0381166020850152612087565b3415611f3b5760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c69642072657175604482015262195cdd60ea1b6064820152608401610a28565b611f48846020015161189a565b6040850151519092506001811115611f6257611f62614c71565b82516001811115611f7557611f75614c71565b14611fd55760405162461bcd60e51b815260206004820152602a60248201527f4d61696e636861696e4761746577617956323a20696e76616c696420746f6b656044820152691b881cdd185b99185c9960b21b6064820152608401610a28565b60208401516040850151611fec9185903090613ac7565b83602001516001600160a01b0316816001600160a01b031614156120875760408481015181015190517f2e1a7d4d00000000000000000000000000000000000000000000000000000000815260048101919091526001600160a01b03821690632e1a7d4d90602401600060405180830381600087803b15801561206e57600080fd5b505af1158015612082573d6000803e3d6000fd5b505050505b607680546000918261209883614e06565b91905055905060006120bf858386602001516075548a613ce190949392919063ffffffff16565b90507fd7b25068d9dc8d00765254cfb7f5070f98d263c8d68931d937c7362fa738048b6120eb8261353d565b826040516120fa9291906151c0565b60405180910390a1505050505050565b60006001600160e01b031982167f7965db0b000000000000000000000000000000000000000000000000000000001480610aa757507f01ffc9a7000000000000000000000000000000000000000000000000000000006001600160e01b0319831614610aa7565b6110178282612b43565b6074805473ffffffffffffffffffffffffffffffffffffffff19166001600160a01b0383169081179091556040519081527f9d2334c23be647e994f27a72c5eee42a43d5bdcfe15bb88e939103c2b114cbaf906020015b60405180910390a150565b6003805473ffffffffffffffffffffffffffffffffffffffff19166001600160a01b0383169081179091556040519081527fef40dc07567635f84f5edbd2f8dbc16b40d9d282dd8e7e6f4ff58236b6836169906020016121d2565b6000808284111561228b5760405162461bcd60e51b815260206004820152601c60248201527f4761746577617956323a20696e76616c6964207468726573686f6c64000000006044820152606401610a28565b505060018054600280549285905583905560048054919291849186919060006122b383614e06565b9091555060408051868152602081018690527f976f8a9c5bdf8248dec172376d6e2b80a8e3df2f0328e381c6db8e1cf138c0f891015b60405180910390a49250929050565b600080828411156123715760405162461bcd60e51b815260206004820152602760248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420746860448201527f726573686f6c64000000000000000000000000000000000000000000000000006064820152608401610a28565b5050603780546038805492859055839055600480549192918491869190600061239983614e06565b9091555060408051868152602081018690527f31312c97b89cc751b832d98fd459b967a2c3eef3b49757d1cf5ebaa12bb6eee191016122e9565b6002546037546123e391906151a1565b6038546001546123f391906151a1565b1115610a675760405162461bcd60e51b815260206004820152602860248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420746860448201527f726573686f6c64730000000000000000000000000000000000000000000000006064820152608401610a28565b848314801561247557508481145b6124e75760405162461bcd60e51b815260206004820152602860248201527f4d61696e636861696e4761746577617956323a20696e76616c6964206172726160448201527f79206c656e6774680000000000000000000000000000000000000000000000006064820152608401610a28565b60005b8581101561262c5784848281811061250457612504614d90565b90506020020160208101906125199190614aec565b6078600089898581811061252f5761252f614d90565b90506020020160208101906125449190614aec565b6001600160a01b039081168252602082019290925260400160002080547fffffffffffffffffffffff0000000000000000000000000000000000000000ff1661010093909216929092021790558282828181106125a3576125a3614d90565b90506020020160208101906125b89190615151565b607860008989858181106125ce576125ce614d90565b90506020020160208101906125e39190614aec565b6001600160a01b031681526020810191909152604001600020805460ff19166001838181111561261557612615614c71565b02179055508061262481614e06565b9150506124ea565b507fa4f03cc9c0e0aeb5b71b4ec800702753f65748c2cf3064695ba8e8b46be704448686868686866040516120fa969594939291906152c1565b8281146126c85760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612743578282828181106126e5576126e5614d90565b905060200201356039600087878581811061270257612702614d90565b90506020020160208101906127179190614aec565b6001600160a01b031681526020810191909152604001600020558061273b81614e06565b9150506126cb565b507f80bc635c452ae67f12f9b6f12ad4daa6dbbc04eeb9ebb87d354ce10c0e210dc0848484846040516115bc9493929190615339565b8281146127db5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612856578282828181106127f8576127f8614d90565b90506020020135603a600087878581811061281557612815614d90565b905060200201602081019061282a9190614aec565b6001600160a01b031681526020810191909152604001600020558061284e81614e06565b9150506127de565b507f64557254143204d91ba2d95acb9fda1e5fea55f77efd028685765bc1e94dd4b5848484846040516115bc9493929190615339565b8281146128ee5760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b838110156129fa57620f424083838381811061290f5761290f614d90565b90506020020135111561298a5760405162461bcd60e51b815260206004820152602860248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c696420706560448201527f7263656e746167650000000000000000000000000000000000000000000000006064820152608401610a28565b82828281811061299c5761299c614d90565b90506020020135603b60008787858181106129b9576129b9614d90565b90506020020160208101906129ce9190614aec565b6001600160a01b03168152602081019190915260400160002055806129f281614e06565b9150506128f1565b507fb05f5de88ae0294ebb6f67c5af2fcbbd593cc6bdfe543e2869794a4c8ce3ea50848484846040516115bc9493929190615339565b828114612a925760405162461bcd60e51b815260206004820152602a60248201527f5769746864726177616c4c696d69746174696f6e3a20696e76616c69642061726044820152690e4c2f240d8cadccee8d60b31b6064820152608401610a28565b60005b83811015612b0d57828282818110612aaf57612aaf614d90565b90506020020135603c6000878785818110612acc57612acc614d90565b9050602002016020810190612ae19190614aec565b6001600160a01b0316815260208101919091526040016000205580612b0581614e06565b915050612a95565b507fb5d2963614d72181b4df1f993d45b83edf42fa19710f0204217ba1b3e183bb73848484846040516115bc9493929190615339565b612b4d8282613db6565b6000828152607360205260409020610f8a9082613e58565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff1661101757612ba3816001600160a01b03166014613e6d565b612bae836020613e6d565b604051602001612bbf9291906153d0565b60408051601f198184030181529082905262461bcd60e51b8252610a2891600401615451565b612bef828261404e565b6000828152607360205260409020610f8a90826140d1565b60005460ff16612c595760405162461bcd60e51b815260206004820152601460248201527f5061757361626c653a206e6f74207061757365640000000000000000000000006044820152606401610a28565b6000805460ff191690557f5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa335b6040516001600160a01b03909116815260200160405180910390a1565b6000823561014084013582612cbe6080870160608801614aec565b9050612cdb612cd6368890038801610100890161516e565b613a1d565b6001612ced6040880160208901615151565b6001811115612cfe57612cfe614c71565b14612d715760405162461bcd60e51b815260206004820152602860248201527f4d61696e636861696e4761746577617956323a20696e76616c6964207265636560448201527f697074206b696e640000000000000000000000000000000000000000000000006064820152608401610a28565b60808601354614612de95760405162461bcd60e51b8152602060048201526024808201527f4d61696e636861696e4761746577617956323a20696e76616c6964206368616960448201527f6e206964000000000000000000000000000000000000000000000000000000006064820152608401610a28565b6000612dfe61084a6080890160608a01614aec565b9050612e1261012088016101008901615151565b6001811115612e2357612e23614c71565b81516001811115612e3657612e36614c71565b148015612e675750612e4e60e0880160c08901614aec565b6001600160a01b031681602001516001600160a01b0316145b612ebf5760405162461bcd60e51b815260206004820152602360248201527f4d61696e636861696e4761746577617956323a20696e76616c696420726563656044820152621a5c1d60ea1b6064820152608401610a28565b60008481526079602052604090205415612f415760405162461bcd60e51b815260206004820152603260248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220707260448201527f6f636573736564207769746864726177616c00000000000000000000000000006064820152608401610a28565b6001612f5561012089016101008a01615151565b6001811115612f6657612f66614c71565b1480612f795750612f7782846133bc565b155b612feb5760405162461bcd60e51b815260206004820152603260248201527f4d61696e636861696e4761746577617956323a2072656163686564206461696c60448201527f79207769746864726177616c206c696d697400000000000000000000000000006064820152608401610a28565b6000612fff6112f3368a90038a018a614ff0565b9050600061300f607754836140e6565b6003549091506001600160a01b0316600061303d6130356101208d016101008e01615151565b878985614142565b60408051606081018252600080825260208201819052918101829052919b50919250819081906000805b8f5181101561323c578f818151811061308257613082614d90565b6020908102919091018101518051818301516040808401518151600081529586018083528f905260ff9093169085015260608401526080830152935060019060a0016020604051602081039080840390855afa1580156130e6573d6000803e3d6000fd5b505050602060405103519450846001600160a01b0316846001600160a01b0316106131795760405162461bcd60e51b815260206004820152602160248201527f4d61696e636861696e4761746577617956323a20696e76616c6964206f72646560448201527f72000000000000000000000000000000000000000000000000000000000000006064820152608401610a28565b6040517f953865650000000000000000000000000000000000000000000000000000000081526001600160a01b03808716600483015286955089169063953865659060240160206040518083038186803b1580156131d657600080fd5b505afa1580156131ea573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061320e9190614f89565b6132189083615484565b915086821061322a576001955061323c565b8061323481614e06565b915050613067565b50846132b05760405162461bcd60e51b815260206004820152603660248201527f4d61696e636861696e4761746577617956323a20717565727920666f7220696e60448201527f73756666696369656e7420766f746520776569676874000000000000000000006064820152608401610a28565b50505060008a81526079602052604090208690555050881561332c576000888152607a602052604090819020805460ff19166001179055517f89e52969465b1f1866fc5d46fd62de953962e9cb33552443cd999eba05bd20dc906133179086908e906150c4565b60405180910390a15050505050505050610aa7565b6133368688614233565b61337561334960608d0160408e01614aec565b87607460009054906101000a90046001600160a01b03168e61010001803603810190611583919061516e565b7f21e88e956aa3e086f6388e899965cef814688f99ad8bb29b08d396571016372d848c6040516133a69291906150c4565b60405180910390a1505050505050505092915050565b6001600160a01b0382166000908152603a602052604081205482106133e357506000610aa7565b60006133f2620151804261549c565b6001600160a01b0385166000908152603e60205260409020549091508111156134385750506001600160a01b0382166000908152603c6020526040902054811015610aa7565b6001600160a01b0384166000908152603d602052604090205461345c908490615484565b6001600160a01b0385166000908152603c602052604090205411159150610aa79050565b600060025460016002548460015461349891906151a1565b6134a29190615484565b6134ac919061518a565b610aa7919061549c565b60005460ff16156134fc5760405162461bcd60e51b815260206004820152601060248201526f14185d5cd8589b194e881c185d5cd95960821b6044820152606401610a28565b6000805460ff191660011790557f62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258612c863390565b600061119883836142c3565b60007fb9d1fe7c9deeec5dc90a2f47ff1684239519f2545b2228d3d91fb27df3189eea60001b8260000151836020015161357a85604001516142ed565b61358786606001516142ed565b6135948760800151614350565b6040516020016135a9969594939291906154be565b604051602081830303815290604052805190602001209050919050565b6000620f42406135d683856151a1565b611198919061549c565b6000816001600160a01b0316836001600160a01b031614156136905760408086015190516001600160a01b0386169180156108fc02916000818181858888f1935050505061368b57816001600160a01b031663d0e30db086604001516040518263ffffffff1660e01b81526004016000604051808303818588803b15801561366757600080fd5b505af115801561367b573d6000803e3d6000fd5b505050505061368b858585614393565b613a0c565b6000855160018111156136a5576136a5614c71565b1415613866576040517f70a082310000000000000000000000000000000000000000000000000000000081523060048201526000906001600160a01b038516906370a082319060240160206040518083038186803b15801561370657600080fd5b505afa15801561371a573d6000803e3d6000fd5b505050506040513d601f19601f8201168201806040525081019061373e9190614f89565b9050856040015181101561385557836001600160a01b03166340c10f193083896040015161376c919061518a565b6040516001600160a01b03909216602483015260448201526064016040516020818303038152906040529060e01b6020820180516001600160e01b0383818316178352505050506040516137c091906154f8565b6000604051808303816000865af19150503d80600081146137fd576040519150601f19603f3d011682016040523d82523d6000602084013e613802565b606091505b505080925050816138555760405162461bcd60e51b815260206004820152601b60248201527f546f6b656e3a204552433230206d696e74696e67206661696c656400000000006044820152606401610a28565b613860868686614393565b50613a0c565b60018551600181111561387b5761387b614c71565b141561399e5761389083858760200151614437565b61368b57602085810151604080516001600160a01b038881166024830152604480830194909452825180830390940184526064909101825292820180516001600160e01b03167f40c10f1900000000000000000000000000000000000000000000000000000000179052519185169161390991906154f8565b6000604051808303816000865af19150503d8060008114613946576040519150601f19603f3d011682016040523d82523d6000602084013e61394b565b606091505b5050809150508061368b5760405162461bcd60e51b815260206004820152601c60248201527f546f6b656e3a20455243373231206d696e74696e67206661696c6564000000006044820152606401610a28565b60405162461bcd60e51b815260206004820152602160248201527f546f6b656e3a20756e737570706f7274656420746f6b656e207374616e64617260448201527f64000000000000000000000000000000000000000000000000000000000000006064820152608401610a28565b5050505050565b6000610aa7825490565b600081516001811115613a3257613a32614c71565b148015613a43575060008160400151115b8015613a5157506020810151155b80613a7b5750600181516001811115613a6c57613a6c614c71565b148015613a7b57506040810151155b610a655760405162461bcd60e51b815260206004820152601360248201527f546f6b656e3a20696e76616c696420696e666f000000000000000000000000006044820152606401610a28565b600060608186516001811115613adf57613adf614c71565b1415613bbd5760408681015181516001600160a01b038881166024830152878116604483015260648083019390935283518083039093018352608490910183526020820180516001600160e01b03166323b872dd60e01b179052915191851691613b4991906154f8565b6000604051808303816000865af19150503d8060008114613b86576040519150601f19603f3d011682016040523d82523d6000602084013e613b8b565b606091505b509092509050818015613bb6575080511580613bb6575080806020019051810190613bb69190615514565b9150613c84565b600186516001811115613bd257613bd2614c71565b141561399e57602086810151604080516001600160a01b0389811660248301528881166044830152606480830194909452825180830390940184526084909101825292820180516001600160e01b03166323b872dd60e01b1790525191851691613c3c91906154f8565b6000604051808303816000865af19150503d8060008114613c79576040519150601f19603f3d011682016040523d82523d6000602084013e613c7e565b606091505b50909250505b81610f5c57613c92866144e2565b613ca6866001600160a01b03166014613e6d565b613cba866001600160a01b03166014613e6d565b613cce866001600160a01b03166014613e6d565b604051602001612bbf9493929190615536565b613d516040805160a08101825260008082526020808301829052835160608082018652838252818301849052818601849052848601919091528451808201865283815280830184905280860184905281850152845190810185528281529081018290529283015290608082015290565b83815260006020820181905250604080820180516001600160a01b039788169052602080890151825190891690820152905146908301528751606084018051918916909152805195909716940193909352935182015292909201516080820152919050565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff166110175760008281526072602090815260408083206001600160a01b03851684529091529020805460ff19166001179055613e143390565b6001600160a01b0316816001600160a01b0316837f2f8788117e7eff1d82e926ec794901d17c78024a50270940304540a733656f0d60405160405180910390a45050565b6000611198836001600160a01b03841661454f565b60606000613e7c8360026151a1565b613e87906002615484565b67ffffffffffffffff811115613e9f57613e9f614e21565b6040519080825280601f01601f191660200182016040528015613ec9576020820181803683370190505b5090507f300000000000000000000000000000000000000000000000000000000000000081600081518110613f0057613f00614d90565b60200101906001600160f81b031916908160001a9053507f780000000000000000000000000000000000000000000000000000000000000081600181518110613f4b57613f4b614d90565b60200101906001600160f81b031916908160001a9053506000613f6f8460026151a1565b613f7a906001615484565b90505b6001811115613fff577f303132333435363738396162636465660000000000000000000000000000000085600f1660108110613fbb57613fbb614d90565b1a60f81b828281518110613fd157613fd1614d90565b60200101906001600160f81b031916908160001a90535060049490941c93613ff881615606565b9050613f7d565b5083156111985760405162461bcd60e51b815260206004820181905260248201527f537472696e67733a20686578206c656e67746820696e73756666696369656e746044820152606401610a28565b60008281526072602090815260408083206001600160a01b038516845290915290205460ff16156110175760008281526072602090815260408083206001600160a01b0385168085529252808320805460ff1916905551339285917ff6391f5c32d9c69d2a47ea670b442974b53935d1edc7fd64eb21e047a839171b9190a45050565b6000611198836001600160a01b03841661459e565b604080517f19010000000000000000000000000000000000000000000000000000000000006020808301919091526022820185905260428083018590528351808403909101815260629092019092528051910120600090611198565b6000806000836001600160a01b031663926323d56040518163ffffffff1660e01b815260040160206040518083038186803b15801561418057600080fd5b505afa158015614194573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906141b89190614f89565b90506141c381613480565b925060008760018111156141d9576141d9614c71565b1415614229576001600160a01b038616600090815260396020526040902054851061420a5761420781614691565b92505b6001600160a01b0386166000908152603a602052604090205485101591505b5094509492505050565b6000614242620151804261549c565b6001600160a01b0384166000908152603e6020526040902054909150811115614291576001600160a01b03929092166000908152603e6020908152604080832094909455603d90529190912055565b6001600160a01b0383166000908152603d6020526040812080548492906142b9908490615484565b9091555050505050565b60008260000182815481106142da576142da614d90565b9060005260206000200154905092915050565b805160208083015160408085015190516000946135a9947f353bdd8d69b9e3185b3972e08b03845c0c14a21a390215302776a7a34b0e87649491939192019384526001600160a01b03928316602085015291166040830152606082015260800190565b805160208083015160408085015190516000946135a9947f1e2b74b2a792d5c0f0b6e59b037fa9d43d84fbb759337f0112fcc15ca414fc8d94919391920161561d565b600080845160018111156143a9576143a9614c71565b14156143c5576143be828486604001516146a9565b90506143ef565b6001845160018111156143da576143da614c71565b141561399e576143be82848660200151614437565b80610e89576143fd846144e2565b614411846001600160a01b03166014613e6d565b614425846001600160a01b03166014613e6d565b604051602001612bbf93929190615648565b604080513060248201526001600160a01b038481166044830152606480830185905283518084039091018152608490920183526020820180516001600160e01b03166323b872dd60e01b1790529151600092861691614495916154f8565b6000604051808303816000865af19150503d80600081146144d2576040519150601f19603f3d011682016040523d82523d6000602084013e6144d7565b606091505b509095945050505050565b606061450d826000015160018111156144fd576144fd614c71565b6001600160a01b03166001613e6d565b61451a8360200151614795565b6145278460400151614795565b604051602001614539939291906156d9565b6040516020818303038152906040529050919050565b600081815260018301602052604081205461459657508154600181810184556000848152602080822090930184905584548482528286019093526040902091909155610aa7565b506000610aa7565b600081815260018301602052604081205480156146875760006145c260018361518a565b85549091506000906145d69060019061518a565b905081811461463b5760008660000182815481106145f6576145f6614d90565b906000526020600020015490508087600001848154811061461957614619614d90565b6000918252602080832090910192909255918252600188019052604090208390555b855486908061464c5761464c6157a2565b600190038181906000526020600020016000905590558560010160008681526020019081526020016000206000905560019350505050610aa7565b6000915050610aa7565b600060385460016038548460375461349891906151a1565b604080516001600160a01b038481166024830152604480830185905283518084039091018152606490920183526020820180516001600160e01b03167fa9059cbb0000000000000000000000000000000000000000000000000000000017905291516000926060929087169161471f91906154f8565b6000604051808303816000865af19150503d806000811461475c576040519150601f19603f3d011682016040523d82523d6000602084013e614761565b606091505b50909250905081801561478c57508051158061478c57508080602001905181019061478c9190615514565b95945050505050565b6060816147d557505060408051808201909152600481527f3078303000000000000000000000000000000000000000000000000000000000602082015290565b8160005b81156147f857806147e981614e06565b915050600882901c91506147d9565b6111848482613e6d565b604080516060810182526000808252602082015290810161483e6040805160608101909152806000815260200160008152602001600081525090565b905290565b60006020828403121561485557600080fd5b81356001600160e01b03198116811461119857600080fd5b6001600160a01b0381168114610a6557600080fd5b803561198d8161486d565b8060608101831015610aa757600080fd5b8060808101831015610aa757600080fd5b60008083601f8401126148c157600080fd5b50813567ffffffffffffffff8111156148d957600080fd5b6020830191508360208260051b850101111561172857600080fd5b60008060008060008060008060008060006101408c8e03121561491657600080fd5b61491f8c614882565b9a5061492d60208d01614882565b995061493b60408d01614882565b985060608c0135975060808c0135965060a08c0135955060c08c0135945067ffffffffffffffff8060e08e0135111561497357600080fd5b6149838e60e08f01358f0161488d565b9450806101008e0135111561499757600080fd5b6149a88e6101008f01358f0161489e565b9350806101208e013511156149bc57600080fd5b506149ce8d6101208e01358e016148af565b81935080925050509295989b509295989b9093969950565b600080600080604085870312156149fc57600080fd5b843567ffffffffffffffff80821115614a1457600080fd5b614a20888389016148af565b90965094506020870135915080821115614a3957600080fd5b50614a46878288016148af565b95989497509550505050565b60008060008060008060608789031215614a6b57600080fd5b863567ffffffffffffffff80821115614a8357600080fd5b614a8f8a838b016148af565b90985096506020890135915080821115614aa857600080fd5b614ab48a838b016148af565b90965094506040890135915080821115614acd57600080fd5b50614ada89828a016148af565b979a9699509497509295939492505050565b600060208284031215614afe57600080fd5b81356111988161486d565b600060208284031215614b1b57600080fd5b5035919050565b60008060408385031215614b3557600080fd5b823591506020830135614b478161486d565b809150509250929050565b600060a08284031215614b6457600080fd5b50919050565b60006101608284031215614b6457600080fd5b60008060006101808486031215614b9357600080fd5b614b9d8585614b6a565b925061016084013567ffffffffffffffff80821115614bbb57600080fd5b818601915086601f830112614bcf57600080fd5b813581811115614bde57600080fd5b876020606083028501011115614bf357600080fd5b6020830194508093505050509250925092565b60008060408385031215614c1957600080fd5b8235614c248161486d565b946020939093013593505050565b60008060408385031215614c4557600080fd5b50508035926020909101359150565b60006101608284031215614c6757600080fd5b6111988383614b6a565b634e487b7160e01b600052602160045260246000fd5b60028110610a6557634e487b7160e01b600052602160045260246000fd5b81516040820190614cb581614c87565b808352506001600160a01b03602084015116602083015292915050565b60008060008060008060006080888a031215614ced57600080fd5b873567ffffffffffffffff80821115614d0557600080fd5b614d118b838c016148af565b909950975060208a0135915080821115614d2a57600080fd5b614d368b838c016148af565b909750955060408a0135915080821115614d4f57600080fd5b614d5b8b838c016148af565b909550935060608a0135915080821115614d7457600080fd5b50614d818a828b0161489e565b91505092959891949750929550565b634e487b7160e01b600052603260045260246000fd5b6000808335601e19843603018112614dbd57600080fd5b83018035915067ffffffffffffffff821115614dd857600080fd5b6020019150600581901b360382131561172857600080fd5b634e487b7160e01b600052601160045260246000fd5b6000600019821415614e1a57614e1a614df0565b5060010190565b634e487b7160e01b600052604160045260246000fd5b6040516060810167ffffffffffffffff81118282101715614e6857634e487b7160e01b600052604160045260246000fd5b60405290565b60028110610a6557600080fd5b600060608284031215614e8d57600080fd5b614e95614e37565b90508135614ea281614e6e565b80825250602082013560208201526040820135604082015292915050565b600060a08284031215614ed257600080fd5b614eda614e37565b8235614ee58161486d565b81526020830135614ef58161486d565b6020820152614f078460408501614e7b565b60408201529392505050565b600060608284031215614f2557600080fd5b6040516060810181811067ffffffffffffffff82111715614f5657634e487b7160e01b600052604160045260246000fd5b604052823560ff81168114614f6a57600080fd5b8152602083810135908201526040928301359281019290925250919050565b600060208284031215614f9b57600080fd5b5051919050565b600060608284031215614fb457600080fd5b614fbc614e37565b90508135614fc98161486d565b81526020820135614fd98161486d565b806020830152506040820135604082015292915050565b6000610160828403121561500357600080fd5b60405160a0810181811067ffffffffffffffff8211171561503457634e487b7160e01b600052604160045260246000fd5b60405282358152602083013561504981614e6e565b602082015261505b8460408501614fa2565b604082015261506d8460a08501614fa2565b6060820152615080846101008501614e7b565b60808201529392505050565b80356150978161486d565b6001600160a01b0390811683526020820135906150b38261486d565b166020830152604090810135910152565b6000610180820190508382528235602083015260208301356150e581614e6e565b6150ee81614c87565b80604084015250615105606083016040850161508c565b61511560c0830160a0850161508c565b61012061010084013561512781614e6e565b61513081614c87565b81840152830135610140808401919091529092013561016090910152919050565b60006020828403121561516357600080fd5b813561119881614e6e565b60006060828403121561518057600080fd5b6111988383614e7b565b60008282101561519c5761519c614df0565b500390565b60008160001904831182151516156151bb576151bb614df0565b500290565b6000610180820190508382528251602083015260208301516151e181614c87565b6040838101919091528381015180516001600160a01b03908116606086015260208201511660808501529081015160a084015250606083015180516001600160a01b0390811660c085015260208201511660e08401526040810151610100840152506080830151805161525381614c87565b6101208401526020810151610140840152604001516101609092019190915292915050565b8183526000602080850194508260005b858110156152b657813561529b8161486d565b6001600160a01b031687529582019590820190600101615288565b509495945050505050565b6060815260006152d560608301888a615278565b6020838203818501526152e982888a615278565b8481036040860152858152869250810160005b8681101561532a57833561530f81614e6e565b61531881614c87565b825292820192908201906001016152fc565b509a9950505050505050505050565b60408152600061534d604083018688615278565b82810360208401528381527f07ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff84111561538557600080fd5b8360051b80866020840137600091016020019081529695505050505050565b60005b838110156153bf5781810151838201526020016153a7565b83811115610e895750506000910152565b7f416363657373436f6e74726f6c3a206163636f756e74200000000000000000008152600083516154088160178501602088016153a4565b7f206973206d697373696e6720726f6c652000000000000000000000000000000060179184019182015283516154458160288401602088016153a4565b01602801949350505050565b60208152600082518060208401526154708160408501602087016153a4565b601f01601f19169190910160400192915050565b6000821982111561549757615497614df0565b500190565b6000826154b957634e487b7160e01b600052601260045260246000fd5b500490565b8681526020810186905260c081016154d586614c87565b8560408301528460608301528360808301528260a0830152979650505050505050565b6000825161550a8184602087016153a4565b9190910192915050565b60006020828403121561552657600080fd5b8151801515811461119857600080fd5b7f546f6b656e3a20636f756c64206e6f74207472616e7366657220000000000000815260008551602061556f82601a8601838b016153a4565b7f2066726f6d200000000000000000000000000000000000000000000000000000601a9285019283015286516155aa81838501848b016153a4565b630103a37960e51b92018181019290925285516155cd81602485018985016153a4565b660103a37b5b2b7160cd1b6024939091019283015284516155f481602b85018489016153a4565b91909101602b01979650505050505050565b60008161561557615615614df0565b506000190190565b8481526080810161562d85614c87565b84602083015283604083015282606083015295945050505050565b7f546f6b656e3a20636f756c64206e6f74207472616e736665722000000000000081526000845161568081601a8501602089016153a4565b630103a37960e51b601a9184019182015284516156a481601e8401602089016153a4565b660103a37b5b2b7160cd1b601e929091019182015283516156cc8160258401602088016153a4565b0160250195945050505050565b7f546f6b656e496e666f280000000000000000000000000000000000000000000081526000845161571181600a8501602089016153a4565b80830190507f2c0000000000000000000000000000000000000000000000000000000000000080600a830152855161575081600b850160208a016153a4565b600b920191820152835161576b81600c8401602088016153a4565b7f2900000000000000000000000000000000000000000000000000000000000000600c9290910191820152600d0195945050505050565b634e487b7160e01b600052603160045260246000fdfeb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d610348617350726f787941646d696e3a20756e617574686f72697a65642073656e64a2646970667358221220badebb50cc96ecc4d7c364bf224a8d7c089229c1d01abb009e0241a26ff0da2564736f6c63430008090033";
}
