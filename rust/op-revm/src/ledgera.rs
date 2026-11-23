//! Reads the ledgerA Registry layout shared with the Solidity contract.

use op_alloy_consensus::{OpTxType, POST_EXEC_TX_TYPE_ID};
use revm::{
    Database,
    primitives::{Address, B256, U256, address, keccak256, uint},
};

/// Fixed Registry predeploy address.
pub const KYC_REGISTRY: Address = address!("0x4200000000000000000000000000000000000700");
/// L1 attributes deposits must run before user transaction execution.
pub const L1_ATTRIBUTES_DEPOSITOR: Address = address!("0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001");

const BASE_SLOT: U256 =
    uint!(0x8fe41602647df745ca7420aedc6edbf55eb356a7ee29e0fe07118b9b158f2e00_U256);
const STATUS_ACTIVE: u8 = 1;

fn mapping_slot(key: B256, slot: U256) -> U256 {
    let mut words = [0_u8; 64];
    words[..32].copy_from_slice(key.as_slice());
    words[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(words).0)
}

/// Returns the Solidity mapping slot for the sender's packed account word.
pub fn account_slot(account: Address) -> U256 {
    mapping_slot(B256::left_padding_from(account.as_slice()), BASE_SLOT)
}

/// Returns the Solidity mapping slot for the VASP's packed status word.
pub fn vasp_slot(id: u32) -> U256 {
    mapping_slot(B256::from(U256::from(id)), BASE_SLOT + U256::ONE)
}

/// The enforcement flag occupies bit 0 of this word.
pub fn flags_slot() -> U256 {
    BASE_SLOT + U256::from(2)
}

