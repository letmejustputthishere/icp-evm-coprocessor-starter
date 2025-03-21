#![cfg(test)]

use alloy::{hex::FromHex, primitives::Address};
use candid::{decode_one, CandidType, Encode, Principal};
use evm_rpc_canister_types::RpcService;
use helpers::{
    evm::EvmEnv,
    http_outcalls::handle_http_outcalls,
    icp::{query, update, Canister, EmptyRecord},
};
use ic_cdk::api::management_canister::{ecdsa::EcdsaKeyId, main::CanisterId};
use lazy_static::lazy_static;
use pocket_ic::{
    management_canister::CanisterSettings, nonblocking::PocketIc, PocketIcBuilder, RejectResponse,
};
use serde::Deserialize;
use std::{
    marker::PhantomData,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tokio::{sync::Mutex, task};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct InitArg {
    pub rpc_service: RpcService,
    pub chain_id: u64,
    pub filter_addresses: Vec<String>,
    pub coprocessor_evm_address: String,
    pub filter_events: Vec<String>,
    pub ecdsa_key_id: EcdsaKeyId,
}

mod helpers;
mod tests;

mod chain_fusion;

lazy_static! {
    static ref WORKSPACE_ROOT: PathBuf = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .expect("Failed to get workspace root")
        .workspace_root
        .into();
}

struct TestEnv {
    pic: Arc<Mutex<PocketIc>>,
    user: Principal,
    chain_fusion: CanisterId,
    evm: EvmEnv,
}

impl TestEnv {
    async fn new() -> Self {
        std::env::set_var("RUST_LOG", "error");

        let evm = EvmEnv::new().await;

        let pic = PocketIcBuilder::new()
            .with_nns_subnet()
            .with_ii_subnet()
            .build_async()
            .await;

        pic.set_time(
            SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(1738933200))
                .unwrap(),
        )
        .await;

        let controller =
            Principal::from_text("6vui3-u5w5r-6ks6a-ojcem-giomu-gmfhx-bjohd-kpkos-3xlmt-n7bn7-wae")
                .unwrap();

        let user =
            Principal::from_text("y3flu-q4efd-gss2z-iz4vu-eyroy-blzhh-4f27c-y5fsx-h7ckx-un4vy-4ae")
                .unwrap();

        let evm_rpc = pic
            .create_canister_with_id(Some(controller), None, Canister::EvmRpc.id())
            .await
            .unwrap();
        pic.add_cycles(evm_rpc, u64::MAX.into()).await;
        pic.install_canister(
            evm_rpc,
            Canister::EvmRpc.wasm(),
            Encode!(&EmptyRecord {}).unwrap(),
            Some(controller),
        )
        .await;

        let provider = Provider {
            pic: Arc::new(Mutex::new(pic)),
            default_caller: controller,
        };

        let rpc_node_url = "http://localhost:8545".to_string();
        let chain_fusion = chain_fusion::deploy(
            &provider,
            chain_fusion::InitArg {
                ecdsa_key_id: chain_fusion::EcdsaKeyId {
                    curve: chain_fusion::EcdsaCurve::Secp256K1,
                    name: "dfx_test_key".to_string(),
                },
                rpc_service: chain_fusion::RpcService::Custom(chain_fusion::RpcApi {
                    url: rpc_node_url.clone(),
                    headers: None,
                }),
                filter_addresses: vec![evm.contract.to_string()],
                coprocessor_evm_address: evm.contract.to_string(),
                filter_events: vec!["NewJob(uint256)".to_string()],
                chain_id: 31337,
            },
        )
        .with_controller(controller)
        .with_cycles(u64::MAX.into())
        .with_wasm(Canister::ChainFusion.wasm())
        .call()
        .await
        .unwrap();

        let chain_fusion = chain_fusion.canister_id;

        let test = TestEnv {
            pic: Arc::clone(&provider.pic),
            user,
            chain_fusion,
            evm,
        };

        while test.get_evm_address().await.is_none() {
            test.tick().await;
        }

        let canister_evm_address =
            Address::from_hex(test.get_evm_address().await.unwrap()).unwrap();

        test.evm.update_coprocessor(canister_evm_address).await;

        test.evm.transfer_eth(canister_evm_address, "1").await;

        let pic = Arc::downgrade(&test.pic);
        task::spawn(handle_http_outcalls(
            pic,
            test.evm.anvil_url.clone(),
            vec![rpc_node_url],
        ));
        test
    }

    pub async fn tick(&self) {
        let pic = self.pic.lock().await;
        pic.advance_time(Duration::from_secs(1)).await;
        pic.tick().await;
    }

    pub fn provider(&self) -> Provider {
        Provider {
            pic: Arc::clone(&self.pic),
            default_caller: self.user,
        }
    }

    #[allow(dead_code)]
    async fn update<T>(
        &self,
        canister: CanisterId,
        caller: Principal,
        method: &str,
        arg: impl CandidType,
    ) -> Result<T, String>
    where
        T: for<'a> Deserialize<'a> + CandidType,
    {
        let pic = self.pic.lock().await;
        update(&pic, canister, caller, method, arg).await
    }

    async fn query<T>(
        &self,
        canister: CanisterId,
        caller: Principal,
        method: &str,
        arg: impl CandidType,
    ) -> Result<T, String>
    where
        T: for<'a> Deserialize<'a> + CandidType,
    {
        let pic = self.pic.lock().await;
        query(&pic, canister, caller, method, arg).await
    }

    pub async fn get_evm_address(&self) -> Option<String> {
        self.query::<Option<String>>(self.chain_fusion, self.user, "get_evm_address", ())
            .await
            .unwrap()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to candid encode call arguments: {}", .0)]
    ArgumentEncoding(candid::error::Error),
    #[error("canister rejected: {}, error_code: {}", .0.reject_message, .0.error_code)]
    Reject(RejectResponse),
    #[error("failed to candid decode call result: {}", .0)]
    ResultDecoding(candid::error::Error),
    #[error("canister creation failed: {}", .0)]
    CreateCanister(String),
    #[error("canister id is missing")]
    UnspecifiedCanister,
}

pub enum CallMode {
    Query,
    Update,
}

pub struct CallBuilder<R: for<'a> Deserialize<'a> + CandidType> {
    pic: Arc<Mutex<PocketIc>>,
    caller: Principal,
    canister_id: Principal,
    call_mode: CallMode,
    method: String,
    args: Result<Vec<u8>, candid::error::Error>,
    _result: PhantomData<R>,
}

impl<R: for<'a> Deserialize<'a> + CandidType> CallBuilder<R> {
    pub fn with_caller(self, caller: Principal) -> Self {
        Self { caller, ..self }
    }

    pub fn with_update(self) -> Self {
        Self {
            call_mode: CallMode::Update,
            ..self
        }
    }

    pub async fn call(self) -> Result<R, Error> {
        let args = self.args.map_err(Error::ArgumentEncoding)?;

        let pic = self.pic.lock().await;
        let result = match self.call_mode {
            CallMode::Query => {
                pic.query_call(self.canister_id, self.caller, &self.method, args)
                    .await
            }
            CallMode::Update => {
                pic.update_call(self.canister_id, self.caller, &self.method, args)
                    .await
            }
        };

        let reply = result.map_err(Error::Reject)?;

        decode_one(&reply).map_err(Error::ResultDecoding)
    }
}

pub enum DeployMode {
    Create,
    Install,
    Reinstall,
    Upgrade,
}

pub struct DeployBuilder<C> {
    pic: Arc<Mutex<PocketIc>>,
    caller: Principal,
    canister_id: Option<Principal>,
    mode: DeployMode,
    settings: CanisterSettings,
    cycles: u128,
    wasm: Vec<u8>,
    args: Result<Vec<u8>, candid::error::Error>,
    instance: Box<dyn FnOnce(Principal) -> C>,
}

impl<C> DeployBuilder<C> {
    pub fn with_canister_id(self, canister_id: Principal) -> Self {
        Self {
            canister_id: Some(canister_id),
            ..self
        }
    }
    pub fn with_controller(self, controller: Principal) -> Self {
        Self {
            caller: controller,
            ..self
        }
    }

    pub fn with_controllers(self, controllers: Vec<Principal>) -> Self {
        let result = Self {
            settings: CanisterSettings {
                controllers: Some(controllers.clone()),
                ..self.settings
            },
            ..self
        };
        if let Some(caller) = controllers.first().cloned() {
            Self { caller, ..result }
        } else {
            result
        }
    }

    pub fn with_cycles(self, cycles: u128) -> Self {
        Self { cycles, ..self }
    }

    pub fn with_settings(self, settings: CanisterSettings) -> Self {
        let caller = settings
            .controllers
            .as_ref()
            .and_then(|c| c.first().cloned());
        let result = Self { settings, ..self };
        if let Some(caller) = caller {
            Self { caller, ..result }
        } else {
            result
        }
    }

    pub fn with_wasm(self, wasm: Vec<u8>) -> Self {
        Self { wasm, ..self }
    }

    pub fn with_install(self) -> Self {
        Self {
            mode: DeployMode::Install,
            ..self
        }
    }

    pub fn with_upgrade(self) -> Self {
        Self {
            mode: DeployMode::Upgrade,
            ..self
        }
    }

    pub fn with_reinstall(self) -> Self {
        Self {
            mode: DeployMode::Reinstall,
            ..self
        }
    }

    pub async fn call(self) -> Result<C, Error> {
        let args = self.args.map_err(Error::ArgumentEncoding)?;

        let pic = self.pic.lock().await;

        let canister_id = if let DeployMode::Create = self.mode {
            match self.canister_id {
                Some(canister_id) => pic
                    .create_canister_with_id(Some(self.caller), Some(self.settings), canister_id)
                    .await
                    .map_err(Error::CreateCanister)?,
                None => {
                    pic.create_canister_with_settings(Some(self.caller), Some(self.settings))
                        .await
                }
            }
        } else {
            match self.canister_id {
                Some(canister_id) => canister_id,
                None => {
                    return Err(Error::UnspecifiedCanister);
                }
            }
        };

        pic.add_cycles(canister_id, self.cycles).await;

        match self.mode {
            DeployMode::Create | DeployMode::Install => {
                pic.install_canister(canister_id, self.wasm, args, Some(self.caller))
                    .await;
            }
            DeployMode::Reinstall => {
                pic.reinstall_canister(canister_id, self.wasm, args, Some(self.caller))
                    .await
                    .map_err(Error::Reject)?;
            }
            DeployMode::Upgrade => {
                pic.upgrade_canister(canister_id, self.wasm, args, Some(self.caller))
                    .await
                    .map_err(Error::Reject)?;
            }
        }

        Ok((self.instance)(canister_id))
    }
}

#[derive(Clone)]
pub struct Provider {
    pic: Arc<Mutex<PocketIc>>,
    default_caller: Principal,
}

impl Provider {
    fn call<R>(
        &self,
        canister_id: Principal,
        call_kind: CallMode,
        method: &str,
        args: Result<Vec<u8>, candid::error::Error>,
    ) -> CallBuilder<R>
    where
        R: for<'a> Deserialize<'a> + CandidType,
    {
        CallBuilder {
            pic: Arc::clone(&self.pic),
            caller: self.default_caller,
            canister_id,
            call_mode: call_kind,
            method: method.to_string(),
            args,
            _result: PhantomData {},
        }
    }

    fn deploy<C>(
        &self,
        args: Result<Vec<u8>, candid::error::Error>,
        instance: Box<dyn FnOnce(Principal) -> C>,
    ) -> DeployBuilder<C> {
        DeployBuilder {
            pic: Arc::clone(&self.pic),
            caller: self.default_caller,
            canister_id: None,
            mode: DeployMode::Create,
            settings: CanisterSettings::default(),
            cycles: u64::MAX as u128,
            wasm: vec![],
            args,
            instance,
        }
    }
}
