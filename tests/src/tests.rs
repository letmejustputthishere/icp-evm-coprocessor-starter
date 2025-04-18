use std::path::PathBuf;

use alloy::{
    hex::FromHex,
    primitives::{utils::parse_ether, Address, Uint, U256},
};
use candid::Principal;
use ic_test::{EvmUser, IcpTest, IcpUser};

use crate::bindings::{
    chain_fusion::{self, ChainFusionCanister},
    evm::Coprocessor::{self, CoprocessorInstance},
    evm_rpc::{self, EvmRpcCanister},
};

struct Env {
    test: IcpTest,
    evm_rpc: EvmRpcCanister,
    chain_fusion: ChainFusionCanister,
    coprocessor: CoprocessorInstance<(), EvmUser>,
    evm_user: EvmUser,
}

async fn setup(test: IcpTest) -> Env {
    let evm_user = test.evm.test_user(0);
    let icp_user = test.icp.test_user(0);

    let coprocessor = Coprocessor::deploy(evm_user.clone()).await.unwrap();

    let evm_rpc = evm_rpc::deploy(
        &icp_user,
        evm_rpc::InstallArgs {
            logFilter: None,
            demo: None,
            manageApiKeys: None,
            overrideProvider: None,
            nodesInSubnet: None,
        },
    )
    .call()
    .await;

    let chain_fusion = chain_fusion::deploy(
        &icp_user,
        chain_fusion::InitArg {
            ecdsa_key_id: chain_fusion::EcdsaKeyId {
                curve: chain_fusion::EcdsaCurve::Secp256K1,
                name: "dfx_test_key".to_string(),
            },
            rpc_service: chain_fusion::RpcService::Custom(chain_fusion::RpcApi {
                url: test.evm.rpc_url().to_string(),
                headers: None,
            }),
            chain_id: test.evm.chain_id(),
            filter_addresses: vec![coprocessor.address().to_string()],
            coprocessor_evm_address: coprocessor.address().to_string(),
            filter_events: vec!["NewJob(uint256)".to_string()],
        },
    )
    .call()
    .await;

    while chain_fusion.get_evm_address().call().await.is_none() {
        test.tick().await;
    }

    let canister_evm_address =
        Address::from_hex(chain_fusion.get_evm_address().call().await.unwrap()).unwrap();

    let receipt = coprocessor
        .updateCoprocessor(canister_evm_address)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    assert!(receipt.status());

    test.evm
        .transfer(
            &evm_user,
            canister_evm_address,
            parse_ether("0.01").unwrap(),
        )
        .await;

    Env {
        test,
        evm_user,
        evm_rpc,
        chain_fusion,
        coprocessor,
    }
}

#[tokio::test]
async fn test_coprocessor_job() {
    let Env {
        test,
        evm_user,
        evm_rpc,
        chain_fusion,
        coprocessor,
    } = setup(IcpTest::new().await).await;

    let user_balance_before = test.evm.get_balance(evm_user.address).await;

    let payment = parse_ether("0.1").unwrap();

    let receipt = coprocessor
        .newJob()
        .value(payment)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    assert!(receipt.status());

    let user_balance_after = test.evm.get_balance(evm_user.address).await;

    // This is not a strict equality because of gas cost payments.
    assert!(user_balance_before - payment >= user_balance_after);

    for _ in 0..100 {
        test.icp.tick().await;
    }

    let result = coprocessor.getResult(Uint::from(0)).call().await.unwrap();
    assert_eq!(result._0, "6765");
}
