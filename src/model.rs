use rust_decimal::Decimal;

pub type ClientId = u16;
pub type TxId = u32;

/// Max decimal places accepted for an amount.
pub const AMOUNT_SCALE: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transaction {
    Deposit {
        client: ClientId,
        tx: TxId,
        amount: Decimal,
    },
    Withdrawal {
        client: ClientId,
        tx: TxId,
        amount: Decimal,
    },
    Dispute {
        client: ClientId,
        tx: TxId,
    },
    Resolve {
        client: ClientId,
        tx: TxId,
    },
    Chargeback {
        client: ClientId,
        tx: TxId,
    },
}

impl Transaction {
    /// The client and tx ids, which every transaction type carries.
    pub fn ids(&self) -> (ClientId, TxId) {
        match *self {
            Self::Deposit { client, tx, .. }
            | Self::Withdrawal { client, tx, .. }
            | Self::Dispute { client, tx }
            | Self::Resolve { client, tx }
            | Self::Chargeback { client, tx } => (client, tx),
        }
    }
}
