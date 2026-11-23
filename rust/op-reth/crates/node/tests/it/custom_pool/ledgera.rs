use super::{
    build_components_with_pool, funded_signers, node_test_setup_with_genesis, signed_transfer_raw,
};
use alloy_consensus::{Block, BlockBody, Header, SignableTransaction, TxEip1559};
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Signature, U256, uint};
use alloy_signer_local::PrivateKeySigner;
use op_alloy_network::Optimism;
use op_revm::ledgera::{KYC_REGISTRY, account_slot, flags_slot, vasp_slot};
use reth_e2e_test_utils::{node::NodeTestContext, transaction::TransactionTestContext};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, rpc::BasicEngineValidatorBuilder};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_evm::{OpEvmConfig, OpRethReceiptBuilder};
use reth_optimism_node::{
    OpEngineApiBuilder, OpNode,
    node::{OpEngineValidatorBuilder, OpPoolBuilder},
    utils::optimism_payload_attributes,
};
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_provider::StateProvider;
use reth_revm::{database::StateProviderDatabase, test_utils::StateProviderTest};
use reth_rpc_api::EthApiServer;
use std::sync::Arc;

const KYC_ERROR: &str = "ledgera: sender not allowed";
const GAS: u64 = 200_000;
const TIP: u128 = 1_000_000_000;
const MAX_FEE: u128 = 3_000_000_000;

fn genesis() -> (Genesis, Vec<PrivateKeySigner>) {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("../../assets/genesis.json")).unwrap();
    let signers = funded_signers(4, reth_optimism_chainspec::OP_SEPOLIA.chain().into());

    for signer in &signers[1..3] {
        genesis.alloc.entry(signer.address()).or_default().balance = uint!(1000000000000000_U256);
    }
    genesis.alloc.entry(signers[3].address()).or_default().balance = U256::ONE;

    let registry = genesis.alloc.entry(KYC_REGISTRY).or_default().storage.get_or_insert_default();
    registry.insert(B256::from(flags_slot()), B256::from(U256::ONE));
    registry.insert(B256::from(vasp_slot(1)), B256::from(U256::ONE));
    for signer in &signers[..2] {
        registry
            .insert(B256::from(account_slot(signer.address())), B256::from(uint!(0x10001_U256)));
    }
    (genesis, signers)
}

#[tokio::test]
async fn ledgera_standard_pool_checks_ready_transactions() {
    let (genesis, signers) = genesis();
    let (config, db, runtime, chain_id) = node_test_setup_with_genesis(genesis);
    let op = OpNode::default();
    let node_handle = NodeBuilder::new(config.clone())
        .with_database(db)
        .with_types::<OpNode>()
        .with_components(op.components())
        .with_add_ons(op.add_ons())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                runtime.clone(),
                builder.config.datadir(),
                Default::default(),
            );
            builder.launch_with(launcher)
        })
        .await
        .unwrap();

    let mut ctx =
        NodeTestContext::new(node_handle.node, optimism_payload_attributes).await.unwrap();
    let l1_info =
        TransactionTestContext::optimism_l1_block_info_tx(chain_id, signers[0].clone(), 0).await;
    ctx.rpc.inject_tx(l1_info).await.unwrap();

    let (allowed_raw, allowed_hash) =
        signed_transfer_raw(&signers[1], chain_id, 0, GAS, TIP, MAX_FEE);
    let (denied_raw, denied_hash) =
        signed_transfer_raw(&signers[2], chain_id, 0, GAS, TIP, MAX_FEE);
    let (underfunded_raw, _) = signed_transfer_raw(&signers[3], chain_id, 0, GAS, TIP, MAX_FEE);
    assert_eq!(ctx.rpc.inject_tx(allowed_raw).await.unwrap(), allowed_hash);
    let denied = ctx.rpc.inject_tx(denied_raw).await.unwrap_err().to_string();
    assert!(denied.contains(KYC_ERROR), "{denied}");
    let underfunded = ctx.rpc.inject_tx(underfunded_raw).await.unwrap_err().to_string();
    assert!(underfunded.contains("insufficient funds"), "{underfunded}");
    assert!(!underfunded.contains(KYC_ERROR), "{underfunded}");

    let eth = ctx.rpc.inner.eth_api();
    let call = alloy_rpc_types_eth::TransactionRequest::default().to(signers[2].address());
    assert!(eth.call(call.clone().into(), None, None, None).await.is_ok());
    let estimate = eth
        .estimate_gas(call.from(signers[2].address()).into(), None, None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(estimate.contains(KYC_ERROR), "{estimate}");

    let payload = ctx.new_payload().await.unwrap();
    let hashes: Vec<_> =
        payload.block().body().transactions.iter().map(|tx| *tx.tx_hash()).collect();
    assert!(hashes.contains(&allowed_hash));
    assert!(!hashes.contains(&denied_hash));
}

