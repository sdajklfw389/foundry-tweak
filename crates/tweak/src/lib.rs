pub mod code;
pub mod compatibility;
mod constants;
mod metadata;

use std::collections::BTreeMap;

use alloy_json_abi::JsonAbi;
use alloy_primitives::{keccak256, Address, Bytes, B256};
use eyre::Result;

use foundry_cli::opts::RpcOpts;
use foundry_evm::{backend::Backend, fork::CreateFork};
pub use metadata::ClonedProject;
use revm::{
    primitives::{Bytecode, KECCAK_EMPTY},
    Database,
};
use tracing::{info, debug};

/// Tweak data contains both the bytecode and ABI for each tweaked contract
pub type TweakData = BTreeMap<Address, TweakInfo>;

/// Information about a tweaked contract
#[derive(Debug, Clone)]
pub struct TweakInfo {
    pub bytecode: Bytes,
    pub abi: JsonAbi,
    pub name: String,
}

pub async fn build_tweak_data(
    projects: &Vec<ClonedProject>,
    rpc: &RpcOpts,
    quick: bool,
) -> Result<TweakData> {
    let mut tweak_data = BTreeMap::new();
    for project in projects {
        let metadata = &project.metadata;
        let address = metadata.address;
        let code = project.tweaked_code(rpc, quick).await?;
        let abi = project.get_abi()?;
        let name = metadata.target_contract.clone();
        
        info!("build_tweak_data: Adding tweak for {} at {:?} with {} ABI items", 
            name, address, abi.len());
        
        tweak_data.insert(address, TweakInfo {
            bytecode: code,
            abi,
            name,
        });
    }
    Ok(tweak_data)
}

pub fn build_tweaked_backend(fork: Option<CreateFork>, tweak_data: &TweakData) -> Result<Backend> {
    info!("build_tweak_backend");
    let mut backend = Backend::spawn(fork);
    for (address, info) in tweak_data {
        tweak_backend_once(&mut backend, *address, info.bytecode.clone())?;
    }
    Ok(backend)
}

/// Tweak the code of a contract in the blockchain backend.
pub fn tweak_backend_once(
    backend: &mut Backend,
    tweak_address: Address,
    tweaked_code: Bytes,
) -> Result<()> {
    let mut info = backend.basic(tweak_address)?.unwrap_or_default();
    let code_hash = if tweaked_code.as_ref().is_empty() {
        KECCAK_EMPTY
    } else {
        B256::from_slice(&keccak256(tweaked_code.as_ref())[..])
    };
    info.code_hash = code_hash;
    info.code = Some(Bytecode::new_raw(alloy_primitives::Bytes(tweaked_code.0)));
    backend.insert_account_info(tweak_address, info);
    Ok(())
}

pub fn tweak_backend(backend: &mut Backend, tweak_data: &TweakData) -> Result<()> {
    use revm::DatabaseCommit;
    use revm::primitives::{Account, HashMap};
    
    // Build the changes map that includes both account info AND the bytecode
    let mut changes = HashMap::new();
    
    for (tweak_address, tweak_info) in tweak_data {
        let tweaked_code = &tweak_info.bytecode;
        let mut acc_info = backend.basic(*tweak_address)?.unwrap_or_default();
        
        let code_hash = if tweaked_code.as_ref().is_empty() {
            revm::primitives::KECCAK_EMPTY
        } else {
            B256::from_slice(&alloy_primitives::keccak256(tweaked_code.as_ref())[..])
        };
        
        let bytecode = Bytecode::new_raw(alloy_primitives::Bytes(tweaked_code.clone().0));
        acc_info.code_hash = code_hash;
        acc_info.code = Some(bytecode.clone());
        
        // Create an Account with the tweaked code
        let account = Account {
            info: acc_info,
            storage: HashMap::new(), // Preserve existing storage
            status: revm::primitives::AccountStatus::Touched,
        };
        
        changes.insert(*tweak_address, account);
        
        info!("tweak_backend: Prepared tweak for {} at address {:?} with {} bytes of code (hash: {:?})", 
            tweak_info.name, tweak_address, tweaked_code.len(), code_hash);
    }
    
    // Commit all changes at once - this should properly update the CacheDB's contracts map
    backend.commit(changes);
    
    info!("tweak_backend: Committed {} tweaked addresses", tweak_data.len());

    Ok(())
}
