use std::fmt;
use std::io::Read;
use std::str::FromStr;

use csv::{ReaderBuilder, StringRecord, Trim};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::log::LogLine;
use crate::model::{ClientId, Transaction, TxId, AMOUNT_SCALE};

/// One CSV row with the column types checked. Columns are read by position:
/// type, client, tx, amount. Turned into a `Transaction` by `to_transaction`.
#[derive(Deserialize)]
struct RawRecord {
    kind: String,
    client: ClientId,
    tx: TxId,
    #[serde(default)] // a missing trailing column reads as None
    amount: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The input could not be read. Fatal: stop processing.
    Io,
    /// The first row is not `type, client, tx, amount`. Fatal: without it the
    /// first transaction would be silently taken as the header.
    InvalidHeader,
    /// Row does not match `type, client, tx, amount` with the expected types
    /// (also: too many columns, invalid UTF-8).
    MalformedRow,
    UnknownType,
    MissingAmount,
    /// Not a plain decimal number such as `1.5` (`1e3`, `+1`, `1_000`, `abc` are not).
    InvalidAmount,
    NonPositiveAmount,
    AmountPrecision,
}

impl ErrorKind {
    pub fn is_fatal(self) -> bool {
        matches!(self, Self::Io | Self::InvalidHeader)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::InvalidHeader => "invalid_header",
            Self::MalformedRow => "malformed_row",
            Self::UnknownType => "unknown_type",
            Self::MissingAmount => "missing_amount",
            Self::InvalidAmount => "invalid_amount",
            Self::NonPositiveAmount => "non_positive_amount",
            Self::AmountPrecision => "amount_precision",
        }
    }
}

/// A rejected row. Carries identifiers and the reason only, never amounts or raw
/// row content, so it is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    pub line: Option<u64>,
    pub client: Option<ClientId>,
    pub tx: Option<TxId>,
    pub kind: ErrorKind,
}

impl RowError {
    /// An error for a row whose client/tx ids could not be read.
    fn new(line: Option<u64>, kind: ErrorKind) -> Self {
        Self {
            line,
            client: None,
            tx: None,
            kind,
        }
    }
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let log = LogLine {
            reason: self.kind.as_str(),
            line: self.line,
            client: self.client,
            tx: self.tx,
        };
        log.fmt(f)
    }
}

/// Streams transactions from CSV, one row at a time (the file is never loaded whole).
pub struct TxReader<R: Read> {
    csv: csv::Reader<R>,
    record: StringRecord,
    stopped: bool,
}

const HEADER: [&str; 4] = ["type", "client", "tx", "amount"];

/// Checks the header row, then returns a reader over the transactions.
/// An empty input is valid and has no transactions.
pub fn read<R: Read>(input: R) -> Result<TxReader<R>, RowError> {
    let mut csv = ReaderBuilder::new()
        .trim(Trim::All)
        .flexible(true) // dispute/resolve/chargeback rows may leave out the amount column
        .from_reader(input);
    let header_ok = match csv.headers() {
        Ok(h) => h.is_empty() || h.iter().eq(HEADER),
        Err(e) if e.is_io_error() => return Err(RowError::new(Some(1), ErrorKind::Io)),
        Err(_) => false,
    };
    if !header_ok {
        return Err(RowError::new(Some(1), ErrorKind::InvalidHeader));
    }
    Ok(TxReader {
        csv,
        record: StringRecord::new(),
        stopped: false,
    })
}

impl<R: Read> Iterator for TxReader<R> {
    type Item = Result<Transaction, RowError>;

    /// Yields one transaction or one rejected row per CSV line.
    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped {
            return None;
        }
        match self.csv.read_record(&mut self.record) {
            Ok(true) => Some(parse_record(&self.record)),
            Ok(false) => None,
            Err(e) => {
                let line = e.position().map(|p| p.line());
                if !e.is_io_error() {
                    // Invalid UTF-8: the row is skipped and reading goes on.
                    return Some(Err(RowError::new(line, ErrorKind::MalformedRow)));
                }
                // The input itself can't be read: report it once, then stop.
                self.stopped = true;
                Some(Err(RowError::new(line, ErrorKind::Io)))
            }
        }
    }
}

