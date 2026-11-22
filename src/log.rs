use std::fmt;

use crate::model::{ClientId, TxId};

/// The one format for a logged error: the reason plus whichever ids are known.
/// Never carries amounts, balances or raw row content (README assumption 10).
pub struct LogLine<'a> {
    pub reason: &'a str,
    pub line: Option<u64>,
    pub client: Option<ClientId>,
    pub tx: Option<TxId>,
}

impl fmt::Display for LogLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error={}", self.reason)?;
        if let Some(line) = self.line {
            write!(f, " line={line}")?;
        }
        if let Some(client) = self.client {
            write!(f, " client={client}")?;
        }
        if let Some(tx) = self.tx {
            write!(f, " tx={tx}")?;
        }
        Ok(())
    }
}
