use std::collections::{HashMap, HashSet};

use rust_decimal::Decimal;

use crate::account::{Account, AccountError};
use crate::model::{ClientId, Transaction, TxId};

/// Dispute flow of a stored deposit:
/// Normal -> Disputed -> Resolved (money back to available)
///                    \-> Chargeback (money gone, account locked)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    Normal,
    Disputed,
    Resolved,
    Chargeback,
}

/// Only deposits are stored: they are the only transactions that can be disputed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredDeposit {
    client: ClientId,
    amount: Decimal,
    state: TxState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// Deposit or withdrawal reuses the tx id of an earlier deposit or withdrawal.
    DuplicateTx,
    /// Dispute/resolve/chargeback references a deposit that does not exist.
    UnknownTx,
    /// The referenced deposit belongs to another client.
    ClientMismatch,
    /// The referenced deposit is not in the state this step requires.
    InvalidState,
    AccountLocked,
    InsufficientFunds,
    /// A balance would not fit in a `Decimal`.
    Overflow,
}

impl EngineError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateTx => "duplicate_tx",
            Self::UnknownTx => "unknown_tx",
            Self::ClientMismatch => "client_mismatch",
            Self::InvalidState => "invalid_state",
            Self::AccountLocked => "account_locked",
            Self::InsufficientFunds => "insufficient_funds",
            Self::Overflow => "overflow",
        }
    }
}

impl From<AccountError> for EngineError {
    fn from(e: AccountError) -> Self {
        match e {
            AccountError::Locked => Self::AccountLocked,
            AccountError::InsufficientFunds => Self::InsufficientFunds,
            AccountError::Overflow => Self::Overflow,
        }
    }
}

/// Applies transactions to accounts. Money math is delegated to `Account`.
#[derive(Debug, Default)]
pub struct Engine {
    accounts: HashMap<ClientId, Account>,
    deposits: HashMap<TxId, StoredDeposit>,
    /// Ids of applied withdrawals, only to reject reuse. Withdrawals can't be disputed.
    withdrawals: HashSet<TxId>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accounts(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }

    /// Applies one transaction. On error nothing changes, except that the client's
    /// (empty) account is opened: the spec says to create a record for a client that
    /// doesn't exist yet, whether or not the transaction is then rejected.
    pub fn apply(&mut self, transaction: Transaction) -> Result<(), EngineError> {
        let (client, _) = transaction.ids();
        get_or_open(&mut self.accounts, client);
        match transaction {
            Transaction::Deposit { client, tx, amount } => self.deposit(client, tx, amount),
            Transaction::Withdrawal { client, tx, amount } => self.withdraw(client, tx, amount),
            Transaction::Dispute { client, tx } => self.dispute(client, tx),
            Transaction::Resolve { client, tx } => self.resolve(client, tx),
            Transaction::Chargeback { client, tx } => self.chargeback(client, tx),
        }
    }

    fn deposit(&mut self, client: ClientId, tx: TxId, amount: Decimal) -> Result<(), EngineError> {
        self.ensure_new_tx(tx)?;
        get_or_open(&mut self.accounts, client).deposit(amount)?;
        self.deposits.insert(
            tx,
            StoredDeposit {
                client,
                amount,
                state: TxState::Normal,
            },
        );
        Ok(())
    }

    fn withdraw(&mut self, client: ClientId, tx: TxId, amount: Decimal) -> Result<(), EngineError> {
        self.ensure_new_tx(tx)?;
        get_or_open(&mut self.accounts, client).withdraw(amount)?;
        self.withdrawals.insert(tx);
        Ok(())
    }

    fn ensure_new_tx(&self, tx: TxId) -> Result<(), EngineError> {
        if self.deposits.contains_key(&tx) || self.withdrawals.contains(&tx) {
            return Err(EngineError::DuplicateTx);
        }
        Ok(())
    }