/// Parses one CSV record. Errors carry the line, plus client/tx once those are known.
fn parse_record(record: &StringRecord) -> Result<Transaction, RowError> {
    let line = record.position().map(|p| p.line());
    // A malformed row still reports whichever ids do parse, so the log line can name them.
    let malformed = || RowError {
        line,
        client: record.get(1).and_then(|s| s.parse().ok()),
        tx: record.get(2).and_then(|s| s.parse().ok()),
        kind: ErrorKind::MalformedRow,
    };
    if record.len() > HEADER.len() {
        return Err(malformed());
    }
    let raw: RawRecord = record.deserialize(None).map_err(|_| malformed())?;
    to_transaction(&raw).map_err(|kind| RowError {
        line,
        client: Some(raw.client),
        tx: Some(raw.tx),
        kind,
    })
}

fn to_transaction(raw: &RawRecord) -> Result<Transaction, ErrorKind> {
    let (client, tx) = (raw.client, raw.tx);
    Ok(match raw.kind.as_str() {
        "deposit" => Transaction::Deposit {
            client,
            tx,
            amount: required_amount(raw)?,
        },
        "withdrawal" => Transaction::Withdrawal {
            client,
            tx,
            amount: required_amount(raw)?,
        },
        "dispute" => Transaction::Dispute { client, tx },
        "resolve" => Transaction::Resolve { client, tx },
        "chargeback" => Transaction::Chargeback { client, tx },
        _ => return Err(ErrorKind::UnknownType),
    })
}

fn required_amount(raw: &RawRecord) -> Result<Decimal, ErrorKind> {
    let amount = raw.amount.as_deref().ok_or(ErrorKind::MissingAmount)?;
    parse_amount(amount)
}

