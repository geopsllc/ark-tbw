# ark-tbw (Mainsail)

All-in-one **true block weight** reward sharing for ARK Mainsail validators: it follows the
chain, splits every block you propose between your voters by their weight at that block, and
pays them. One binary, one SQLite file, run by systemd.

> This is the `mainsail` branch, for the current Mainsail testnet (chain id 11812, TARK). The
> ARK Core v3 version is on `main`. The two use different databases.

- **Exact:** voter balances follow the chain's transactions, and every block you propose
  within the node's 256-block account history is checked against the node's own balances
  (one batched JSON-RPC call). Integer math; every block splits to exactly reward + fees.
- **Fast:** reads only your blocks and the chain's transactions, not every block for every voter.
- **Safe payouts:** a set of payments is bound to one wallet nonce when it is signed, so it can
  never be paid twice, even across crashes, restarts or network errors.
- **Big multipayments:** up to 205 voters per transaction, packed by gas and checked with
  `eth_estimateGas` before signing.
- **Live changes:** change the share from any block on, pay now, cancel a stuck payout, reload
  the config, all while it runs.

How it works, and why true block weight is possible with the existing Mainsail API:
[DESIGN.md](DESIGN.md).

Runs on Ubuntu 22.04, 24.04 and 26.04 (x86_64 and arm64). The binary is static: no runtime
dependencies.

## Install

Build a static binary (on any machine with Docker), or build on the server with Rust:

```bash
tools/build-linux.sh            # dist/ark-tbw-x86_64-linux and dist/ark-tbw-aarch64-linux
```

```bash
cargo build --release           # on the server itself: target/release/ark-tbw
```

Install it and create the config:

```bash
sudo install -m 755 dist/ark-tbw-x86_64-linux /usr/local/bin/ark-tbw
mkdir -p ~/.ark-tbw && cp config.example.toml ~/.ark-tbw/config.toml
nano ~/.ark-tbw/config.toml
```

Your node serves the REST API on port 4003, JSON-RPC on 4008 and the transaction pool on 4007;
ark-tbw finds the other two from `network.api`. Without a node of your own, use the public
gateway `https://testnet.mainsailhq.com/api`.

The passphrase lives in its own file, readable only by you. Mainsail keeps the ARK key scheme,
so it is the same passphrase as the validator's wallet:

```bash
install -m 600 /dev/null ~/.ark-tbw/secret.toml
nano ~/.ark-tbw/secret.toml     # passphrase = "..."   (and second_passphrase = "..." if set)
```

Initialise: this reads your voters as of the start height (default: now).

```bash
ark-tbw init                         # start now
ark-tbw init --from-height 24700000  # or catch up and pay from a given block
```

Check everything (node, JSON-RPC history, pool, validator, secret, balance) without changing
anything:

```bash
ark-tbw check
```

To try it first without sending anything, set `payout.enabled = false` (shadow mode): payouts
are computed and recorded (`ark-tbw payouts`) but no transaction is made.

## Run as a service

ark-tbw always runs as your own user, never as root. There are two ways to install the service:
a system service (the default) or a rootless user service.

### System service (default)

```bash
sudo ark-tbw service install    # writes /etc/systemd/system/ark-tbw.service, enabled at boot
ark-tbw start
```

sudo is only needed to write the unit file into `/etc/systemd/system` and to control the
service: `ark-tbw start`, `stop`, `restart` and `reload` call `sudo systemctl` for you. The
program itself runs as the user who ran `sudo` (or `--user NAME`), with extra systemd sandboxing
(read-only system directories, private /tmp). Reading the log needs sudo unless your user is in
the `adm` or `systemd-journal` group (`sudo usermod -aG adm $USER`, then log in again).

### Rootless user service (no sudo)

```bash
ark-tbw service install --rootless    # writes ~/.config/systemd/user/ark-tbw.service
ark-tbw start
```

The service belongs to your account's own systemd instance, so installing, start/stop/restart/
reload and the log need no sudo at all. The commands are the same; ark-tbw sees which kind is
installed.

One thing needs an admin, once: **lingering**. Without it, a user service runs only while you
are logged in, and does not start at boot. `service install --rootless` turns it on when it is
allowed to, and otherwise tells you to run:

```bash
sudo loginctl enable-linger $USER
```

Run the install from a real login to the account (`ssh user@host`). After `su` or `sudo -u` it
works once lingering is on; before that the user manager is not running and install says so.

The user service has the same restart behaviour but leaves out the sandboxing options that need
root to set up. Only one kind can be installed at a time: remove one
(`ark-tbw service uninstall`, with sudo for the system service) before installing the other.

### Commands

| | |
|---|---|
| `ark-tbw start` / `stop` / `restart` | through systemctl (with sudo for the system service). A stop finishes the current step first |
| `ark-tbw reload` | re-read the config without a restart (SIGHUP) |
| `ark-tbw logs` | last 100 log lines |
| `ark-tbw logs -f` | follow the log |
| `ark-tbw logs --since "1 hour ago"` | log since a time |
| `ark-tbw service show [--rootless]` | print the unit file without installing it |
| `ark-tbw service uninstall` | remove the service, with sudo for the system service (config and database stay) |

