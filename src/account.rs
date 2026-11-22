use rust_decimal::Decimal;

use crate::model::ClientId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    Locked,
    InsufficientFunds,
    /// The result would not fit in a `Decimal`.
    Overflow,
}

/// A client's balances. All money math lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    client: ClientId,
    available: Decimal,
    held: Decimal,
    locked: bool,
}

impl Account {
    pub fn new(client: ClientId) -> Self {
        Self {
            client,
            available: Decimal::ZERO,
            held: Decimal::ZERO,
            locked: false,
        }
    }

    pub fn client(&self) -> ClientId {
        self.client
    }

    pub fn available(&self) -> Decimal {
        self.available
    }

    pub fn held(&self) -> Decimal {
        self.held
    }

    /// Can't overflow: `deposit` is the only operation that raises the total, and it
    /// rejects any deposit that would take `available + held` past `Decimal::MAX`.
    pub fn total(&self) -> Decimal {
        self.available + self.held
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Blocked on a locked account, and when the new total would not fit in a `Decimal`.
    pub fn deposit(&mut self, amount: Decimal) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        let available = add(self.available, amount)?;
        add(available, self.held)?; // the total must fit too, or `total()` would overflow
        self.available = available;
        Ok(())
    }

    /// Blocked on a locked account, and when it would take `available` below zero.
    pub fn withdraw(&mut self, amount: Decimal) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        if amount > self.available {
            return Err(AccountError::InsufficientFunds);
        }
        self.available -= amount;
        Ok(())
    }

    /// Dispute: move the amount from available to held. Allowed on a locked account,
    /// and `available` may go negative if the money was already withdrawn.
    pub fn hold(&mut self, amount: Decimal) -> Result<(), AccountError> {
        let available = sub(self.available, amount)?;
        let held = add(self.held, amount)?;
        (self.available, self.held) = (available, held);
        Ok(())
    }

    /// Resolve: move the amount from held back to available.
    pub fn release(&mut self, amount: Decimal) -> Result<(), AccountError> {
        let held = sub(self.held, amount)?;
        let available = add(self.available, amount)?;
        (self.available, self.held) = (available, held);
        Ok(())
    }

    /// Chargeback: remove the held amount and lock the account.
    pub fn chargeback(&mut self, amount: Decimal) -> Result<(), AccountError> {
        self.held = sub(self.held, amount)?;
        self.locked = true;
        Ok(())
    }

    fn ensure_unlocked(&self) -> Result<(), AccountError> {
        if self.locked {
            Err(AccountError::Locked)
        } else {
            Ok(())
        }
    }
}

// Checked math: an overflow is reported instead of panicking, and the balance is untouched.
fn add(a: Decimal, b: Decimal) -> Result<Decimal, AccountError> {
    a.checked_add(b).ok_or(AccountError::Overflow)
}

fn sub(a: Decimal, b: Decimal) -> Result<Decimal, AccountError> {
    a.checked_sub(b).ok_or(AccountError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn account_with(available: &str) -> Account {
        let mut a = Account::new(1);
        a.deposit(d(available)).unwrap();
        a
    }

    fn locked_account() -> Account {
        let mut a = account_with("2");
        a.hold(d("1")).unwrap();
        a.chargeback(d("1")).unwrap();
        a
    }

    #[test]
    fn deposit_adds_to_available() {
        let a = account_with("1.5");
        assert_eq!((a.available, a.held, a.locked), (d("1.5"), d("0"), false));
    }

    #[test]
    fn locked_account_blocks_deposit_and_withdraw() {
        let mut a = locked_account();
        assert_eq!(a.deposit(d("1")), Err(AccountError::Locked));
        assert_eq!(a.withdraw(d("0.5")), Err(AccountError::Locked));
        assert_eq!(a.available, d("1"));
    }

    #[test]
    fn locked_account_still_allows_dispute_flow() {
        let mut a = locked_account();
        a.hold(d("1")).unwrap();
        a.release(d("1")).unwrap();
        assert_eq!((a.available, a.held), (d("1"), d("0")));
    }

    #[test]
    fn withdraw_subtracts_from_available() {
        let mut a = account_with("2");
        a.withdraw(d("0.5")).unwrap();
        assert_eq!(a.available, d("1.5"));
    }

    #[test]
    fn withdraw_cannot_go_negative() {
        let mut a = account_with("1");
        assert_eq!(a.withdraw(d("5")), Err(AccountError::InsufficientFunds));
        assert_eq!(a.available, d("1"));
        a.withdraw(d("1")).unwrap();
        assert_eq!(a.available, d("0"));
    }

    #[test]
    fn hold_moves_available_to_held_even_below_zero() {
        let mut a = account_with("2");
        a.withdraw(d("2")).unwrap();
        a.hold(d("2")).unwrap();
        assert_eq!((a.available, a.held, a.total()), (d("-2"), d("2"), d("0")));
    }

    #[test]
    fn release_moves_held_to_available() {
        let mut a = account_with("2");
        a.hold(d("0.5")).unwrap();
        a.release(d("0.5")).unwrap();
        assert_eq!((a.available, a.held), (d("2"), d("0")));
    }

    #[test]
    fn chargeback_removes_held_and_locks() {
        let mut a = account_with("2");
        a.hold(d("0.5")).unwrap();
        a.chargeback(d("0.5")).unwrap();
        assert_eq!((a.available, a.held, a.locked), (d("1.5"), d("0"), true));
        assert_eq!(a.total(), d("1.5"));
    }

    #[test]
    fn overflow_is_an_error_not_a_panic() {
        let mut a = account_with(&Decimal::MAX.to_string());
        assert_eq!(a.deposit(d("1")), Err(AccountError::Overflow));
        assert_eq!(a.available, Decimal::MAX);
    }

    #[test]
    fn deposit_rejected_when_total_would_overflow() {
        // held=MAX after a dispute, so a second MAX deposit fits in available but not in total.
        let mut a = account_with(&Decimal::MAX.to_string());
        a.hold(Decimal::MAX).unwrap();
        assert_eq!(a.deposit(Decimal::MAX), Err(AccountError::Overflow));
        assert_eq!(
            (a.available, a.held, a.total()),
            (d("0"), Decimal::MAX, Decimal::MAX)
        );
    }
}