fn parse_amount(s: &str) -> Result<Decimal, ErrorKind> {
    // Plain decimals only. `Decimal::from_str` alone would also accept "1e3" and "1_000".
    let unsigned = s.strip_prefix('-').unwrap_or(s);
    let plain = unsigned.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if unsigned.is_empty() || !plain || unsigned.matches('.').count() > 1 {
        return Err(ErrorKind::InvalidAmount);
    }
    // Count decimals on the text: `from_str` silently rounds past 28 decimals, which
    // would let "1.00000000000000000000000000001" through as 1. Trailing zeros don't count.
    let decimals = unsigned
        .split_once('.')
        .map_or(0, |(_, frac)| frac.trim_end_matches('0').len());
    if decimals > AMOUNT_SCALE as usize {
        return Err(ErrorKind::AmountPrecision);
    }
    let amount = Decimal::from_str(s).map_err(|_| ErrorKind::InvalidAmount)?;
    if amount <= Decimal::ZERO {
        return Err(ErrorKind::NonPositiveAmount);
    }
    // Drop trailing zeros ("1.50000" -> 1.5) so the output never shows more than 4 decimals.
    Ok(amount.normalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(csv: &str) -> Vec<Result<Transaction, RowError>> {
        read(csv.as_bytes()).unwrap().collect()
    }

    fn kinds(csv: &[u8]) -> Vec<Option<ErrorKind>> {
        read(csv)
            .unwrap()
            .map(|r| r.err().map(|e| e.kind))
            .collect()
    }

    #[test]
    fn reads_input_with_spaces() {
        let rows = parse(
            "type, client, tx, amount\n\
             deposit, 3, 1, 4.25\n\
             deposit, 7, 2, 1.5\n\
             deposit, 3, 3, 0.75\n\
             withdrawal, 3, 4, 2.0\n\
             withdrawal, 7, 5, 2.5\n",
        );
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[3],
            Ok(Transaction::Withdrawal {
                client: 3,
                tx: 4,
                amount: Decimal::new(2, 0)
            })
        );
    }

    #[test]
    fn reads_amountless_types_with_or_without_amount_column() {
        let rows = parse("type,client,tx,amount\ndispute,1,1\nresolve,1,1,\nchargeback,2,2\n");
        assert_eq!(
            rows,
            [
                Ok(Transaction::Dispute { client: 1, tx: 1 }),
                Ok(Transaction::Resolve { client: 1, tx: 1 }),
                Ok(Transaction::Chargeback { client: 2, tx: 2 }),
            ]
        );
    }

    #[test]
    fn rejects_bad_rows_with_ids_only() {
        let rows = parse(
            "type,client,tx,amount\n\
             deposit,1,1,1.00001\n\
             deposit,70000,5,1\n\
             deposit,1,6,abc\n\
             deposit,1,7\n\
             refund,1,8,1\n\
             deposit,1,9,1.0000\n",
        );
        let kinds: Vec<_> = rows
            .iter()
            .map(|r| r.as_ref().err().map(|e| e.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                Some(ErrorKind::AmountPrecision),
                Some(ErrorKind::MalformedRow), // client does not fit u16
                Some(ErrorKind::InvalidAmount),
                Some(ErrorKind::MissingAmount),
                Some(ErrorKind::UnknownType),
                None,
            ]
        );
        let first = rows[0].as_ref().unwrap_err();
        assert_eq!(
            first.to_string(),
            "error=amount_precision line=2 client=1 tx=1"
        );
    }

    #[test]
    fn rejects_zero_negative_and_non_plain_amounts() {
        let got = kinds(
            b"type,client,tx,amount\n\
              deposit,1,1,0\n\
              deposit,1,2,-1.0\n\
              withdrawal,1,3,-0.5\n\
              deposit,1,4,1e3\n\
              deposit,1,5,+1\n\
              deposit,1,6,1_000\n\
              deposit,1,7,.5\n",
        );
        use ErrorKind::*;
        let want = [
            Some(NonPositiveAmount),
            Some(NonPositiveAmount),
            Some(NonPositiveAmount),
            Some(InvalidAmount),
            Some(InvalidAmount),
            Some(InvalidAmount),
            None,
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn missing_or_wrong_header_is_fatal() {
        for input in [
            "deposit,1,1,1.0\n",
            "client,type,tx,amount\n1,deposit,1,1.0\n",
        ] {
            let err = read(input.as_bytes()).err().unwrap();
            assert_eq!(
                (err.kind, err.kind.is_fatal()),
                (ErrorKind::InvalidHeader, true)
            );
        }
    }

    #[test]
    fn empty_input_has_no_transactions() {
        assert!(parse("").is_empty());
        assert!(parse("type,client,tx,amount\n").is_empty());
    }

    #[test]
    fn broken_rows_are_skipped_and_reading_continues() {
        let got = kinds(
            b"type,client,tx,amount\n\
              deposit,1,1,1.0,extra\n\
              deposit,1,2,\xff\xfe\n\
              \n\
              deposit,1,3,1.0\n",
        );
        assert_eq!(
            got,
            [
                Some(ErrorKind::MalformedRow),
                Some(ErrorKind::MalformedRow),
                None
            ]
        );
    }

    #[test]
    fn accepts_bom_crlf_and_quoted_fields() {
        let rows = parse("\u{feff}type,client,tx,amount\r\n\"deposit\",\"1\",\"1\",\"1.0\"\r\n");
        assert_eq!(
            rows,
            [Ok(Transaction::Deposit {
                client: 1,
                tx: 1,
                amount: Decimal::new(10, 1)
            })]
        );
    }

    fn amount_of(row: &Result<Transaction, RowError>) -> String {
        match row {
            Ok(Transaction::Deposit { amount, .. }) => amount.to_string(),
            other => panic!("expected a deposit, got {other:?}"),
        }
    }

    #[test]
    fn precision_is_checked_before_decimal_rounds() {
        // Past 28 decimals `from_str` rounds, so precision has to be checked on the text.
        let got = kinds(
            b"type,client,tx,amount\n\
              deposit,1,1,1.00000000000000000000000000001\n\
              deposit,1,2,0.00000000000000000000000000001\n\
              deposit,1,3,1.2.3\n",
        );
        use ErrorKind::*;
        assert_eq!(
            got,
            [
                Some(AmountPrecision),
                Some(AmountPrecision),
                Some(InvalidAmount)
            ]
        );
    }

    #[test]
    fn amounts_are_stored_without_trailing_zeros() {
        // Without normalizing, "1.50000" keeps scale 5 and would print with 5 decimals.
        let rows = parse("type,client,tx,amount\ndeposit,1,1,1.50000\ndeposit,1,2,100.0000\n");
        assert_eq!(
            (amount_of(&rows[0]), amount_of(&rows[1])),
            ("1.5".into(), "100".into())
        );
    }

    #[test]
    fn malformed_row_keeps_the_ids_that_parse() {
        // A bad tx id must not lose the client id: the log line still names the client.
        let rows = parse("type,client,tx,amount\ndeposit,5,99999999999,1\ndeposit,6,7,1,extra\n");
        let ids: Vec<_> = rows
            .iter()
            .map(|r| {
                r.as_ref()
                    .map_err(|e| (e.kind, e.client, e.tx))
                    .unwrap_err()
            })
            .collect();
        assert_eq!(
            ids,
            [
                (ErrorKind::MalformedRow, Some(5), None),
                (ErrorKind::MalformedRow, Some(6), Some(7))
            ]
        );
    }
}
