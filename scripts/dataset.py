#!/usr/bin/env python3
"""Mint a large, deterministic transactions CSV with edge cases, plus the expected result.

    python3 scripts/dataset.py gen --rows 1000000
    cargo run --release -- target/datasets/large.csv \\
        > target/datasets/accounts.csv 2> target/datasets/stderr.log
    python3 scripts/dataset.py check

`gen` writes the CSV and `<csv>.expected.json`: final balances per client, the
accepted count and the count of every rejection reason, computed by a small
independent model of the README assumptions. `check` compares a run against it.

The CSV is mostly valid random traffic (deposits, withdrawals and dispute flows
over many clients) with edge-case rows mixed in at random positions. Edge cases
use their own client ids (60000+) and tx ids (3_000_000_000+), so they never
collide with the random traffic.
"""
import argparse
import collections
import json
import random
import sys
import decimal
from decimal import Decimal
from pathlib import Path

decimal.getcontext().prec = 50  # the default 28 digits can't hold Decimal::MAX exactly
SCALE = 10_000  # amounts are held as integers in units of 0.0001
DECIMAL_MAX = 79228162514264337593543950335  # rust_decimal::Decimal::MAX
OUT = Path("target/datasets/large.csv")


class Model:
    """The README rules, written independently of the Rust code."""

    def __init__(self):
        self.accounts = {}  # client -> [available, held, locked]
        self.deposits = {}  # tx -> [client, amount, state]
        self.withdrawals = set()
        self.reasons = collections.Counter()
        self.accepted = 0

    def open(self, client):
        return self.accounts.setdefault(client, [0, 0, False])

    def reject(self, reason):
        self.reasons[reason] += 1
        return False

    def apply(self, kind, client, tx, amount):
        acct = self.open(client)  # assumption 9: opened even if rejected below
        if kind in ("deposit", "withdrawal"):
            if tx in self.deposits or tx in self.withdrawals:
                return self.reject("duplicate_tx")
            if acct[2]:
                return self.reject("account_locked")
            if kind == "deposit":
                if max(acct[0], acct[0] + acct[1]) + amount > DECIMAL_MAX * SCALE:
                    return self.reject("overflow")
                acct[0] += amount
                self.deposits[tx] = [client, amount, "normal"]
            else:
                if amount > acct[0]:
                    return self.reject("insufficient_funds")
                acct[0] -= amount
                self.withdrawals.add(tx)
        else:
            dep = self.deposits.get(tx)
            if dep is None:
                return self.reject("unknown_tx")
            if dep[0] != client:
                return self.reject("client_mismatch")
            if dep[2] != ("normal" if kind == "dispute" else "disputed"):
                return self.reject("invalid_state")
            if kind == "dispute":
                acct[0] -= dep[1]
                acct[1] += dep[1]
                dep[2] = "disputed"
            elif kind == "resolve":
                acct[1] -= dep[1]
                acct[0] += dep[1]
                dep[2] = "resolved"
            else:
                acct[1] -= dep[1]
                acct[2] = True
                dep[2] = "chargeback"
        self.accepted += 1
        return True

    def run(self, row):
        if row.tx is not None:
            self.apply(*row.tx)
        else:
            self.reject(row.reason)  # a row that doesn't parse opens nothing


class Row:
    """One CSV line. `tx` is (kind, client, tx, amount) if the reader should accept
    it; otherwise `reason` is the reader's rejection and `client` the readable id."""

    __slots__ = ("raw", "tx", "reason", "client")

    def __init__(self, raw, tx=None, reason=None, client=None):
        self.raw = raw if isinstance(raw, bytes) else raw.encode()
        self.tx, self.reason, self.client = tx, reason, client


def amount_text(units, rng):
    whole, frac = divmod(units, SCALE)
    full = f"{whole}.{frac:04d}"
    style = rng.random()
    if style < 0.4:
        return full  # 12.5000
    if frac == 0 and style < 0.7:
        return str(whole)  # 12
    return full.rstrip("0").rstrip(".") if frac else f"{whole}.0"  # 12.5


def tx_row(kind, client, tx, amount, rng):
    if amount is None:
        raw = f"{kind}, {client}, {tx}" + ("," if rng.random() < 0.5 else "")
    else:
        raw = f"{kind}, {client}, {tx}, {amount_text(amount, rng)}"
    return Row(raw, tx=(kind, client, tx, amount))