    /// Holds the full deposit amount, even if part of it was already withdrawn.
    /// `available` can then go negative: the client owes that money until the
    /// dispute is resolved or charged back.
    fn dispute(&mut self, client: ClientId, tx: TxId) -> Result<(), EngineError> {
        let deposit = find_deposit(&mut self.deposits, client, tx, TxState::Normal)?;
        get_or_open(&mut self.accounts, client).hold(deposit.amount)?;
        deposit.state = TxState::Disputed;
        Ok(())
    }

    fn resolve(&mut self, client: ClientId, tx: TxId) -> Result<(), EngineError> {
        let deposit = find_deposit(&mut self.deposits, client, tx, TxState::Disputed)?;
        get_or_open(&mut self.accounts, client).release(deposit.amount)?;
        deposit.state = TxState::Resolved;
        Ok(())
    }

    fn chargeback(&mut self, client: ClientId, tx: TxId) -> Result<(), EngineError> {
        let deposit = find_deposit(&mut self.deposits, client, tx, TxState::Disputed)?;
        get_or_open(&mut self.accounts, client).chargeback(deposit.amount)?;
        deposit.state = TxState::Chargeback;
        Ok(())
    }
}

// Free functions (not methods) so `deposits` and `accounts` can be borrowed at the same time.

/// Gets the client's account, opening an empty one on first use.
fn get_or_open(accounts: &mut HashMap<ClientId, Account>, client: ClientId) -> &mut Account {
    accounts
        .entry(client)
        .or_insert_with(|| Account::new(client))
}