The service restarts itself on failure. It stays stopped (exit code 2) on errors a restart cannot
fix, such as a bad config or a processed block that changed, so you can look first.
`ark-tbw logs` reads journald, the same as `journalctl -u ark-tbw` (rootless:
`journalctl --user -u ark-tbw`).

## Day to day

```bash
ark-tbw status              # sync height, share, next payout, unpaid totals
ark-tbw voters              # weight, unpaid and paid per voter (--all: incl. former voters)
ark-tbw blocks              # your last proposed blocks and how each was split
ark-tbw payouts             # payout history;  ark-tbw payouts 12  for one payout in detail
ark-tbw pay-now             # pay out now instead of waiting for the next trigger
ark-tbw payees              # recipients that cannot be paid through the multipayment
```

### Changing the share

```bash
ark-tbw share set 60                       # from the next block on
ark-tbw share set 55 --from-height 24800000
ark-tbw share                              # the schedule
```

The keep is always 100% − share. Blocks already processed keep the share they were split with
(shown per block in `ark-tbw blocks`). The running service applies changes without a restart.

### Redirecting a voter's rewards

```toml
[redirect]
"0xvoter..." = "0xpayto..."
```

The voter's rewards are still computed and tracked on its own address; only the payment goes to
the other address. Run `ark-tbw reload` afterwards.

### Recipients that cannot receive

The MultiPayment contract forwards each amount with a small gas stipend. A recipient that is a
contract needing more gas to receive gets nothing from it: the contract reports that payment as
failed and returns the amount to your wallet in the same transaction. ark-tbw reads that result
per recipient, puts the amount back on the voter's unpaid balance and pays that recipient with
its own transfer from then on. If even a plain transfer fails, the recipient is skipped (its
rewards keep accruing) until you set a `[redirect]` for it. `ark-tbw payees` lists them;
`ark-tbw unblock 0x...` tries the multipayment again.

### Stuck payouts

A payout waits for each of its transactions to be confirmed. One that drops out of the pool is
broadcast again; a gas price rejection re-signs it with the same nonce and twice the gas price
(within `payout.max_fee`). If a transaction still never confirms (`ark-tbw payouts ID`), cancel
it:

```bash
ark-tbw cancel-payout ID
```

This takes the transaction's nonce with a zero-value transfer to your own wallet at a higher gas
price. Whatever takes the nonce first wins: if the payment does, it is paid; if the cancel does,
the amounts go back to the voters' unpaid balances for the next payout. Nothing can be paid twice.

## Configuration

See [config.example.toml](config.example.toml); every option is documented there. The main ones:

| option | |
|---|---|
| `network.api` | your node (`http://127.0.0.1:4003/api`, fastest) or `https://testnet.mainsailhq.com/api` |
| `network.rpc`, `network.tx_api` | JSON-RPC and pool API; derived from `network.api` by default |
| `network.confirmations` | blocks kept back from the tip (default 3; must stay well under 256) |
| `share.percent` | share for voters; the keep is the rest |
| `share.keep_address` | where the keep goes (default: stays in the validator wallet) |
| `share.min_vote`, `share.max_vote` | minimum weight to be paid, and the cap on counted weight |
| `share.fees` | share transaction fees too (default: they go to the keep) |
| `share.anti_dilution` | count unpaid rewards as weight |
| `share.blacklist`, `share.whitelist`, `[share.custom]` | exclude, restrict, or set per-voter rates |
| `payout.every_blocks`, `payout.at` | pay every N of your own blocks and/or at UTC times |
| `payout.min_amount` | smaller balances carry over |
| `payout.gas_price`, `payout.max_fee` | gas price in gwei (default: network minimum), and the most one transaction may cost |
| `payout.enabled` | `false` = shadow mode |

Fees are gas used × gas price and are paid from the keep: it is charged gas limit × price when a
transaction is signed and refunded the difference once it confirms. If the keep is short, the
fees are carried as keep debt and paid off by later blocks. A 200-voter multipayment uses about
4.9 million gas, 0.0245 TARK at 5 gwei.

Amounts in the database and on screen have 8 decimals, as on ARK Core; on chain they are wei
(18 decimals). A voter's weight is its balance rounded down to 8 decimals.

## Development

```bash
cargo test                                          # unit tests (no network)
cargo test broadcast_unfunded -- --ignored          # posts a multipayment from an empty testnet wallet; must be rejected
tools/test-ubuntu.sh                                # runs the Linux binary on Ubuntu 22.04 / 24.04 / 26.04 in Docker
```

## Credits

The true block weight approach and the core3-tbw feature set this replaces are by
[galperins4](https://github.com/galperins4/core3-tbw). ark-tbw is a new implementation and shares
no code with it. Transaction signing comes from [ARK Wallet](https://github.com/geopsllc/ark-wallet).

Not affiliated with ARK.