def random_traffic(rows, clients, rng):
    """Valid traffic only: a model tracks balances so nothing here gets rejected."""
    model, out, next_tx = Model(), [], 1
    normal = collections.defaultdict(list)  # client -> undisputed deposit ids
    disputed = collections.defaultdict(list)

    def take(ids):
        i = rng.randrange(len(ids))
        ids[i], ids[-1] = ids[-1], ids[i]
        return ids.pop()

    while len(out) < rows:
        client = rng.randint(1, clients)
        acct = model.open(client)
        roll = rng.random()
        if roll < 0.08 and normal[client]:
            kind, tx, amount = "dispute", take(normal[client]), None
            disputed[client].append(tx)
        elif roll < 0.14 and disputed[client]:
            kind = "chargeback" if rng.random() < 0.002 else "resolve"
            tx, amount = take(disputed[client]), None
        elif acct[2]:
            continue  # locked: only the dispute flow above still applies
        elif roll < 0.40 and acct[0] > 0:
            kind, tx, amount = "withdrawal", next_tx, rng.randint(1, acct[0])
            next_tx += 1
        else:
            kind, tx, amount = "deposit", next_tx, rng.randint(1, 100_000_000)
            next_tx += 1
            normal[client].append(tx)
        assert model.apply(kind, client, tx, amount), (kind, client, tx)
        out.append(tx_row(kind, client, tx, amount, rng))
    return out


