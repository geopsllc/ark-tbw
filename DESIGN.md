# ark-tbw design

This is the Mainsail version (branch `mainsail`). ark-tbw replaces the two-script [core3-tbw](https://github.com/galperins4/core3-tbw) setup (`tbw.py`
computes, `pay.py` pays, both under pm2) with one Rust service under systemd. This file records
what core3-tbw does, where it breaks, and how ark-tbw handles each case.

## True block weight

Each block the validator proposes is split by the votes **as they stood after that block**: the
voter share (`share.percent` of the reward, plus fees if `share.fees`) goes to voters in
proportion to their weight, and the keep (100% − share) to the validator. A voter earns only for
blocks proposed while it voted. Weight is what the consensus contract counts: the voter's
native balance (the sum of voter balances equals the validator's `validatorVoteBalance`
exactly; checked on testnet).

## Is true block weight possible with the Mainsail API?

It was said it cannot be done: the REST API has no balance-at-height endpoint and no per-block
balance changes, and the JSON-RPC has no `eth_getLogs`, no traces and no `eth_getBlockReceipts`.
It can, by combining two things the API does have:

1. **Every balance change is visible as an event.** `/transactions?timestamp.from&timestamp.to
   &fullReceipt=true` gives every transaction with its receipt: status, `gasUsed` (already net
   of refunds; Σ gasUsed × gasPrice equals the block's `fee`) and its logs. From those:
   - the sender pays gasUsed × gasPrice, also when the call reverts (status 0);
   - value moves from sender to recipient only when the call succeeds;
   - MultiPayment emits `Payment(recipient, amount, success)` per recipient; failed amounts
     come back to the sender in the same call;
   - the consensus contract emits `Voted(voter, validator)` / `Unvoted(voter, validator)`,
     `ValidatorRegistered(addr, blsKey)` (the fee is the call's value) and
     `ValidatorResigned(addr)` (the registration fee is refunded);
   - the block proposer receives the reward plus the block's fees (no burn), and
     `/validators/{v}/blocks?number.from&number.to` lists a validator's blocks.
   Transactions carry their block's timestamp and every block has its own, so ordering
   transactions against blocks needs no per-block reads.
2. **The node keeps account history for the last 256 blocks.** `eth_getBalance(address,
   height)` answers exactly for any height within 256 blocks of the tip (older heights answer
   0, so ark-tbw asks for the node's height in the same batch and never trusts an answer outside
   the window). A JSON-RPC batch counts as one request against the gateway's rate limit; a
   batch of 300 balances works.

So ark-tbw replays the events to keep every voter's balance, and at **every own block within
the window** it reads all voters' balances at that block in one batched call and compares.
Live following is therefore exact by construction: anything the events miss is corrected from
the node, with a warning that names the wallet and the difference.

The only thing no event shows is native value moved *inside* a third-party contract (a contract
that forwards TARK to a voter without a log). During live following the check above covers it.
During a catch-up older than 256 blocks there is no node state to compare with, and the
balances come from the events alone; the reconcile at the tip (below) then compares every
voter's balance with the node's and repairs any difference, so it cannot carry forward.

## How ark-tbw reads the chain

| what | request | volume |
|---|---|---|
| your proposed blocks | `/validators/{you}/blocks?number.from&number.to` | one page per 100 own blocks |
| all transactions with receipts and logs | `/transactions?timestamp.from&timestamp.to&fullReceipt=true` | the whole testnet has a few hundred |
| blocks proposed by voters that are validators | as the first row, per such voter | rare |
| voter balances at each own block within 256 blocks of the tip | one JSON-RPC batch of `eth_getBalance` | one call per own block (~9 minutes) |

The transaction range is split until each part fits a single page (no rows lost or repeated
across page boundaries) and every part's row count is checked against the API's total.

A voter that starts voting is brought in from its own history (`/transactions?address=`
matches sender, recipient and multipayment recipients): current balance minus every change
since the vote, read twice to rule out a transaction landing in between. A fresh install does
the same for all voters at once from two voter-list snapshots around one pass over all
transactions since the start height; when the start height is within the history window the
balances are then taken from the node exactly.

Once caught up, every hour (and at start-up) ark-tbw compares the voter list with the node's
(`/validators/{v}/voters`, votes since our height applied from the logs) and every tracked
wallet's balance with the node's at our height (`eth_getBalance`). Any difference is logged and
repaired.

Measured on testnet (public gateway): an install 7,000 blocks back, before the validator had
even registered, caught up in 13 s; the two voters were brought in from their vote logs, the
last three own blocks were checked against the node (0 differences) and the reconcile matched.
Two installs of the same 21-voter validator, one at the tip (balances from the node) and one
3,000 blocks back (balances from events), ended with byte-identical balances and identical
per-voter amounts.

## Multipayments and gas

The MultiPayment contract (`pay(address[], uint256[])`, value = Σ amounts) has no recipient
limit; the transaction gas limit (5,000,000) is the limit. Measured with `eth_estimateGas` on
testnet: ~42,000 gas base plus ~24,200 per recipient that already exists or ~38,800 per new
address, so **205** existing recipients (4,995,737 gas) or **130** new ones (4,999,310) fit.
ark-tbw packs recipients by that model plus 3% (198 existing per transaction), treats addresses
it does not track as new, caps at 205, and checks every transaction with `eth_estimateGas`
before signing; if an estimate is above the plan, the plan is made again with a heavier model.

The block gas limit is 10,000,000, so two full multipayments fit in one block.

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
| 11 | vote and unvote in the same block compared by timestamp: equal timestamps count as not voting even if the last action was a vote | events are ordered by block, then by their position in the block (`transactionIndex`) |
| 12 | balances are rebuilt from the transaction table only | events replayed, and every own block within the node's 256-block history checked against the node's balances |
| 13 | blocks are processed at the tip | only blocks `network.confirmations` deep are processed; if a processed block's hash changes, the service stops (exit 2) instead of paying twice |

### Payouts

| # | core3-tbw | ark-tbw |
|---|---|---|
| 14 | a payout transaction "accepted" by the pool is marked paid; if it is dropped from the pool later, voters are never paid | paid only when the transaction is on chain with `confirmations`; dropped ones are re-broadcast |
| 15 | a network error during broadcast `quit()`s; the next run re-signs the same payments with a **new nonce**: if the first broadcast had reached the node, voters are **paid twice** | payments are bound to one nonce when signed and stored before broadcast; re-signing only ever reuses that nonce, so each set can confirm once |
| 16 | when donations are enabled, the voter transaction fees are not deducted from the reserve: the validator wallet pays them out of pocket | all fees are charged to the keep |
| 17 | no minimum payout: 1-arktoshi payments | `payout.min_amount`; smaller balances carry over |
| 18 | the reserve account pays a transfer to itself when it is the validator wallet | payments to the paying wallet need no transaction |
| 19 | a multipayment chunk of one recipient is skipped (multipayments need 2+) and stays staged | a single recipient is sent as a transfer; multipayments are packed by gas (up to 205 recipients) and checked with `eth_estimateGas` |
| 20 | only the last transaction's records are stored for multipayments | every payment of every transaction is stored |
| 21 | the fee formula over-estimates size and `quit()`s when the reserve cannot cover fees | the keep is charged gas limit × price at signing and refunded to gas used × price on confirmation; a short keep becomes fee debt paid off by later blocks; payouts are postponed if the wallet balance is short |
| 22 | nonce read from the confirmed wallet only: payments while older transactions sit in the pool collide | payouts wait while the wallet has unconfirmed transactions |
| 23 | when the interval is crossed while catching up, `block_count % interval` stages several payouts in a row | a block-count payout stops processing at the exact block, so each payout covers exactly `every_blocks` own blocks |
| 24 | no way to cancel stuck transactions safely | `ark-tbw cancel-payout ID` sends a zero-value self-transfer at a higher gas price on each pending nonce: whichever transaction takes a nonce wins, and payments whose nonce went to the cancel return to the ledger |
| 24a | (Mainsail) a multipayment recipient that cannot receive with the contract's gas stipend silently gets nothing | the receipt's `Payment(…, success)` logs are read per recipient: a failed amount returns to the voter's unpaid balance and the recipient is paid by its own transfer from then on; if that fails too it is skipped until redirected (`ark-tbw payees`) |
| 24b | (Mainsail) a transaction that reverts still costs gas | a reverted payout transaction returns all its payments to the ledger; the keep pays the gas |

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

The wallet nonce (on Mainsail: the next nonce to use) is read **before** looking up our
transactions by hash, so if one of ours took the nonce, the lookup is guaranteed to see it. A gas
price rejection re-signs the same payments with the same nonce and twice the gas price (within
`payout.max_fee`). A confirmed transaction settles per recipient from its receipt.

## Data

One SQLite file (`ark-tbw.db`): `wallets` (tracked voters' state at the processed height),
`accounts` (unpaid / paid per address), `blocks` and `allocations` (every proposed block and each
voter's part), `payouts`, `payout_items`, `slots` (nonces) and `variants` (signed transactions),
`payees` (recipients that need their own transfer, or are blocked), `share_changes`, `commands`,
`meta`.

Balances in `wallets` are wei (text, 18 decimals). Everything in the ledger (`accounts`,
`blocks`, `allocations`, payouts) is in units of 1e-8 ARK, as on ARK Core: on chain an amount is
units × 10^10 wei. Block rewards and fees are rounded down to units before they are split, so
nothing is shared that did not come in; fees charged to the keep are rounded up.
