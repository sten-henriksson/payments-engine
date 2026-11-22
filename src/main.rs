mod account;
mod engine;
mod log;
mod model;
mod reader;

use std::env;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;

use rust_decimal::Decimal;

use account::Account;
use engine::Engine;
use log::LogLine;
use model::Transaction;
use reader::RowError;

const USAGE: &str = "usage: cargo run -- <transactions.csv> > accounts.csv";
const EXIT_USAGE: u8 = 2;

#[derive(Default)]
struct Summary {
    accepted: u64,
    rejected: u64,
}

/// An error that stops the run: nothing more is read or written.
enum Fatal {
    OpenInput(io::Error),
    Read(RowError),
    WriteOutput(io::Error),
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenInput(e) => write!(f, "error=open_input reason={}", e.kind()),
            Self::Read(e) => e.fmt(f),
            Self::WriteOutput(e) => write!(f, "error=write_output reason={}", e.kind()),
        }
    }
}

fn main() -> ExitCode {
    let Some(path) = parse_args(env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::from(EXIT_USAGE);
    };
    match process(&path) {
        Ok(summary) => {
            eprintln!(
                "summary rows_accepted={} rows_rejected={}",
                summary.accepted, summary.rejected
            );
            ExitCode::SUCCESS
        }
        Err(fatal) => {
            eprintln!("{fatal}");
            ExitCode::FAILURE
        }
    }
}

/// Reads the input, applies every transaction and writes the accounts to stdout.
/// The input and its header are checked, and the output header written, before any
/// row is processed, so those failures stop the run before any work is done.
fn process(path: &str) -> Result<Summary, Fatal> {
    let file = open_input(path).map_err(Fatal::OpenInput)?;
    let rows = reader::read(file).map_err(Fatal::Read)?;
    let mut output = csv::Writer::from_writer(io::stdout().lock());
    write_header(&mut output).map_err(Fatal::WriteOutput)?;
    let (engine, summary) = run(rows).map_err(Fatal::Read)?;
    write_accounts(&engine, &mut output).map_err(Fatal::WriteOutput)?;
    Ok(summary)
}

/// Expects exactly one argument: the input path.
fn parse_args(mut args: impl Iterator<Item = String>) -> Option<String> {
    match (args.next(), args.next()) {
        (Some(path), None) => Some(path),
        _ => None,
    }
}

/// Opens the input for reading. Fails if it is missing, not readable, or a directory.
fn open_input(path: &str) -> io::Result<File> {
    let file = File::open(path)?;
    if file.metadata()?.is_dir() {
        return Err(io::ErrorKind::IsADirectory.into());
    }
    Ok(file)
}

/// Writes and flushes the header right away, so an unwritable output (broken pipe,
/// full disk) is reported before any work is done.
fn write_header<W: Write>(csv: &mut csv::Writer<W>) -> io::Result<()> {
    csv.write_record(["client", "available", "held", "total", "locked"])?;
    csv.flush()
}

/// Runs every row through the engine. Rows that fail to parse or that the engine
/// rejects are logged to stderr and skipped. Stops at the first fatal (read) error.
fn run(
    rows: impl IntoIterator<Item = Result<Transaction, RowError>>,
) -> Result<(Engine, Summary), RowError> {
    let mut engine = Engine::new();
    let mut summary = Summary::default();
    for row in rows {
        match row {
            Ok(transaction) => {
                let (client, tx) = transaction.ids();
                match engine.apply(transaction) {
                    Ok(()) => summary.accepted += 1,
                    Err(e) => {
                        summary.rejected += 1;
                        let log = LogLine {
                            reason: e.as_str(),
                            line: None,
                            client: Some(client),
                            tx: Some(tx),
                        };
                        eprintln!("warn {log}");
                    }
                }
            }
            Err(e) if e.kind.is_fatal() => return Err(e),
            Err(e) => {
                summary.rejected += 1;
                eprintln!("warn {e}");
            }
        }
    }
    Ok((engine, summary))
}

/// Writes one row per account, sorted by client, under the header from `write_header`.
/// Amounts always have four decimal places, the same precision the input allows.
fn write_accounts<W: Write>(engine: &Engine, csv: &mut csv::Writer<W>) -> io::Result<()> {
    let mut accounts: Vec<&Account> = engine.accounts().collect();
    accounts.sort_by_key(|a| a.client());

    for a in accounts {
        csv.write_record([
            a.client().to_string(),
            format_money(a.available()),
            format_money(a.held()),
            format_money(a.total()),
            a.locked().to_string(),
        ])?;
    }
    csv.flush()?;
    Ok(())
}

fn format_money(amount: Decimal) -> String {
    // Pad to 4 decimals by hand: `{:.4}` panics in rust_decimal for values near Decimal::MAX.
    let s = amount.to_string();
    let places = model::AMOUNT_SCALE as usize;
    match s.split_once('.') {
        Some((_, frac)) => format!("{s}{}", "0".repeat(places.saturating_sub(frac.len()))),
        None => format!("{s}.{}", "0".repeat(places)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_str(input: &str) -> (Engine, Summary) {
        run(reader::read(input.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn small_input_end_to_end() {
        let input = "type, client, tx, amount\n\
                     deposit, 3, 1, 4.25\n\
                     deposit, 7, 2, 1.5\n\
                     deposit, 3, 3, 0.75\n\
                     withdrawal, 3, 4, 2.0\n\
                     withdrawal, 7, 5, 2.5\n";
        let (engine, summary) = run_str(input);
        let mut csv = csv::Writer::from_writer(Vec::new());
        write_header(&mut csv).unwrap();
        write_accounts(&engine, &mut csv).unwrap();
        assert_eq!(
            String::from_utf8(csv.into_inner().unwrap()).unwrap(),
            "client,available,held,total,locked\n\
             3,3.0000,0.0000,3.0000,false\n\
             7,1.5000,0.0000,1.5000,false\n"
        );
        assert_eq!((summary.accepted, summary.rejected), (4, 1)); // client 7 withdraws 2.5 of 1.5
    }

    #[test]
    fn row_that_does_not_parse_opens_no_account() {
        let (engine, summary) = run_str("type,client,tx,amount\ndeposit,3,1,-5\n");
        assert_eq!((engine.accounts().count(), summary.rejected), (0, 1));
    }

    #[test]
    fn format_money_always_has_four_decimals_even_at_decimal_max() {
        assert_eq!(format_money(Decimal::new(15, 1)), "1.5000");
        assert_eq!(format_money(Decimal::new(2, 0)), "2.0000");
        assert_eq!(format_money(Decimal::new(-12345, 4)), "-1.2345");
        assert_eq!(
            format_money(Decimal::MAX),
            "79228162514264337593543950335.0000"
        );
    }
}