def edge_cases(rng):
    """Each scenario uses fresh client and tx ids. Comments name the README assumption."""
    out = []
    clients = iter(range(60_000, 65_535))
    txs = iter(range(3_000_000_000, 4_000_000_000))
    U = SCALE  # 1.0

    def ok(kind, c, t, amount=None):
        out.append(tx_row(kind, c, t, amount, rng))

    def bad(raw, reason, client=None):
        out.append(Row(raw, reason=reason, client=client))

    def raw_ok(raw, kind, c, t, amount=None):
        out.append(Row(raw, tx=(kind, c, t, amount)))

    # 1: locked blocks deposit and withdrawal, dispute flow on old deposits still works
    c, d1, d2 = next(clients), next(txs), next(txs)
    ok("deposit", c, d1, 10 * U)
    ok("deposit", c, d2, 5 * U)
    ok("dispute", c, d1)
    ok("chargeback", c, d1)
    ok("deposit", c, next(txs), 1 * U)  # account_locked
    ok("withdrawal", c, next(txs), 1 * U)  # account_locked
    ok("dispute", c, d2)
    ok("resolve", c, d2)

    # 2: disputing a withdrawal is ignored
    c, d, w = next(clients), next(txs), next(txs)
    ok("deposit", c, d, 5 * U)
    ok("withdrawal", c, w, 2 * U)
    ok("dispute", c, w)  # unknown_tx

    # 3: client mismatch on dispute/resolve/chargeback is ignored
    a, b, d = next(clients), next(clients), next(txs)
    ok("deposit", a, d, 5 * U)
    ok("dispute", b, d)  # client_mismatch
    ok("dispute", a, d)
    ok("resolve", b, d)  # client_mismatch
    ok("chargeback", b, d)  # client_mismatch
    ok("resolve", a, d)

    # 4: dispute after withdrawal takes available negative
    c, d = next(clients), next(txs)
    ok("deposit", c, d, 5 * U)
    ok("withdrawal", c, next(txs), 5 * U)
    ok("dispute", c, d)

    # 5: a tx can only be disputed once; resolve/chargeback need a dispute first
    c, d1, d2, d3 = next(clients), next(txs), next(txs), next(txs)
    ok("deposit", c, d1, 3 * U)
    ok("deposit", c, d2, 3 * U)
    ok("deposit", c, d3, 3 * U)
    ok("resolve", c, d3)  # invalid_state
    ok("chargeback", c, d3)  # invalid_state
    ok("dispute", c, d1)
    ok("dispute", c, d1)  # invalid_state
    ok("resolve", c, d1)
    ok("dispute", c, d1)  # invalid_state
    ok("dispute", c, d2)
    ok("chargeback", c, d2)
    ok("dispute", c, d2)  # invalid_state

    # 6: reused tx ids on deposit/withdrawal, also across clients
    a, b, t1, t2 = next(clients), next(clients), next(txs), next(txs)
    ok("deposit", a, t1, 5 * U)
    ok("deposit", a, t1, 5 * U)  # duplicate_tx
    ok("withdrawal", a, t1, 1 * U)  # duplicate_tx
    ok("withdrawal", a, t2, 1 * U)
    ok("withdrawal", a, t2, 1 * U)  # duplicate_tx
    ok("deposit", a, t2, 1 * U)  # duplicate_tx
    ok("deposit", b, t1, 1 * U)  # duplicate_tx, other client

    # 7: missing, zero, negative, too precise and non-plain amounts
    for kind in ("deposit", "withdrawal"):
        c = next(clients)
        bad(f"{kind}, {c}, {next(txs)}", "missing_amount", c)
        bad(f"{kind}, {c}, {next(txs)},", "missing_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, 0", "non_positive_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, 0.0000", "non_positive_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, -1.5", "non_positive_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, 1.00001", "amount_precision", c)
        bad(f"{kind}, {c}, {next(txs)}, 1e3", "invalid_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, +1", "invalid_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, 1_000", "invalid_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, abc", "invalid_amount", c)
        bad(f"{kind}, {c}, {next(txs)}, 1.2.3", "invalid_amount", c)

    # 8: broken rows are skipped, processing continues
    c = next(clients)
    bad(f"refund, {c}, {next(txs)}, 1.0", "unknown_type", c)
    bad(f"Deposit, {c}, {next(txs)}, 1.0", "unknown_type", c)
    bad(f", {c}, {next(txs)}, 1.0", "unknown_type", c)
    bad(f"deposit, 70000, {next(txs)}, 1.0", "malformed_row")  # client > u16
    bad(f"deposit, -1, {next(txs)}, 1.0", "malformed_row")
    bad(f"deposit, {c}, 4294967296, 1.0", "malformed_row")  # tx > u32
    bad(f"deposit, , {next(txs)}, 1.0", "malformed_row")
    bad(f"deposit, {c}, {next(txs)}, 1.0, extra", "malformed_row")
    bad(b"deposit, 1, 1, \xff\xfe", "malformed_row")  # invalid UTF-8
    bad("!!!", "malformed_row")

    # 9: any rejected transaction that parses opens the account (zero balances);
    #    a row that doesn't parse opens nothing
    c = next(clients)
    bad(f"deposit, {c}, {next(txs)}, -5", "non_positive_amount", c)  # no account
    ok("withdrawal", next(clients), next(txs), 5 * U)  # insufficient_funds, account opened
    ok("dispute", next(clients), next(txs))  # unknown_tx, account opened

    # withdrawals can't go below zero
    c = next(clients)
    ok("deposit", c, next(txs), 1 * U)
    ok("withdrawal", c, next(txs), 5 * U)  # insufficient_funds
    ok("withdrawal", c, next(txs), 1 * U)  # exactly to zero

    # overflow is rejected, not a crash
    c, t = next(clients), next(txs)
    raw_ok(f"deposit, {c}, {t}, {DECIMAL_MAX}", "deposit", c, t, DECIMAL_MAX * SCALE)
    ok("deposit", c, next(txs), 1 * U)  # overflow

    # boundaries: smallest and largest client and tx ids, smallest amount
    ok("deposit", 0, 0, 1)
    ok("deposit", 65_535, 4_294_967_295, 1)

    # formatting the reader must accept
    c = next(clients)
    t = next(txs)
    raw_ok(f'"deposit","{c}","{t}","1.5"', "deposit", c, t, 15_000)
    t = next(txs)
    raw_ok(f"deposit,\t{c} ,  {t},1.0", "deposit", c, t, U)
    t = next(txs)
    raw_ok(f"deposit, {c}, {t}, .5", "deposit", c, t, 5_000)
    t = next(txs)
    raw_ok(f"deposit, {c}, {t}, 5.", "deposit", c, t, 5 * U)
    t = next(txs)
    raw_ok(f"deposit, {c}, {t}, 1.50000000", "deposit", c, t, 15_000)  # trailing zeros
    t = next(txs)
    raw_ok(f"deposit, {c}, {t}, 2.0\r", "deposit", c, t, 2 * U)  # CRLF line
    raw_ok(f"dispute, {c}, {t}, 99.0", "dispute", c, t, None)  # amount ignored
    raw_ok(f"resolve, {c}, {t}", "resolve", c, t, None)

    # precision: 10,000 deposits of 0.0001 add up to exactly 1
    c = next(clients)
    for _ in range(10_000):
        ok("deposit", c, next(txs), 1)
    return out


