# ark-tbw design

ark-tbw replaces the two-script [core3-tbw](https://github.com/galperins4/core3-tbw) setup (`tbw.py`
computes, `pay.py` pays, both under pm2) with one Rust service under systemd. This file records
what core3-tbw does, where it breaks, and how ark-tbw handles each case.

## True block weight

Each block the validator forges is split by the votes **as they stood after that block**: the
voter share (`share.percent` of the reward, plus fees if `share.fees`) goes to voters in
proportion to their weight, and the keep (100% − share) to the validator. A voter earns only for
blocks forged while it voted. Weight is what Core counts: balance plus HTLC-locked balance.

## Why core3-tbw is slow

For **every block** and **every voter** it runs four queries over the whole `transactions`
table (`SUM` inbound, outbound, multipayments through `jsonb @>`, block rewards), plus two
`jsonb` scans for votes. Cost grows with blocks × voters × chain size. It also needs direct
access to the relay's PostgreSQL.

## How ark-tbw reads the chain

It keeps every voter's balance itself and only applies **changes**, read once, in order, from
the public API (your relay's or api.ark.io):

| what | request | volume on mainnet |
|---|---|---|
| your forged blocks | `/delegates/{you}/blocks?height.from&height.to` | ~2 requests per day of chain |
| all transactions | `/transactions?timestamp.from&timestamp.to` | ~220 transactions per day |
| blocks forged by voters that are validators | same as the first row, per such voter | rare |

A transaction carries its block's timestamp and every block has its own timestamp, so ordering
transactions against your blocks needs no per-block reads. The transaction range is split until
each part fits a single page (no rows lost or repeated across page boundaries) and every
part's row count is checked against the API's total.

Measured on mainnet (api.ark.io, rate-limited to 170 requests/min):

- a fresh install from 14,000 blocks back: **24 requests, 5 s**; the 211-block payout cycle
  matched geops' real core3-tbw payout to within 0.00002 ARK per voter (float rounding in
  core3-tbw; ark-tbw is exact integer math and pays voters exactly 211.00000000 ARK)
- a fresh install from 30 days (324,000 blocks) back: init 212 requests, 69 s; catching up
  through 6,357 own blocks and 30 payouts took about 3 minutes more, after which all 388
  voters matched the node

A voter that starts voting is brought in from its own history: current balance minus every
transaction since the vote, read twice to rule out a transaction landing in between. A fresh
install does the same for all voters at once from two voter-list snapshots around one pass over
all transactions since the start height.

Once caught up, every hour (and at start-up) ark-tbw compares each voter's computed balance
and vote with the node's. Any difference is logged and repaired from the wallet's history.

## Edge cases found in core3-tbw and how ark-tbw handles them

### Allocation

| # | core3-tbw | ark-tbw |
|---|---|---|
| 1 | `voter_share` + `delegate_fee` percentages are never checked: misconfigured, part of each block vanishes or is overpaid | there is only `share.percent`; the keep is always the rest, so the split is 100% by construction |
| 2 | anti-dilution adds unpaid rewards **after** the minimum check: a voter that drops below the minimum keeps earning on its unpaid balance | eligibility is decided on the voter's own stake; unpaid is added only for eligible voters |
| 3 | anti-dilution is added after the cap, so capped voters exceed the cap | cap applies to own stake + unpaid |
| 4 | minimum is `>` (exactly 10 ARK with min 10 is excluded) | minimum is inclusive (`>=`) |
| 5 | floats on balances above 2^53 arktoshi (~90M ARK) and `int()` truncation per voter: dust is lost and totals never add up | u128 integer math; each block splits to exactly reward + fee, dust goes to the keep |
| 6 | block fees always go to the first of the validator's own fee accounts | configurable: `share.fees` |
| 7 | the validator wallet voting for itself earns on rewards it holds for voters | excluded by default; with `include_self` its weight is its balance minus what it holds for others |
| 8 | blacklisted and below-minimum weight is always redistributed | `ineligible = "redistribute"` or `"keep"` |
| 9 | custom rates edited by hand in the database, prompted from stdin | `[share.custom]` in the config; limited to 100% so the keep never goes negative |
| 10 | changing the share means editing the config and restarting; old and new share mix inside a cycle with no record | `ark-tbw share set P [--from-height H]` applies from an exact block while running; every block records the share it used |
| 11 | vote and unvote in the same block compared by timestamp: equal timestamps count as not voting even if the last action was a vote | events are ordered by block, then by the sender's nonce |
| 12 | HTLC lock amounts are counted as received by the recipient at lock time; the locked balance is not weight | lock / claim / refund follow Core: locked balance counts as weight, the recipient gets it on claim |
| 13 | blocks are processed at the tip: a block that is later orphaned in a fork is paid anyway | only blocks `network.confirmations` deep are processed; if a processed block changes, the service stops (exit 2) instead of paying twice |