#[tokio::test]
async fn ledgera_execution_skips_a_transaction_admitted_by_the_identity_pool() {
    let (genesis, signers) = genesis();
    let (config, db, runtime, chain_id) = node_test_setup_with_genesis(genesis);

    let node_handle = NodeBuilder::new(config.clone())
        .with_database(db)
        .with_types::<OpNode>()
        .with_components(build_components_with_pool(OpPoolBuilder::default()))
        .with_add_ons(
            OpNode::default().add_ons_builder::<Optimism>().build::<
                _,
                OpEngineValidatorBuilder,
                OpEngineApiBuilder<OpEngineValidatorBuilder>,
                BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
            >(),
        )
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(runtime.clone(), builder.config.datadir(), Default::default());
            builder.launch_with(launcher)
        })
        .await
        .unwrap();

    let mut ctx =
        NodeTestContext::new(node_handle.node, optimism_payload_attributes).await.unwrap();
    let l1_info =
        TransactionTestContext::optimism_l1_block_info_tx(chain_id, signers[0].clone(), 0).await;
    ctx.rpc.inject_tx(l1_info).await.unwrap();
    let (allowed_raw, allowed_hash) =
        signed_transfer_raw(&signers[1], chain_id, 0, GAS, TIP, MAX_FEE);
    let (denied_raw, denied_hash) =
        signed_transfer_raw(&signers[2], chain_id, 0, GAS, TIP, MAX_FEE);
    ctx.rpc.inject_tx(allowed_raw).await.unwrap();
    assert_eq!(ctx.rpc.inject_tx(denied_raw).await.unwrap(), denied_hash);

    let payload = ctx.new_payload().await.unwrap();
    let hashes: Vec<_> =
        payload.block().body().transactions.iter().map(|tx| *tx.tx_hash()).collect();
    assert!(hashes.contains(&allowed_hash));
    assert!(!hashes.contains(&denied_hash));
}

#[test]
fn ledgera_block_executor_rejects_an_explicit_unregistered_transaction() {
    let sender = Address::repeat_byte(0x34);
    let chain_spec = Arc::new(OpChainSpecBuilder::optimism_mainnet().regolith_activated().build());
    let tx: OpTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        gas_limit: 21_000,
        max_fee_per_gas: MAX_FEE,
        max_priority_fee_per_gas: TIP,
        to: Address::repeat_byte(0x35).into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();
    let block = RecoveredBlock::new_unhashed(
        Block {
            header: Header { timestamp: 1, number: 1, gas_limit: 1_000_000, ..Default::default() },
            body: BlockBody { transactions: vec![tx], ..Default::default() },
        },
        vec![sender],
    );
    let config: OpEvmConfig = OpEvmConfig::new(chain_spec, OpRethReceiptBuilder::default());
    let execute = |flags| {
        let mut provider = StateProviderTest::default();
        provider.insert_account(
            KYC_REGISTRY,
            Account { nonce: 1, ..Default::default() },
            None,
            [(B256::from(flags_slot()), flags)].into_iter().collect(),
        );
        provider.insert_account(
            sender,
            Account { balance: uint!(1000000000000000_U256), ..Default::default() },
            None,
            Default::default(),
        );
        BasicBlockExecutor::new(
            config.clone(),
            StateProviderDatabase::new((&provider).into_evm_state_provider()),
        )
        .execute(&block)
    };

    execute(U256::ZERO).unwrap();
    let error = execute(U256::ONE).unwrap_err().to_string();
    assert!(error.contains(KYC_ERROR), "{error}");
}