/// Reads committed state directly so admission does not warm EVM storage slots.
pub fn sender_allowed<DB: Database>(
    db: &mut DB,
    sender: Address,
    tx_type: u8,
    timestamp: u64,
) -> Result<bool, DB::Error> {
    if sender == Address::ZERO
        || sender == L1_ATTRIBUTES_DEPOSITOR
        || tx_type == POST_EXEC_TX_TYPE_ID
    {
        return Ok(true);
    }
    let _ = db.basic(KYC_REGISTRY)?;
    if db.storage(KYC_REGISTRY, flags_slot())? & U256::ONE == U256::ZERO {
        return Ok(true);
    }
    if tx_type == OpTxType::Eip7702 as u8 {
        return Ok(false);
    }
    let account = db.storage(KYC_REGISTRY, account_slot(sender))?;
    if account.as_limbs()[0] as u8 != STATUS_ACTIVE {
        return Ok(false);
    }
    let valid_until = (account >> 48usize).as_limbs()[0];
    if valid_until != 0 && timestamp > valid_until {
        return Ok(false);
    }
    let id = (account >> 16usize).as_limbs()[0] as u32;
    let vasp = db.storage(KYC_REGISTRY, vasp_slot(id))?;
    Ok(vasp.as_limbs()[0] as u8 == STATUS_ACTIVE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DefaultOp, OpBuilder, OpHaltReason, OpSpecId, OpTransaction,
        transaction::OpTransactionError,
    };
    use revm::{
        ExecuteCommitEvm,
        bytecode::Bytecode,
        context::{CfgEnv, Context, TxEnv},
        context_interface::{
            ContextTr, JournalTr,
            result::{EVMError, ExecutionResult},
        },
        database::InMemoryDB,
        handler::EvmTr,
        primitives::{Bytes, bytes},
        state::AccountInfo,
    };

    const CUSTOMER: Address = address!("0x1111111111111111111111111111111111111111");

    #[test]
    fn ledger_registry_layout_matches_solidity() {
        assert_eq!(
            account_slot(CUSTOMER),
            uint!(0x0af8647d1a0890445823b07eb9e2efa4337166e6e4bd9cee280b017e0ec9696e_U256)
        );
        assert_eq!(
            vasp_slot(1),
            uint!(0x5ef4ef46b66bf28de20f56d7769e17bd4003ba8f75d884139649b0c9bc4ca857_U256)
        );
        assert_eq!(
            flags_slot(),
            uint!(0x8fe41602647df745ca7420aedc6edbf55eb356a7ee29e0fe07118b9b158f2e02_U256)
        );
    }

    #[test]
    fn ledger_registry_applies_all_sender_conditions() {
        let mut db = InMemoryDB::default();
        assert!(sender_allowed(&mut db, CUSTOMER, OpTxType::Eip7702 as u8, 11).unwrap());

        db.insert_account_storage(KYC_REGISTRY, flags_slot(), U256::ONE).unwrap();
        assert!(sender_allowed(&mut db, Address::ZERO, 0, 11).unwrap());
        assert!(
            sender_allowed(&mut db, L1_ATTRIBUTES_DEPOSITOR, OpTxType::Deposit as u8, 11).unwrap()
        );
        assert!(sender_allowed(&mut db, CUSTOMER, POST_EXEC_TX_TYPE_ID, 11).unwrap());
        assert!(!sender_allowed(&mut db, CUSTOMER, 0, 11).unwrap());

        db.insert_account_storage(KYC_REGISTRY, vasp_slot(1), U256::ONE).unwrap();
        for status in [2, 3, 4] {
            let word = U256::from(status) | (U256::ONE << 16usize);
            db.insert_account_storage(KYC_REGISTRY, account_slot(CUSTOMER), word).unwrap();
            assert!(!sender_allowed(&mut db, CUSTOMER, 0, 11).unwrap());
        }

        let active = U256::ONE | (U256::ONE << 16usize) | (U256::from(20) << 48usize);
        db.insert_account_storage(KYC_REGISTRY, account_slot(CUSTOMER), active).unwrap();
        assert!(sender_allowed(&mut db, CUSTOMER, 0, 20).unwrap());
        assert!(!sender_allowed(&mut db, CUSTOMER, 0, 21).unwrap());
        assert!(!sender_allowed(&mut db, CUSTOMER, OpTxType::Eip7702 as u8, 20).unwrap());

        db.insert_account_storage(KYC_REGISTRY, vasp_slot(1), U256::from(2)).unwrap();
        assert!(!sender_allowed(&mut db, CUSTOMER, 0, 20).unwrap());
    }

    const UNREGISTERED: Address = Address::repeat_byte(0x22);
    const COUNTER: Address = Address::repeat_byte(0x33);
    const BALANCE: u64 = 1_000_000;
    const MINT: u128 = 1000;
    const ACTIVE_ACCOUNT: U256 = uint!(0x10001_U256);

    fn execution_db() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        for address in [CUSTOMER, UNREGISTERED] {
            db.insert_account_info(address, AccountInfo::from_balance(U256::from(BALANCE)));
        }
        db.insert_account_info(
            COUNTER,
            AccountInfo::from_bytecode(Bytecode::new_raw(bytes!("60005460010160005500"))),
        );
        db.insert_account_storage(KYC_REGISTRY, flags_slot(), U256::ONE).unwrap();
        db.insert_account_storage(KYC_REGISTRY, vasp_slot(1), U256::ONE).unwrap();
        db
    }

    fn transfer(sender: Address) -> OpTransaction<TxEnv> {
        OpTransaction::builder()
            .base(TxEnv::builder().caller(sender).gas_limit(100_000).gas_price(1).to(COUNTER))
            .enveloped_tx(Some(bytes!("c0")))
            .build_fill()
    }

    fn deposit(sender: Address, recipient: Address, source: u8) -> OpTransaction<TxEnv> {
        OpTransaction::builder()
            .base(TxEnv::builder().caller(sender).gas_limit(100_000).to(recipient))
            .source_hash(B256::with_last_byte(source))
            .mint(MINT)
            .build_fill()
    }

    #[test]
    fn ledger_registry_changes_take_effect_in_the_same_block() {
        let mut db = execution_db();
        let mut registry_code = vec![0x62, 0x01, 0x00, 0x01, 0x7f];
        registry_code.extend_from_slice(&account_slot(CUSTOMER).to_be_bytes::<32>());
        registry_code.extend_from_slice(&[0x55, 0x00]);
        db.insert_account_info(
            KYC_REGISTRY,
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from(registry_code))),
        );
        let mut evm = Context::op()
            .with_db(db)
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH))
            .build_op();

        assert!(matches!(
            evm.transact_commit(transfer(CUSTOMER)),
            Err(EVMError::Transaction(OpTransactionError::SenderNotAllowed))
        ));
        {
            let db = evm.ctx().journal_mut().db_mut();
            let caller = db.basic(CUSTOMER).unwrap().unwrap();
            assert_eq!(caller.balance, U256::from(BALANCE));
            assert_eq!(caller.nonce, 0);
            assert_eq!(db.storage(COUNTER, U256::ZERO).unwrap(), U256::ZERO);
        }
        assert!(
            evm.transact_commit(deposit(L1_ATTRIBUTES_DEPOSITOR, KYC_REGISTRY, 1))
                .unwrap()
                .is_success()
        );
        assert!(evm.transact_commit(transfer(CUSTOMER)).unwrap().is_success());
        let db = evm.ctx().journal_mut().db_mut();
        assert_eq!(db.storage(KYC_REGISTRY, account_slot(CUSTOMER)).unwrap(), ACTIVE_ACCOUNT);
        assert_eq!(db.storage(COUNTER, U256::ZERO).unwrap(), U256::ONE);
    }

    #[test]
    fn ledger_registry_fails_only_the_unregistered_deposit_and_keeps_its_mint() {
        let mut db = execution_db();
        db.insert_account_storage(KYC_REGISTRY, account_slot(CUSTOMER), ACTIVE_ACCOUNT).unwrap();
        let mut evm = Context::op()
            .with_db(db)
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH))
            .build_op();

        assert!(matches!(
            evm.transact_commit(deposit(UNREGISTERED, COUNTER, 1)),
            Ok(ExecutionResult::Halt { reason: OpHaltReason::FailedDeposit, .. })
        ));
        {
            let db = evm.ctx().journal_mut().db_mut();
            let caller = db.basic(UNREGISTERED).unwrap().unwrap();
            assert_eq!(caller.balance, U256::from(BALANCE) + U256::from(MINT));
            assert_eq!(caller.nonce, 1);
            assert_eq!(db.storage(COUNTER, U256::ZERO).unwrap(), U256::ZERO);
        }
        assert!(evm.transact_commit(deposit(CUSTOMER, COUNTER, 2)).unwrap().is_success());
        let db = evm.ctx().journal_mut().db_mut();
        assert_eq!(db.storage(COUNTER, U256::ZERO).unwrap(), U256::ONE);
    }
}
