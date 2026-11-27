# ark-tbw

All-in-one **true block weight** reward sharing for ARK Core v3 validators: it follows the chain,
splits every block you forge between your voters by their weight at that block, and pays them.
One binary, one SQLite file, run by systemd. No separate calculation and payment scripts, no
pm2, no PostgreSQL access needed.

- **Fast:** reads only your forged blocks and the chain's transactions (a month of catch-up is a
  few hundred API requests), instead of scanning every block for every voter.
- **Exact:** integer math; every block splits to exactly reward + fee. Checked against a real
  core3-tbw payout cycle on mainnet: same 106 recipients, every voter within 0.00002 ARK.
- **Safe payouts:** a set of payments is bound to one wallet nonce when it is signed, so it can
  never be paid twice, even across crashes, restarts or network errors.
- **Live changes:** change the share from any block on, pay now, cancel a stuck payout, reload
  the config, all while it runs.

How it works and the edge cases it handles: [DESIGN.md](DESIGN.md).

Runs on Ubuntu 22.04, 24.04 and 26.04 (x86_64 and arm64). The binary is static: no runtime
dependencies.

## Install with the install script

This installs the ARK Core v3 version (branch `main`). For Mainsail, use the script on the
[`mainsail` branch](https://github.com/geopsllc/ark-tbw/tree/mainsail#install-with-the-install-script).

On the server, as the account that should run ark-tbw (not root; it asks for sudo when needed):

```bash
curl -fsSL https://raw.githubusercontent.com/geopsllc/ark-tbw/main/install.sh | bash
```

It asks for your validator, the node API, the voter share, the keep address, the memo and the passphrase, then:

| | |
|---|---|
| binary | `/usr/local/bin/ark-tbw` (a release download, checked against its SHA256SUMS; built from source with Rust when no release is published) |
| config | `~/.ark-tbw/config.toml`, payouts in **shadow mode** (`payout.enabled = false`) |
| keep address | `share.keep_address` (Enter: the keep stays in the validator wallet) |
| passphrase | the validator **wallet** passphrase, in `~/.ark-tbw/secret.toml`, chmod 600 (Enter skips it; add it before you go live) |
| database | `~/.ark-tbw/ark-tbw.db`, initialised at the current tip |
| service | the systemd system service, enabled at boot and started |

It then runs `ark-tbw check` and shows `ark-tbw status`. Watch the shadow payouts
(`ark-tbw payouts`); to go live, fund the validator wallet, set `enabled = true` under
`[payout]`, run `ark-tbw check`, then `ark-tbw reload`.

**Upgrade:** `ark-tbw upgrade` (as the same account, not root). It looks up the newest release
of this branch, and when it is newer runs this branch's installer, which downloads and verifies
the binary, keeps the config, secret and database, and restarts the service on the new binary.
`ark-tbw upgrade --check` only shows the installed and the latest version; `--version X` installs
a specific release, `--force` reinstalls the current one. Running the install command again does
the same.

Options go after `bash -s --`, for example:

```bash
curl -fsSL https://raw.githubusercontent.com/geopsllc/ark-tbw/main/install.sh | bash -s -- --rootless --from-height 37900000
```

| option | |
|---|---|
| `--rootless` | user service and `~/.local/bin`, no sudo at all (see [Rootless user service](#rootless-user-service-no-sudo)) |
| `--from-height N` | start the database right after block N instead of the tip |
| `--from-source` | build with Rust instead of downloading a release |
| `--version TAG` | install a specific release |
| `--dir DIR` | config folder other than `~/.ark-tbw` |
| `--no-service` | install and initialise only |
| `-y` | no questions: defaults, or the values in `TBW_VALIDATOR`, `TBW_API`, `TBW_SHARE`, `TBW_KEEP`, `TBW_MEMO` |

With `--rootless`, install `curl`, `build-essential` and `git` first if they are missing (the
script cannot use sudo then).

## Install by hand

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

The passphrase lives in its own file, readable only by you:

```bash
install -m 600 /dev/null ~/.ark-tbw/secret.toml
nano ~/.ark-tbw/secret.toml     # passphrase = "..."   (and second_passphrase = "..." if set)
```

Initialise: this reads your voters as of the start height (default: now).

```bash
ark-tbw init                         # start now
ark-tbw init --from-height 37900000  # or catch up and pay from a given block
ark-tbw init --import-core3 ~/core3-tbw/core/data/tbw.db   # or continue a core3-tbw install
```

With `--import-core3`, stop core3-tbw first (`pm2 stop tbw pay`). ark-tbw takes over its unpaid
balances and continues right after the last block it processed. Custom rates found in the old
database are listed so you can add them to `[share.custom]`.

Check everything (node, validator, secret, balance) without changing anything:

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
| `ark-tbw upgrade [--check]` | install the newest release, keep config, secret and data |
| `ark-tbw logs` | last 100 log lines |
| `ark-tbw logs -f` | follow the log |
| `ark-tbw logs --since "1 hour ago"` | log since a time |
| `ark-tbw service show [--rootless]` | print the unit file without installing it |
| `ark-tbw service uninstall` | remove the service, with sudo for the system service (config and database stay) |

The service restarts itself on failure. It stays stopped (exit code 2) on errors a restart cannot
fix, such as a bad config or a chain reorganisation deeper than `network.confirmations`, so you
can look first. `ark-tbw logs` reads journald, the same as `journalctl -u ark-tbw` (rootless:
`journalctl --user -u ark-tbw`).

## Day to day

`ark-tbw --help` lists every command with examples; `ark-tbw <command> --help` shows its
options (`install.sh --help` does the same for the installer).

```bash
ark-tbw status              # sync height, share, next payout, unpaid totals
ark-tbw voters              # weight, unpaid and paid per voter (--all: incl. former voters)
ark-tbw blocks              # your last forged blocks and how each was split
ark-tbw payouts             # payout history;  ark-tbw payouts 12  for one payout in detail
ark-tbw pay-now             # pay out now instead of waiting for the next trigger
```

### Changing the share

```bash
ark-tbw share set 60                       # from the next block on
ark-tbw share set 55 --from-height 38000000
ark-tbw share                              # the schedule
```

The keep is always 100% − share. Blocks already processed keep the share they were split with
(shown per block in `ark-tbw blocks`). The running service applies changes without a restart.

### Redirecting a voter's rewards

```toml
[redirect]
"Avoter..." = "Apayto..."
```

The voter's rewards are still computed and tracked on its own address; only the payment goes to
the other address. Run `ark-tbw reload` afterwards.

### Stuck payouts

A payout waits for each of its transactions to be confirmed. One that drops out of the pool is
broadcast again; a fee rejection re-signs it with the same nonce and a higher fee. If a
transaction still never confirms (`ark-tbw payouts ID`), cancel it:

```bash
ark-tbw cancel-payout ID
```

This takes the transaction's nonce with a 1-arktoshi transfer to your own wallet. Whatever takes
the nonce first wins: if the payment does, it is paid; if the cancel does, the amounts go back to
the voters' unpaid balances for the next payout. Nothing can be paid twice.

## Configuration

See [config.example.toml](config.example.toml); every option is documented there. The main ones:

| option | |
|---|---|
| `network.api` | your relay (`http://127.0.0.1:4003/api`, fastest) or `https://api.ark.io/api` with `requests_per_minute = 170` |
| `share.percent` | share for voters; the keep is the rest |
| `share.keep_address` | where the keep goes (default: stays in the validator wallet) |
| `share.min_vote`, `share.max_vote` | minimum weight to be paid, and the cap on counted weight |
| `share.fees` | share transaction fees too (default: they go to the keep) |
| `share.anti_dilution` | count unpaid rewards as weight (core3-tbw always did) |
| `share.blacklist`, `share.whitelist`, `[share.custom]` | exclude, restrict, or set per-voter rates |
| `payout.every_blocks`, `payout.at` | pay every N of your own blocks and/or at UTC times |
| `payout.min_amount` | smaller balances carry over |
| `payout.enabled` | `false` = shadow mode |

Transaction fees are the network's minimum dynamic fee and are paid from the keep. If the keep is
short, the fees are carried as keep debt and paid off by later blocks.

## Development

```bash
cargo test                                          # unit tests (no network)
cargo test broadcast_unfunded -- --ignored          # posts a transaction from an empty wallet to mainnet; must be rejected
tools/test-ubuntu.sh                                # runs the Linux binary on Ubuntu 22.04 / 24.04 / 26.04 in Docker
tools/release.sh v0.2.0 notes.md --latest           # maintainers: build, tag and publish a release (macOS keychain token)
```

## Credits

The true block weight approach and the core3-tbw feature set this replaces are by
[galperins4](https://github.com/galperins4/core3-tbw). ark-tbw is a new implementation and shares
no code with it. Transaction signing comes from [ARK Wallet](https://github.com/geopsllc/ark-wallet).

Not affiliated with ARK.
