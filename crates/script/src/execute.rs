use crate::{
    build::{CompiledState, LinkedBuildData},
    simulate::PreSimulationState,
    ScriptArgs, ScriptConfig,
};

use super::{runner::ScriptRunner, JsonResult, NestedValue, ScriptResult};
use alloy_dyn_abi::FunctionExt;
use alloy_json_abi::{Function, InternalType, JsonAbi};
use alloy_primitives::{Address, Bytes};
use alloy_provider::Provider;
use async_recursion::async_recursion;
use eyre::{eyre, Context, OptionExt, Result};
use foundry_cheatcodes::ScriptWallets;
use foundry_cli::{
    opts::RpcOpts,
    utils::{ensure_clean_constructor, needs_setup},
};
use foundry_common::{
    fmt::{format_token, format_token_raw},
    provider::get_http_provider,
    shell, ContractsByArtifact,
};
use foundry_config::{Config, NamedChain};
use foundry_debugger::Debugger;
use foundry_evm::{
    decode::decode_console_logs,
    inspectors::cheatcodes::BroadcastableTransactions,
    revm::Database,
    traces::{
        decode_trace_arena,
        identifier::{SignaturesIdentifier, TraceIdentifiers},
        render_trace_arena, CallTraceDecoder, CallTraceDecoderBuilder, TraceKind,
    },
};
use foundry_tweak::{build_tweak_data, tweak_backend, TweakData};
use futures::future::join_all;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};
use yansi::Paint;

/// State after linking, contains the linked build data along with library addresses and optional
/// array of libraries that need to be predeployed.
pub struct LinkedState {
    pub args: ScriptArgs,
    pub script_config: ScriptConfig,
    pub script_wallets: ScriptWallets,
    pub build_data: LinkedBuildData,
}

/// Container for data we need for execution which can only be obtained after linking stage.
#[derive(Debug)]
pub struct ExecutionData {
    /// Function to call.
    pub func: Function,
    /// Calldata to pass to the target contract.
    pub calldata: Bytes,
    /// Bytecode of the target contract.
    pub bytecode: Bytes,
    /// ABI of the target contract.
    pub abi: JsonAbi,
}