def merge(traffic, edges, rng):
    """Spreads edge rows through the traffic, keeping each list's own order."""
    total = len(traffic) + len(edges)
    edge_at = set(rng.sample(range(total), len(edges)))
    t, e, out = iter(traffic), iter(edges), []
    for i in range(total):
        out.append(next(e) if i in edge_at else next(t))
    return out


def gen(args):
    rng = random.Random(args.seed)
    edges = edge_cases(rng)
    rows = merge(random_traffic(max(args.rows - len(edges), 0), args.clients, rng), edges, rng)

    model = Model()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "wb") as f:
        f.write(b"type, client, tx, amount\n")
        for i, row in enumerate(rows):
            f.write(row.raw + b"\n")
            if i % 50_000 == 0:
                f.write(b"\n")  # blank lines are skipped, not rejected
            model.run(row)

    expected = {
        "accounts": {str(c): a for c, a in model.accounts.items()},
        "accepted": model.accepted,
        "reasons": dict(model.reasons),
    }
    expected_path = args.out.with_suffix(".expected.json")
    expected_path.write_text(json.dumps(expected))
    print(
        f"wrote {args.out} ({len(rows)} rows, {len(edges)} edge-case rows, "
        f"{len(model.accounts)} clients) and {expected_path}",
        file=sys.stderr,
    )


def check(args):
    expected = json.loads(args.csv.with_suffix(".expected.json").read_text())
    problems = []

    actual = {}
    lines = args.accounts.read_text().splitlines()
    if lines[0] != "client,available,held,total,locked":
        problems.append(f"bad header: {lines[0]}")
    for line in lines[1:]:
        c, avail, held, total, locked = line.split(",")
        actual[c] = (Decimal(avail), Decimal(held), Decimal(total), locked == "true")
    for c, (avail, held, locked) in expected["accounts"].items():
        want = (Decimal(avail) / SCALE, Decimal(held) / SCALE, Decimal(avail + held) / SCALE, locked)
        got = actual.pop(c, None)
        if got != want:
            problems.append(f"client {c}: expected {want}, got {got}")
    problems += [f"unexpected client {c}" for c in actual]

    reasons, summary = collections.Counter(), None
    for line in args.stderr.read_text().splitlines():
        if line.startswith("warn "):
            reasons[line.split()[1].removeprefix("error=")] += 1
        elif line.startswith("summary "):
            summary = dict(kv.split("=") for kv in line.split()[1:])
        else:
            problems.append(f"unexpected stderr line: {line}")
    if dict(reasons) != expected["reasons"]:
        problems.append(f"reasons: expected {expected['reasons']}, got {dict(reasons)}")
    want_summary = {"rows_accepted": str(expected["accepted"]), "rows_rejected": str(sum(expected["reasons"].values()))}
    if summary != want_summary:
        problems.append(f"summary: expected {want_summary}, got {summary}")

    for p in problems[:20]:
        print(p)
    print(f"{'FAIL' if problems else 'OK'}: {len(expected['accounts'])} accounts, "
          f"{expected['accepted']} accepted, {sum(expected['reasons'].values())} rejected"
          + (f", {len(problems)} problems" if problems else ""))
    return 1 if problems else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gen", help="write the dataset and its expected result")
    g.add_argument("--rows", type=int, default=1_000_000)
    g.add_argument("--clients", type=int, default=5_000)
    g.add_argument("--seed", type=int, default=42)
    g.add_argument("--out", type=Path, default=OUT)
    c = sub.add_parser("check", help="compare a run against the expected result")
    c.add_argument("--csv", type=Path, default=OUT)
    c.add_argument("--accounts", type=Path, default=OUT.parent / "accounts.csv")
    c.add_argument("--stderr", type=Path, default=OUT.parent / "stderr.log")
    args = p.parse_args()
    sys.exit(gen(args) if args.cmd == "gen" else check(args))


if __name__ == "__main__":
    main()
