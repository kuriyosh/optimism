//! Applies ledgerA sender admission at the canonical head.

use crate::{OpValidatorWrapper, txpool::OpTransactionValidator};
use alloy_consensus::BlockHeader as _;
use alloy_primitives::Address;
use op_revm::ledgera::sender_allowed;
use reth_primitives_traits::SealedBlock;
use reth_provider::{BlockReaderIdExt, StateProvider, StateProviderFactory};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use std::{any::Any, error::Error, fmt, io};

/// Adds ledgerA admission after the standard OP validation.
#[derive(Clone, Copy, Debug, Default)]
pub struct KycValidatorWrapper;

impl<Provider, T, Evm> OpValidatorWrapper<Provider, T, Evm> for KycValidatorWrapper
where
    Provider: BlockReaderIdExt + StateProviderFactory,
    OpTransactionValidator<Provider, T, Evm>: TransactionValidator,
{
    type Validator = KycValidator<OpTransactionValidator<Provider, T, Evm>, Provider>;

    fn wrap(
        self,
        inner: OpTransactionValidator<Provider, T, Evm>,
        provider: Provider,
    ) -> Self::Validator {
        KycValidator { inner, provider }
    }
}

/// Validates the sender against the Registry at the latest canonical block.
pub struct KycValidator<V, Provider> {
    inner: V,
    provider: Provider,
}

impl<V, Provider> KycValidator<V, Provider> {
    /// Returns the underlying OP validator.
    pub const fn inner(&self) -> &V {
        &self.inner
    }
}

impl<V: fmt::Debug, Provider> fmt::Debug for KycValidator<V, Provider> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("KycValidator").field("inner", &self.inner).finish_non_exhaustive()
    }
}

impl<V, Provider> KycValidator<V, Provider>
where
    Provider: BlockReaderIdExt + StateProviderFactory,
{
    fn sender_allowed(
        &self,
        tx: &impl PoolTransaction,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let header = self
            .provider
            .latest_header()?
            .ok_or_else(|| io::Error::other("canonical head is unavailable"))?;
        let state = self.provider.state_by_block_hash(header.hash())?;
        let mut db = StateProviderDatabase::new((&state).into_evm_state_provider());
        Ok(sender_allowed(&mut db, tx.sender(), tx.ty(), header.header().timestamp())?)
    }
}

impl<V, Provider> TransactionValidator for KycValidator<V, Provider>
where
    V: TransactionValidator,
    Provider: BlockReaderIdExt + StateProviderFactory,
{
    type Transaction = V::Transaction;
    type Block = V::Block;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        let outcome = self.inner.validate_transaction(origin, transaction).await;
        let Some(transaction) = outcome.as_valid_transaction() else {
            return outcome;
        };

        match (self.sender_allowed(transaction.transaction()), outcome) {
            (Err(error), outcome) => TransactionValidationOutcome::Error(outcome.tx_hash(), error),
            (Ok(false), TransactionValidationOutcome::Valid { transaction, .. }) => {
                let transaction = transaction.into_transaction();
                let error = SenderNotAllowed(transaction.sender());
                TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::Other(Box::new(error)),
                )
            }
            (_, outcome) => outcome,
        }
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
    }
}

#[derive(Debug)]
struct SenderNotAllowed(Address);

impl fmt::Display for SenderNotAllowed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ledgera: sender not allowed: {}", self.0)
    }
}

impl Error for SenderNotAllowed {}

impl PoolTransactionError for SenderNotAllowed {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