### Payouts

| # | core3-tbw | ark-tbw |
|---|---|---|
| 14 | a payout transaction "accepted" by the pool is marked paid; if it is dropped from the pool later, voters are never paid | paid only when the transaction is on chain with `confirmations`; dropped ones are re-broadcast |
| 15 | a network error during broadcast `quit()`s; the next run re-signs the same payments with a **new nonce**: if the first broadcast had reached the node, voters are **paid twice** | payments are bound to one nonce when signed and stored before broadcast; re-signing only ever reuses that nonce, so each set can confirm once |
| 16 | when donations are enabled, the voter transaction fees are not deducted from the reserve: the validator wallet pays them out of pocket | all fees are charged to the keep |
| 17 | no minimum payout: 1-arktoshi payments | `payout.min_amount`; smaller balances carry over |
| 18 | the reserve account pays a transfer to itself when it is the validator wallet | payments to the paying wallet need no transaction |
| 19 | a multipayment chunk of one recipient is skipped (multipayments need 2+) and stays staged | a single recipient is sent as a transfer |
| 20 | only the last transaction's records are stored for multipayments | every payment of every transaction is stored |
| 21 | the fee formula over-estimates size and `quit()`s when the reserve cannot cover fees | exact minimum fee (ARK Wallet's verified formula); a short keep becomes fee debt paid off by later blocks; payouts are postponed if the wallet balance is short |
| 22 | nonce read from the confirmed wallet only: payments while older transactions sit in the pool collide | payouts wait while the wallet has unconfirmed transactions |
| 23 | when the interval is crossed while catching up, `block_count % interval` stages several payouts in a row | a block-count payout stops processing at the exact block, so each payout covers exactly `every_blocks` own blocks |
| 24 | no way to cancel stuck transactions safely | `ark-tbw cancel-payout ID` sends a 1-arktoshi self-transfer on each pending nonce: whichever transaction takes a nonce wins, and payments whose nonce went to the cancel return to the ledger |

### Operations

| # | core3-tbw | ark-tbw |
|---|---|---|
| 25 | two pm2 processes sharing a SQLite file without transactions; a crash between statements leaves balances half-updated | one process; every chunk of blocks and every payout step is one SQLite transaction (WAL, synchronous=FULL) |
| 26 | passphrase in the main config | separate secret file that must be `chmod 600` and owned by the service user, or a systemd credential; the passphrase is checked against the validator's public key (and second public key) before anything is signed |
| 27 | settings like `manual_pay` / `custom` are flags you set, run, and unset | commands: `pay-now`, `share set`, `cancel-payout`, picked up by the running service |
| 28 | fixed paths under `~/core3-tbw` | `--config` anywhere; database next to it |

## Payout state machine

```
plan ──► signed (stored, ledger debited) ──► broadcast ──► confirmed (paid)
                                      │
                                      └─► nonce taken by a transaction that is not one of
                                          ours for this set ──► failed (back to the ledger,
                                          fee refunded to the keep)
```

The wallet nonce is read **before** looking up our transactions by id, so if one of ours took the
nonce, the lookup is guaranteed to see it. A fee rejection re-signs the same payments with the
same nonce and twice the fee (up to `payout.max_fee`).

## Data

One SQLite file (`ark-tbw.db`): `wallets` (tracked voters' state at the processed height),
`accounts` (unpaid / paid per address), `blocks` and `allocations` (every forged block and each
voter's part), `payouts`, `payout_items`, `slots` (nonces) and `variants` (signed transactions),
`share_changes`, `commands`, `meta`.