impl LinkedState {
    /// Given linked and compiled artifacts, prepares data we need for execution.
    /// This includes the function to call and the calldata to pass to it.
    pub async fn prepare_execution(self) -> Result<PreExecutionState> {
        let Self { args, script_config, script_wallets, build_data } = self;

        // Please print the script_config details: such as script_config.evm_opts.env.
        info!("script_config.evm_opts.env.chain_id: {:?}", script_config.evm_opts.env.chain_id);
        info!("script_config.evm_opts.fork_url: {:?}", script_config.evm_opts.fork_url);
        info!("script_config.evm_opts.fork_block_number: {:?}", script_config.evm_opts.fork_block_number);
        info!("script_config.evm_opts.env.block_number: {:?}", script_config.evm_opts.env.block_number);
        info!("script_config.evm_opts.env.block_timestamp: {:?}", script_config.evm_opts.env.block_timestamp);
        info!("script_config.evm_opts.env.block_difficulty: {:?}", script_config.evm_opts.env.block_difficulty);
        info!("script_config.evm_opts.env.block_prevrandao: {:?}", script_config.evm_opts.env.block_prevrandao);
        info!("script_config.evm_opts.env.block_gas_limit: {:?}", script_config.evm_opts.env.block_gas_limit);
        info!("script_config.evm_opts.env.block_base_fee_per_gas: {:?}", script_config.evm_opts.env.block_base_fee_per_gas);
        info!("script_config.evm_opts.env.block_coinbase: {:?}", script_config.evm_opts.env.block_coinbase);

        let target_contract = build_data.get_target_contract()?;
        info!("target_contract: {:?}", target_contract.name);

        let bytecode = target_contract.bytecode().ok_or_eyre("target contract has no bytecode")?;

        let (func, calldata) = args.get_method_and_calldata(&target_contract.abi)?;
        info!("func: {:?}", func.name);

        ensure_clean_constructor(&target_contract.abi)?;

        // prepare tweak
        let tweak = if !args.tweak.is_empty() {
            let cloned_projects = args
                .tweak
                .iter()
                .map(|path| {
                    let path = dunce::canonicalize(path)
                        .map_err(|e| eyre!("Failed to load tweak project: {:?}", e))?;
                    foundry_tweak::ClonedProject::load_with_root(&path).wrap_err_with(|| {
                        format!("Failed to load tweak project from path: {:?}", &path)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let rpc = RpcOpts {
                url: script_config.evm_opts.fork_url.clone().or(args
                    .evm_opts
                    .fork_url
                    .clone()
                    .or(Some("http://localhost:8545".to_string()))),
                ..Default::default()
            };

            build_tweak_data(&cloned_projects, &rpc, args.tweak_quick).await?
        } else {
            TweakData::new()
        };

        // Log tweak data for debugging
        for (addr, info) in tweak.iter() {
            info!("execute.rs: Tweak data loaded - {} at Address: {:?}, Code length: {} bytes, ABI items: {}", 
                info.name, addr, info.bytecode.len(), info.abi.len());
            if info.bytecode.len() > 64 {
                info!("execute.rs: Code preview - First 32 bytes: {:02x?}", &info.bytecode[..32]);
                info!("execute.rs: Code preview - Last 32 bytes: {:02x?}", &info.bytecode[info.bytecode.len()-32..]);
            }
        }

        Ok(PreExecutionState {
            args,
            script_config,
            script_wallets,
            execution_data: ExecutionData {
                func,
                calldata,
                bytecode: bytecode.clone(),
                abi: target_contract.abi.clone(),
            },
            build_data,
            tweak_data: tweak,
        })
    }
}

/// Same as [LinkedState], but also contains [ExecutionData].
#[derive(Debug)]
pub struct PreExecutionState {
    pub args: ScriptArgs,
    pub script_config: ScriptConfig,
    pub script_wallets: ScriptWallets,
    pub build_data: LinkedBuildData,
    pub execution_data: ExecutionData,
    pub tweak_data: TweakData,
}

impl PreExecutionState {
    /// Executes the script and returns the state after execution.
    /// Might require executing script twice in cases when we determine sender from execution.
    #[async_recursion]
    pub async fn execute(mut self) -> Result<ExecutedState> {
        // Log before applying tweak
        info!("execute.rs: About to apply tweak to backend with {} addresses", self.tweak_data.len());
        for (addr, tweak_info) in self.tweak_data.iter() {
            info!("execute.rs: Applying tweak - {} at Address: {:?}, Code length: {} bytes", 
                tweak_info.name, addr, tweak_info.bytecode.len());
        }
        
        // Apply tweak BEFORE creating executor (like forge replay does)
        let mut runner = self
            .script_config
            .get_runner_with_cheatcodes(
                self.build_data.known_contracts.clone(),
                self.script_wallets.clone(),
                self.args.debug,
                self.build_data.build_data.target.clone(),
                Some(&self.tweak_data),
            )
            .await?;
        
        // Verify tweak was applied
        for (addr, tweak_info) in self.tweak_data.iter() {
            let account_info = runner.executor.backend_mut().basic(*addr)?;
            if let Some(info) = account_info {
                if let Some(ref code) = info.code {
                    info!("execute.rs: Verification AFTER tweak - {} at Address {:?} has code length: {} bytes", 
                        tweak_info.name, addr, code.len());
                    
                    // Check if code matches what we expect
                    let code_bytes = code.original_bytes();
                    if code_bytes.as_ref() == tweak_info.bytecode.as_ref() {
                        info!("execute.rs: ✓ Code matches expected tweak");
                    } else {
                        warn!("execute.rs: ✗ Code DOES NOT match expected tweak! Expected {} bytes, got {} bytes", 
                            tweak_info.bytecode.len(), code_bytes.len());
                    }
                } else {
                    warn!("execute.rs: Verification - Address {:?} has NO code after tweak!", addr);
                }
            } else {
                warn!("execute.rs: Verification - Address {:?} has NO account info after tweak!", addr);
            }
        }
        
        let result = self.execute_with_runner(&mut runner).await?;
        
        // IMPORTANT: Verify code AGAIN after execution to see if it changed
        info!("execute.rs: Verifying code AFTER execution...");
        for (addr, tweak_info) in self.tweak_data.iter() {
            let account_info = runner.executor.backend_mut().basic(*addr)?;
            if let Some(info) = account_info {
                if let Some(ref code) = info.code {
                    info!("execute.rs: AFTER execution - {} at Address {:?} has code length: {} bytes", 
                        tweak_info.name, addr, code.len());
                    let code_bytes = code.original_bytes();
                    if code_bytes.as_ref() == tweak_info.bytecode.as_ref() {
                        info!("execute.rs: ✓ Code STILL matches expected tweak");
                    } else {
                        warn!("execute.rs: ✗ Code changed during execution! Expected {} bytes, got {} bytes", 
                            tweak_info.bytecode.len(), code_bytes.len());
                    }
                }
            }
        }

        // If we have a new sender from execution, we need to use it to deploy libraries and relink
        // contracts.
        if let Some(new_sender) = self.maybe_new_sender(result.transactions.as_ref())? {
            self.script_config.update_sender(new_sender).await?;

            // Rollback to rerun linking with the new sender.
            let state = CompiledState {
                args: self.args,
                script_config: self.script_config,
                script_wallets: self.script_wallets,
                build_data: self.build_data.build_data,
            };

            return state.link().await?.prepare_execution().await?.execute().await;
        }

        Ok(ExecutedState {
            args: self.args,
            script_config: self.script_config,
            script_wallets: self.script_wallets,
            build_data: self.build_data,
            execution_data: self.execution_data,
            execution_result: result,
            tweak_data: self.tweak_data,
        })
    }

    /// Executes the script using the provided runner and returns the [ScriptResult].
    pub async fn execute_with_runner(&self, runner: &mut ScriptRunner) -> Result<ScriptResult> {
        let (address, mut setup_result) = runner.setup(
            &self.build_data.predeploy_libraries,
            self.execution_data.bytecode.clone(),
            needs_setup(&self.execution_data.abi),
            self.script_config.sender_nonce,
            self.args.broadcast,
            self.script_config.evm_opts.fork_url.is_none(),
        )?;

        if setup_result.success {
            let script_result = runner.script(address, self.execution_data.calldata.clone())?;

            setup_result.success &= script_result.success;
            setup_result.gas_used = script_result.gas_used;
            setup_result.logs.extend(script_result.logs);
            setup_result.traces.extend(script_result.traces);
            setup_result.labeled_addresses.extend(script_result.labeled_addresses);
            setup_result.returned = script_result.returned;
            setup_result.breakpoints = script_result.breakpoints;

            match (&mut setup_result.transactions, script_result.transactions) {
                (Some(txs), Some(new_txs)) => {
                    txs.extend(new_txs);
                }
                (None, Some(new_txs)) => {
                    setup_result.transactions = Some(new_txs);
                }
                _ => {}
            }
        }

        Ok(setup_result)
    }

    /// It finds the deployer from the running script and uses it to predeploy libraries.
    ///
    /// If there are multiple candidate addresses, it skips everything and lets `--sender` deploy
    /// them instead.
    fn maybe_new_sender(
        &self,
        transactions: Option<&BroadcastableTransactions>,
    ) -> Result<Option<Address>> {
        let mut new_sender = None;

        if let Some(txs) = transactions {
            // If the user passed a `--sender` don't check anything.
            if self.build_data.predeploy_libraries.libraries_count() > 0 &&
                self.args.evm_opts.sender.is_none()
            {
                for tx in txs.iter() {
                    if tx.transaction.to.is_none() {
                        let sender = tx.transaction.from.expect("no sender");
                        if let Some(ns) = new_sender {
                            if sender != ns {
                                shell::println("You have more than one deployer who could predeploy libraries. Using `--sender` instead.")?;
                                return Ok(None);
                            }
                        } else if sender != self.script_config.evm_opts.sender {
                            new_sender = Some(sender);
                        }
                    }
                }
            }
        }
        Ok(new_sender)
    }
}

/// Container for information about RPC-endpoints used during script execution.
pub struct RpcData {
    /// Unique list of rpc urls present.
    pub total_rpcs: HashSet<String>,
    /// If true, one of the transactions did not have a rpc.
    pub missing_rpc: bool,
}

impl RpcData {
    /// Iterates over script transactions and collects RPC urls.
    fn from_transactions(txs: &BroadcastableTransactions) -> Self {
        let missing_rpc = txs.iter().any(|tx| tx.rpc.is_none());
        let total_rpcs =
            txs.iter().filter_map(|tx| tx.rpc.as_ref().cloned()).collect::<HashSet<_>>();

        Self { total_rpcs, missing_rpc }
    }

    /// Returns true if script might be multi-chain.
    /// Returns false positive in case when missing rpc is the same as the only rpc present.
    pub fn is_multi_chain(&self) -> bool {
        self.total_rpcs.len() > 1 || (self.missing_rpc && !self.total_rpcs.is_empty())
    }

    /// Checks if all RPCs support EIP-3855. Prints a warning if not.
    async fn check_shanghai_support(&self) -> Result<()> {
        let chain_ids = self.total_rpcs.iter().map(|rpc| async move {
            let provider = get_http_provider(rpc);
            let id = provider.get_chain_id().await.ok()?;
            NamedChain::try_from(id).ok()
        });

        let chains = join_all(chain_ids).await;
        let iter = chains.iter().flatten().map(|c| (c.supports_shanghai(), c));
        if iter.clone().any(|(s, _)| !s) {
            let msg = format!(
                "\
EIP-3855 is not supported in one or more of the RPCs used.
Unsupported Chain IDs: {}.
Contracts deployed with a Solidity version equal or higher than 0.8.20 might not work properly.
For more information, please see https://eips.ethereum.org/EIPS/eip-3855",
                iter.filter(|(supported, _)| !supported)
                    .map(|(_, chain)| *chain as u64)
                    .format(", ")
            );
            shell::println(msg.yellow())?;
        }
        Ok(())
    }
}

/// Container for data being collected after execution.
pub struct ExecutionArtifacts {
    /// Trace decoder used to decode traces.
    pub decoder: CallTraceDecoder,
    /// Return values from the execution result.
    pub returns: HashMap<String, NestedValue>,
    /// Information about RPC endpoints used during script execution.
    pub rpc_data: RpcData,
}

/// State after the script has been executed.
pub struct ExecutedState {
    pub args: ScriptArgs,
    pub script_config: ScriptConfig,
    pub script_wallets: ScriptWallets,
    pub build_data: LinkedBuildData,
    pub execution_data: ExecutionData,
    pub execution_result: ScriptResult,
    pub tweak_data: TweakData,
}

impl ExecutedState {
    /// Collects the data we need for simulation and various post-execution tasks.
    pub async fn prepare_simulation(self) -> Result<PreSimulationState> {
        info!("get returns");
        let returns = self.get_returns()?;

        info!("build_trace_decoder with tweaked ABIs");
        let decoder = self.build_trace_decoder(&self.build_data.known_contracts, Some(&self.tweak_data)).await?;

        info!("transactions.clone()");
        let txs = self.execution_result.transactions.clone().unwrap_or_default();
        info!("RpcData::from_transactions");
        let rpc_data = RpcData::from_transactions(&txs);

        if rpc_data.is_multi_chain() {
            shell::eprintln(format!(
                "{}",
                "Multi chain deployment is still under development. Use with caution.".yellow()
            ))?;
            if !self.build_data.libraries.is_empty() {
                eyre::bail!(
                    "Multi chain deployment does not support library linking at the moment."
                )
            }
        }
        rpc_data.check_shanghai_support().await?;

        Ok(PreSimulationState {
            args: self.args,
            script_config: self.script_config,
            script_wallets: self.script_wallets,
            build_data: self.build_data,
            execution_data: self.execution_data,
            execution_result: self.execution_result,
            execution_artifacts: ExecutionArtifacts { decoder, returns, rpc_data },
        })
    }

    /// Builds [CallTraceDecoder] from the execution result and known contracts.
    async fn build_trace_decoder(
        &self,
        known_contracts: &ContractsByArtifact,
        tweak_data: Option<&TweakData>,
    ) -> Result<CallTraceDecoder> {
        info!("CallTraceDecoderBuilder::new");
        let mut decoder_builder = CallTraceDecoderBuilder::new()
            .with_labels(self.execution_result.labeled_addresses.clone())
            .with_verbosity(self.script_config.evm_opts.verbosity)
            .with_known_contracts(known_contracts)
            .with_signature_identifier(SignaturesIdentifier::new(
                Config::foundry_cache_dir(),
                self.script_config.config.offline,
            )?);
        
        // Add tweaked ABIs to the decoder if provided
        if let Some(tweaks) = tweak_data {
            for (address, tweak_info) in tweaks {
                let event_count = tweak_info.abi.events().count();
                let debug_log_events: Vec<_> = tweak_info.abi.events()
                    .filter(|e| e.name == "DebugLog" || e.name == "DebugLogNumber")
                    .collect();
                
                info!("build_trace_decoder: Adding tweaked ABI for {} at {:?} ({} total ABI items, {} events, {} DebugLog events)", 
                    tweak_info.name, address, tweak_info.abi.len(), event_count, debug_log_events.len());
                
                if !debug_log_events.is_empty() {
                    for event in &debug_log_events {
                        info!("build_trace_decoder: Found DebugLog event: {} with {} indexed inputs", 
                            event.name, event.inputs.iter().filter(|i| i.indexed).count());
                    }
                }
                
                decoder_builder = decoder_builder.with_abi(&tweak_info.abi);
            }
        }
        
        let mut decoder = decoder_builder.build();

        info!("TraceIdentifiers::new");
        let mut identifier = TraceIdentifiers::new().with_local(known_contracts).with_etherscan(
            &self.script_config.config,
            self.script_config.evm_opts.get_remote_chain_id().await,
        )?;

        // Decoding traces using etherscan is costly as we run into rate limits,
        // causing scripts to run for a very long time unnecessarily.
        // Therefore, we only try and use etherscan if the user has provided an API key.
        let should_use_etherscan_traces = self.script_config.config.etherscan_api_key.is_some();
        if !should_use_etherscan_traces {
            identifier.etherscan = None;
        }

        info!("identify");
        for (_, trace) in &self.execution_result.traces {
            decoder.identify(trace, &mut identifier);
        }

        Ok(decoder)
    }

    /// Collects the return values from the execution result.
    fn get_returns(&self) -> Result<HashMap<String, NestedValue>> {
        let mut returns = HashMap::new();
        let returned = &self.execution_result.returned;
        let func = &self.execution_data.func;

        match func.abi_decode_output(returned, false) {
            Ok(decoded) => {
                for (index, (token, output)) in decoded.iter().zip(&func.outputs).enumerate() {
                    let internal_type =
                        output.internal_type.clone().unwrap_or(InternalType::Other {
                            contract: None,
                            ty: "unknown".to_string(),
                        });

                    let label = if !output.name.is_empty() {
                        output.name.to_string()
                    } else {
                        index.to_string()
                    };

                    returns.insert(
                        label,
                        NestedValue {
                            internal_type: internal_type.to_string(),
                            value: format_token_raw(token),
                        },
                    );
                }
            }
            Err(_) => {
                shell::println(format!("{returned:?}"))?;
            }
        }

        Ok(returns)
    }
}

impl PreSimulationState {
    pub fn show_json(&self) -> Result<()> {
        let result = &self.execution_result;

        let console_logs = decode_console_logs(&result.logs);
        let output = JsonResult {
            logs: console_logs,
            gas_used: result.gas_used,
            returns: self.execution_artifacts.returns.clone(),
        };
        let j = serde_json::to_string(&output)?;
        shell::println(j)?;

        if !self.execution_result.success {
            return Err(eyre::eyre!(
                "script failed: {}",
                &self.execution_artifacts.decoder.revert_decoder.decode(&result.returned[..], None)
            ));
        }

        Ok(())
    }

    pub async fn show_traces(&self) -> Result<()> {
        let verbosity = self.script_config.evm_opts.verbosity;
        let func = &self.execution_data.func;
        let result = &self.execution_result;
        let decoder = &self.execution_artifacts.decoder;

        if !result.success || verbosity > 3 {
            if result.traces.is_empty() {
                warn!(verbosity, "no traces");
            }

            shell::println("Traces:")?;
            for (kind, trace) in &result.traces {
                let should_include = match kind {
                    TraceKind::Setup => verbosity >= 5,
                    TraceKind::Execution => verbosity > 3,
                    _ => false,
                } || !result.success;

                if should_include {
                    let mut trace = trace.clone();
                    decode_trace_arena(&mut trace, decoder).await?;
                    shell::println(render_trace_arena(&trace))?;
                }
            }
            shell::println(String::new())?;
        }

        if result.success {
            shell::println(format!("{}", "Script ran successfully.".green()))?;
        }

        if self.script_config.evm_opts.fork_url.is_none() {
            shell::println(format!("Gas used: {}", result.gas_used))?;
        }

        if result.success && !result.returned.is_empty() {
            shell::println("\n== Return ==")?;
            match func.abi_decode_output(&result.returned, false) {
                Ok(decoded) => {
                    for (index, (token, output)) in decoded.iter().zip(&func.outputs).enumerate() {
                        let internal_type =
                            output.internal_type.clone().unwrap_or(InternalType::Other {
                                contract: None,
                                ty: "unknown".to_string(),
                            });

                        let label = if !output.name.is_empty() {
                            output.name.to_string()
                        } else {
                            index.to_string()
                        };
                        shell::println(format!(
                            "{}: {internal_type} {}",
                            label.trim_end(),
                            format_token(token)
                        ))?;
                    }
                }
                Err(_) => {
                    shell::println(format!("{:x?}", (&result.returned)))?;
                }
            }
        }

        use foundry_evm::constants::HARDHAT_CONSOLE_ADDRESS;
        
        info!("show_traces: Total logs collected: {}", result.logs.len());
        let mut console_address_logs = 0;
        let mut other_logs = 0;
        
        for (i, log) in result.logs.iter().enumerate() {
            let is_console = log.address == HARDHAT_CONSOLE_ADDRESS;
            if is_console {
                console_address_logs += 1;
            } else {
                other_logs += 1;
            }
            
            if i < 10 || is_console {  // Log first 10 or all console logs
                info!("show_traces: Log {}: address={:?} (is_console={}), topics={}, data_len={}", 
                    i, log.address, is_console, log.topics().len(), log.data.data.len());
                if is_console && !log.topics().is_empty() {
                    info!("show_traces:   topic0={:?}", log.topics()[0]);
                }
            }
        }
        
        info!("show_traces: Logs breakdown - {} from console address, {} from other addresses", 
            console_address_logs, other_logs);
        
        let console_logs = decode_console_logs(&result.logs);
        info!("show_traces: Decoded console logs: {} (expected ~{})", console_logs.len(), console_address_logs);
        if !console_logs.is_empty() {
            shell::println("\n== Logs ==")?;
            for log in console_logs {
                shell::println(format!("  {log}"))?;
            }
        } else if !result.logs.is_empty() {
            info!("show_traces: WARNING: {} logs collected but none decoded as console logs", result.logs.len());
        }

        if !result.success {
            return Err(eyre::eyre!(
                "script failed: {}",
                &self.execution_artifacts.decoder.revert_decoder.decode(&result.returned[..], None)
            ));
        }

        Ok(())
    }

    pub fn run_debugger(self) -> Result<()> {
        let mut debugger = Debugger::builder()
            .traces(
                self.execution_result
                    .traces
                    .into_iter()
                    .filter(|(t, _)| t.is_execution())
                    .collect(),
            )
            .decoder(&self.execution_artifacts.decoder)
            .sources(self.build_data.sources)
            .breakpoints(self.execution_result.breakpoints)
            .build();
        debugger.try_run()?;
        Ok(())
    }
}