/// The stored deposit `tx`, if it exists, belongs to `client` and is in state `expected`.
fn find_deposit(
    deposits: &mut HashMap<TxId, StoredDeposit>,
    client: ClientId,
    tx: TxId,
    expected: TxState,
) -> Result<&mut StoredDeposit, EngineError> {
    let deposit = deposits.get_mut(&tx).ok_or(EngineError::UnknownTx)?;
    if deposit.client != client {
        return Err(EngineError::ClientMismatch);
    }
    if deposit.state != expected {
        return Err(EngineError::InvalidState);
    }
    Ok(deposit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Transaction::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// (available, held, locked) for a client.
    fn balances(engine: &Engine, client: ClientId) -> (Decimal, Decimal, bool) {
        let a = &engine.accounts[&client];
        (a.available(), a.held(), a.locked())
    }

    fn engine_with_deposit() -> Engine {
        let mut e = Engine::new();
        e.apply(Deposit {
            client: 1,
            tx: 1,
            amount: d("2"),
        })
        .unwrap();
        e
    }

    #[test]
    fn deposit_and_withdrawal() {
        let mut e = engine_with_deposit();
        e.apply(Withdrawal {
            client: 1,
            tx: 2,
            amount: d("0.5"),
        })
        .unwrap();
        assert_eq!(balances(&e, 1), (d("1.5"), d("0"), false));
        assert_eq!(e.deposits[&1].state, TxState::Normal);
    }

    #[test]
    fn duplicate_deposit_is_rejected() {
        let mut e = engine_with_deposit();
        assert_eq!(
            e.apply(Deposit {
                client: 1,
                tx: 1,
                amount: d("5")
            }),
            Err(EngineError::DuplicateTx)
        );
        assert_eq!(balances(&e, 1), (d("2"), d("0"), false));
    }

    #[test]
    fn dispute_then_resolve() {
        let mut e = engine_with_deposit();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        assert_eq!(balances(&e, 1), (d("0"), d("2"), false));
        e.apply(Resolve { client: 1, tx: 1 }).unwrap();
        assert_eq!(balances(&e, 1), (d("2"), d("0"), false));
        assert_eq!(e.deposits[&1].state, TxState::Resolved);
    }

    #[test]
    fn dispute_then_chargeback_locks() {
        let mut e = engine_with_deposit();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        e.apply(Chargeback { client: 1, tx: 1 }).unwrap();
        assert_eq!(balances(&e, 1), (d("0"), d("0"), true));
        assert_eq!(e.deposits[&1].state, TxState::Chargeback);
        assert_eq!(
            e.apply(Deposit {
                client: 1,
                tx: 9,
                amount: d("1")
            }),
            Err(EngineError::AccountLocked)
        );
        assert!(!e.deposits.contains_key(&9));
    }

    #[test]
    fn dispute_rules() {
        let mut e = engine_with_deposit();
        assert_eq!(
            e.apply(Dispute { client: 1, tx: 99 }),
            Err(EngineError::UnknownTx)
        );
        assert_eq!(
            e.apply(Dispute { client: 2, tx: 1 }),
            Err(EngineError::ClientMismatch)
        );
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        assert_eq!(
            e.apply(Dispute { client: 1, tx: 1 }),
            Err(EngineError::InvalidState)
        );
    }

    #[test]
    fn resolve_and_chargeback_require_disputed() {
        let mut e = engine_with_deposit();
        assert_eq!(
            e.apply(Resolve { client: 1, tx: 1 }),
            Err(EngineError::InvalidState)
        );
        assert_eq!(
            e.apply(Chargeback { client: 1, tx: 1 }),
            Err(EngineError::InvalidState)
        );
        assert_eq!(
            e.apply(Resolve { client: 1, tx: 99 }),
            Err(EngineError::UnknownTx)
        );
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        assert_eq!(
            e.apply(Chargeback { client: 2, tx: 1 }),
            Err(EngineError::ClientMismatch)
        );
        assert_eq!(balances(&e, 1), (d("0"), d("2"), false));
    }

    // One test per README assumption (numbers match the README list).

    fn locked_engine() -> Engine {
        let mut e = Engine::new();
        e.apply(Deposit {
            client: 1,
            tx: 1,
            amount: d("5"),
        })
        .unwrap();
        e.apply(Deposit {
            client: 1,
            tx: 2,
            amount: d("3"),
        })
        .unwrap();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        e.apply(Chargeback { client: 1, tx: 1 }).unwrap();
        e
    }

    #[test]
    fn locked_account_blocks_deposit_and_withdrawal_but_not_disputes() {
        let mut e = locked_engine();
        assert_eq!(
            e.apply(Deposit {
                client: 1,
                tx: 3,
                amount: d("1")
            }),
            Err(EngineError::AccountLocked)
        );
        assert_eq!(
            e.apply(Withdrawal {
                client: 1,
                tx: 4,
                amount: d("1")
            }),
            Err(EngineError::AccountLocked)
        );
        e.apply(Dispute { client: 1, tx: 2 }).unwrap();
        e.apply(Chargeback { client: 1, tx: 2 }).unwrap();
        assert_eq!(balances(&e, 1), (d("0"), d("0"), true));
    }

    #[test]
    fn disputing_a_withdrawal_is_rejected() {
        let mut e = engine_with_deposit();
        e.apply(Withdrawal {
            client: 1,
            tx: 2,
            amount: d("1"),
        })
        .unwrap();
        assert_eq!(
            e.apply(Dispute { client: 1, tx: 2 }),
            Err(EngineError::UnknownTx)
        );
        assert_eq!(balances(&e, 1), (d("1"), d("0"), false));
    }

    #[test]
    fn client_mismatch_is_rejected() {
        let mut e = engine_with_deposit();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        assert_eq!(
            e.apply(Resolve { client: 2, tx: 1 }),
            Err(EngineError::ClientMismatch)
        );
        assert_eq!(
            e.apply(Chargeback { client: 2, tx: 1 }),
            Err(EngineError::ClientMismatch)
        );
        assert_eq!(balances(&e, 1), (d("0"), d("2"), false));
    }

    #[test]
    fn dispute_after_withdrawal_takes_available_negative() {
        let mut e = engine_with_deposit();
        e.apply(Withdrawal {
            client: 1,
            tx: 2,
            amount: d("2"),
        })
        .unwrap();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        assert_eq!(balances(&e, 1), (d("-2"), d("2"), false));
    }

    #[test]
    fn tx_can_only_be_disputed_once() {
        let mut e = engine_with_deposit();
        e.apply(Dispute { client: 1, tx: 1 }).unwrap();
        e.apply(Resolve { client: 1, tx: 1 }).unwrap();
        assert_eq!(
            e.apply(Dispute { client: 1, tx: 1 }),
            Err(EngineError::InvalidState)
        );

        let mut e = locked_engine();
        assert_eq!(
            e.apply(Dispute { client: 1, tx: 1 }),
            Err(EngineError::InvalidState)
        );
    }

    #[test]
    fn reused_tx_id_is_rejected() {
        let mut e = engine_with_deposit();
        assert_eq!(
            e.apply(Withdrawal {
                client: 1,
                tx: 1,
                amount: d("1")
            }),
            Err(EngineError::DuplicateTx)
        );
        e.apply(Withdrawal {
            client: 1,
            tx: 2,
            amount: d("1"),
        })
        .unwrap();
        assert_eq!(
            e.apply(Withdrawal {
                client: 1,
                tx: 2,
                amount: d("1")
            }),
            Err(EngineError::DuplicateTx)
        );
        assert_eq!(
            e.apply(Deposit {
                client: 1,
                tx: 2,
                amount: d("1")
            }),
            Err(EngineError::DuplicateTx)
        );
        assert_eq!(balances(&e, 1), (d("1"), d("0"), false));
    }

    #[test]
    fn every_parsed_transaction_opens_the_account_even_when_rejected() {
        let mut e = Engine::new();
        let insufficient = Withdrawal {
            client: 7,
            tx: 1,
            amount: d("1"),
        };
        assert_eq!(e.apply(insufficient), Err(EngineError::InsufficientFunds));
        e.apply(Deposit {
            client: 8,
            tx: 2,
            amount: d("1"),
        })
        .unwrap();
        let duplicate = Deposit {
            client: 9,
            tx: 2,
            amount: d("1"),
        };
        assert_eq!(e.apply(duplicate), Err(EngineError::DuplicateTx));
        assert_eq!(
            e.apply(Dispute { client: 10, tx: 99 }),
            Err(EngineError::UnknownTx)
        );
        assert_eq!(
            e.apply(Dispute { client: 11, tx: 2 }),
            Err(EngineError::ClientMismatch)
        );
        assert_eq!(
            e.apply(Resolve { client: 12, tx: 2 }),
            Err(EngineError::ClientMismatch)
        );
        assert_eq!(
            e.apply(Chargeback { client: 13, tx: 99 }),
            Err(EngineError::UnknownTx)
        );

        let mut opened: Vec<_> = e.accounts.keys().copied().collect();
        opened.sort();
        assert_eq!(opened, [7, 8, 9, 10, 11, 12, 13]);
        for client in [7, 9, 10, 11, 12, 13] {
            assert_eq!(balances(&e, client), (d("0"), d("0"), false));
        }
        assert_eq!(balances(&e, 8), (d("1"), d("0"), false));
        // A failed withdrawal does not use up its tx id.
        e.apply(Deposit {
            client: 7,
            tx: 1,
            amount: d("1"),
        })
        .unwrap();
    }

    #[test]
    fn withdrawal_beyond_available_is_rejected() {
        let mut e = Engine::new();
        e.apply(Deposit {
            client: 1,
            tx: 1,
            amount: d("1"),
        })
        .unwrap();
        assert_eq!(
            e.apply(Withdrawal {
                client: 1,
                tx: 2,
                amount: d("5")
            }),
            Err(EngineError::InsufficientFunds)
        );
        assert_eq!(balances(&e, 1), (d("1"), d("0"), false));
    }
}
