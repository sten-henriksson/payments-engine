# payments-engine

Reads a csv of transactions, applies them to the client accounts and writes out the
final balances as csv.

```sh
cargo run -- transactions.csv > accounts.csv
```

Output goes to stdout (`client,available,held,total,locked`, one row per client sorted by client id).
Amounts are always printed with 4 decimals.
Everything else goes to stderr, a `warn` line for each skipped row, a `summary` line at the end, or an `error` line if it fails.

Exit codes: `0` when it ran (also when rows got skipped, the spec says to ignore partner errors and every skipped row is logged anyway), `1` if the input cant be read or the output cant be written, `2` for wrong arguments.

## Assumptions

1. locked accounts block deposits and withdrawals, but disputes, resolves and chargebacks on old transactions still go through so we can still reverse other fraud deposits
2. only deposits can be disputed, disputing a withdrawal is ignored since that money already left the account and there's nothing to hold
3. if the client id on a dispute/resolve/chargeback doesn't match the client who made the tx, it's ignored, otherwise one client could freeze another client's money
4. available can go negative on a dispute if the money was already withdrawn, that's the deposit → withdraw → reverse fraud case so we hold the full amount anyway
5. a tx can only be disputed once, after resolve or chargeback any new dispute on it is ignored, so the same deposit can't be held or reversed twice
6. reusing an existing tx id on a deposit or withdrawal is ignored
7. deposits and withdrawals with missing, zero, negative or more than 4 decimal amounts are rejected
8. broken rows (unknown type, bad fields) get logged to stderr and skipped, the rest keeps processing
9. every transaction that parses opens the client's account if it doesn't exist yet (the spec's "if a client doesn't exist create a new record"), even if it's then rejected. A row that doesn't parse opens no account. Rejected txs don't change any balance
10. logs only have tx id, client id and the reason, never amounts or balances

"ignored" and "rejected" means the same thing here, the row doesnt change any balance and a `warn` line with the reason goes to stderr.

Where each one is enforced and tested:

| # | Enforced in | Test |
|---|---|---|
| 1 | `Account::deposit` / `Account::withdraw` check `locked`, `hold`, `release`, `chargeback` don't | `engine::locked_account_blocks_deposit_and_withdrawal_but_not_disputes`, `account::locked_account_still_allows_dispute_flow` |
| 2 | only deposits are stored, so a dispute on a withdrawal id is `unknown_tx` | `engine::disputing_a_withdrawal_is_rejected` |
| 3 | `find_deposit` → `client_mismatch` | `engine::client_mismatch_is_rejected` |
| 4 | `Account::hold` has no balance check | `engine::dispute_after_withdrawal_takes_available_negative`, `account::hold_moves_available_to_held_even_below_zero` |
| 5 | dispute needs state `Normal`, resolve/chargeback leave it `Resolved`/`Chargeback` | `engine::tx_can_only_be_disputed_once` |
| 6 | `Engine::ensure_new_tx` checks deposit and withdrawal ids → `duplicate_tx` | `engine::reused_tx_id_is_rejected` |
| 7 | `reader::parse_amount` → `missing_amount`, `non_positive_amount`, `amount_precision` | `reader::rejects_zero_negative_and_non_plain_amounts`, `reader::precision_is_checked_before_decimal_rounds`, `reader::rejects_bad_rows_with_ids_only` |
| 8 | `main::run` logs and skips every non-fatal error | `reader::broken_rows_are_skipped_and_reading_continues` |
| 9 | `Engine::apply` opens the account before anything else, account methods change nothing on error | `engine::every_parsed_transaction_opens_the_account_even_when_rejected`, `main::row_that_does_not_parse_opens_no_account` |
| 10 | `RowError` and `EngineError` only carry ids and a reason | `reader::rejects_bad_rows_with_ids_only` |

Some more input rules:

- first row has to be the header `type, client, tx, amount` (spaces are fine). Without it the run stops with `error=invalid_header`, otherwise the first transaction would silently be eaten as the header. An empty file is ok.
- amounts have to be plain decimals like `1`, `1.5`, `.5`. Things like `1e3`, `+1` and `1_000` are `invalid_amount`.
- rows with more then 4 columns or invalid UTF-8 are `malformed_row` and skipped.
- a balance that would overflow is rejected (`overflow`) instead of crashing.
- before any row is processed the input has to open and not be a directory, and the output header has to write ok (catches a broken pipe or full disk early).

## How I know its correct

Part of it is the types:

- `Transaction` is an enum so a dispute can't have an amount and a deposit can't be missing one, that gets checked once when the row is parsed and after that the engine dont have to care.
- every stored deposit has a `TxState` (`Normal` → `Disputed` → `Resolved` or `Chargeback`) and each step checks its in the state it expects before touching any balance. So a resolve before a dispute, or a chargeback after a resolve, just gets rejected.
- `total` is never stored, its always `available + held`, so they cant drift apart.
- money is `Decimal`, never float.
- an operation either goes through completely or changes nothing.

Then the tests. Unit tests sit next to the code (38 of them), `account` for the money math, `engine` for the dispute flow and the assumptions above, `reader` for amounts, header and broken rows, `main` end to end in memory.

`tests/fixtures.rs` runs the actual binary on every `tests/fixtures/**/*.csv` and compares stdout to the `.expected.csv` next to it. Theres one per transaction type, one going through all types and states, and `amount_edge_cases.csv` for weird amounts (too many decimals, trailing zeros, overflow). A fixture without an expected file fails so nothing sits there unused.

```sh
cargo test
```

The one I trust the most is `scripts/dataset.py`. It generates 1M rows of random valid traffic with ~10k edge case rows mixed in, and the expected result comes from a seperate small python model of the same rules (written from the assumptions, not from the rust code). `check` then compares every balance, the count of each reject reason and the summary line.

```sh
python3 scripts/dataset.py gen        # target/datasets/large.csv (fixed seed)
cargo run --release -- target/datasets/large.csv > target/datasets/accounts.csv 2> target/datasets/stderr.log
python3 scripts/dataset.py check
# OK: 5016 accounts, 999947 accepted, 53 rejected
```

## Errors

There's basically three kinds:

- fatal before the run starts: wrong arguments, missing/unreadable file, bad header, stdout not writable. Prints `error=…` to stderr, nothing on stdout, exit 2 for arguments otherwise 1.
- fatal in the middle: input can't be read anymore or output can't be written. Stops, `error=…` on stderr, exit 1.
- a bad row: unknown type, bad amount, duplicate tx, insufficent funds etc. Gets skipped with `warn error=<reason> line=… client=… tx=…` and it keeps going, exit 0.

Errors are just enums (`ErrorKind` in the reader, `AccountError` and `EngineError` in the engine) and `main` is the only place that decides if one gets logged or stops the run. No `unwrap` outside tests, and the balance math uses `checked_add`/`checked_sub`.

## Performance

| Input | Time | Throughput | Peak memory |
|---|---|---|---|
| 1M rows, 32 MB | 1.03 s | ~1 M rows/s | 44 MB |
| 3M rows, 98 MB | 3.04 s | ~1 M rows/s | 84 MB |

Rows are streamed one at a time, the file is never loaded whole. So memory doesn't follow the file size, it follows how many tx ids are stored (~32 bytes each). Accounts are capped at 65536 since client is a u16. Deposits are kept since a dispute can point at any old one, withdrawals only keep the id to catch reuse.

The worst case is if all u32 tx ids were deposits, that would need way more ram then any box has. In a real system I'd keep only a dispute window in memory (disputes have a time limit anyway) and push older deposits to a store.

Most time goes to hash map lookups (~34%) and csv parsing (~21%). I tried fewer hash lookups for deposits and accounts (~1% each) and not allocating strings per row (~3%), none of it beat the noise so I reverted it. It's waiting on memory in the big deposit map, not on code.

Measured with hyperfine (10 runs) and `/usr/bin/time -v` on a release build, Threadripper PRO 5955WX, Linux, with other stuff running on the machine (±5–10% noise).

```sh
python3 scripts/dataset.py gen --rows 3000000 --out target/datasets/bench3m.csv
cargo build --release
hyperfine --warmup 3 --runs 10 -N --output=null 'target/release/payments-engine target/datasets/bench3m.csv'
```

### lots of concurrent streams?

The engine is single threaded on purpose. If this was inside a server getting csv from thousands of tcp connections I would shard by client id. Every rule only touches one client's account and that client's own deposits (other clients get `client_mismatch`), so each shard can own its clients without any locks. Connections parse their rows in parallell and send each one to the right shard over a channel, order per client is kept which is all that matters.

The only thing that crosses shards is "tx id is globally unique", that needs a shared set or split up tx id ranges, or you trust upstream on it.
