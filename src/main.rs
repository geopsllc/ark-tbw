mod alloc;
mod api;
mod chain;
mod config;
mod crypto;
mod db;
mod evm;
mod log;
mod payout;
mod service;
mod sync;
mod upgrade;

use anyhow::{anyhow, bail, Context, Result};
use api::{Api, NodeInfo};
use clap::{Parser, Subcommand};
use config::{fmt_ark, fmt_pct, sym, Config, BPS};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sync::{Engine, Me};

const AFTER_HELP: &str = "\
Getting started:
  ark-tbw init                       read your voters as of now (or --from-height N)
  ark-tbw check                      check config, node and secret; changes nothing
  sudo ark-tbw service install       install the systemd service (or: service install --rootless)
  ark-tbw start                      start it;  ark-tbw logs -f  to follow it

Day to day:
  ark-tbw status                     sync height, share, next payout, unpaid totals
  ark-tbw voters [--all]             weight, unpaid and paid per voter
  ark-tbw blocks [-n 50]             your last blocks and how each was split
  ark-tbw payouts [ID]               payout history, or one payout in detail
  ark-tbw share set 60 [--from-height 24700000]
                                     change the voter share (the keep is 100 - share)
  ark-tbw pay-now                    pay out now
  ark-tbw cancel-payout ID           cancel a stuck payout (nothing can be paid twice)
  ark-tbw payees                     recipients the multipayment cannot pay
  ark-tbw unblock 0xADDRESS          pay one through the multipayment again
  ark-tbw reload                     re-read the config after editing it
  ark-tbw upgrade [--check]          install the newest release (keeps config, secret, data)

The config is ~/.ark-tbw/config.toml unless -c/--config or ARK_TBW_CONFIG says otherwise.
Every option is explained in config.example.toml. More: ark-tbw <command> --help";

#[derive(Parser)]
#[command(name = "ark-tbw", version, after_help = AFTER_HELP, about = "True block weight reward sharing for ARK Mainsail validators")]
struct Cli {
    /// Config file [default: ~/.ark-tbw/config.toml]
    #[arg(short, long, global = true, env = "ARK_TBW_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database and read the voters as of the start height
    Init {
        /// Start right after this block (default: validator.start_height, else the confirmed tip)
        #[arg(long)]
        from_height: Option<u64>,
    },
    /// Run the service in the foreground (systemd runs this)
    Run,
    /// Check the config, the node and the secret without changing anything
    Check,
    /// Sync, payout and service state
    Status,
    /// Voters with their weight, unpaid and paid rewards
    Voters {
        /// Include wallets that no longer vote
        #[arg(long)]
        all: bool,
    },
    /// Recent payouts, or one payout in detail
    Payouts {
        /// Show this payout in detail: its transactions and every payment
        id: Option<i64>,
        /// How many payouts to list
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// The validator's processed blocks and how each was split
    Blocks {
        /// How many blocks to list
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Voter share: show the schedule, or change it
    Share {
        #[command(subcommand)]
        action: Option<ShareCmd>,
    },
    /// Ask the running service to pay out now
    PayNow,
    /// Cancel a stuck payout (consumes its nonces; nothing can be paid twice)
    CancelPayout {
        /// Payout id, from  ark-tbw payouts
        id: i64,
    },
    /// Recipients that cannot be paid through the multipayment contract, or not at all
    Payees,
    /// Try paying a blocked or direct-only recipient through the multipayment again
    Unblock {
        /// The recipient's 0x address, from  ark-tbw payees
        address: String,
    },
    /// Show the service log (journald)
    Logs {
        /// Follow new lines
        #[arg(short, long)]
        follow: bool,
        /// How many lines to show
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
        /// e.g. "1 hour ago", "today", "2026-11-26 10:00"
        #[arg(long)]
        since: Option<String>,
    },
    /// Install / remove the systemd service (sudo; or --rootless for a user service)
    Service {
        #[command(subcommand)]
        action: service::ServiceCmd,
    },
    /// systemctl start ark-tbw
    Start,
    /// systemctl stop ark-tbw (finishes the current step first)
    Stop,
    /// systemctl restart ark-tbw
    Restart,
    /// Re-read the config without a restart (systemctl reload ark-tbw)
    Reload,
    /// Install the newest release of this network's branch and restart the service.
    /// Config, secret and database are kept. Runs the branch's install.sh.
    Upgrade {
        /// Only show the installed and the latest version
        #[arg(long)]
        check: bool,
        /// Install this release instead of the latest (e.g. 0.2.0 or the full tag)
        #[arg(long)]
        version: Option<String>,
        /// Build from source with Rust instead of downloading the release binary
        #[arg(long)]
        from_source: bool,
        /// Reinstall even when the installed version is the latest
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ShareCmd {
    /// Change the voter share from a block on (default: the next block to be processed)
    Set {
        /// New voter share in percent, e.g. 60 or 85.5
        percent: String,
        /// First block split with the new share (default: the next block to be processed)
        #[arg(long)]
        from_height: Option<u64>,
    },
}

fn main() {
    // behave like a normal CLI when piped into head / less
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    if let Err(e) = real_main(cli) {
        error!("{e:#}");
        let code = if e.downcast_ref::<Fatal>().is_some() { 2 } else { 1 };
        std::process::exit(code);
    }
}

/// An error that a restart cannot fix (bad config, deep reorg). systemd does not restart on it.
#[derive(Debug)]
struct Fatal(String);
impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Fatal {}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(config::default_config_path)
}

fn load_config(cli: &Cli) -> Result<Config> {
    Config::load(&config_path(cli)).map_err(|e| anyhow!(Fatal(format!("{e:#}"))))
}

fn real_main(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Logs { follow, lines, since } => return service::logs(*follow, *lines, since.as_deref()),
        Cmd::Service { action } => return service::run(action, &config_path(&cli)),
        Cmd::Start => return service::systemctl("start"),
        Cmd::Stop => return service::systemctl("stop"),
        Cmd::Restart => return service::systemctl("restart"),
        Cmd::Reload => return service::systemctl("reload"),
        Cmd::Upgrade { check, version, from_source, force } => {
            return upgrade::run(&config_path(&cli), *check, version.as_deref(), *from_source, *force)
        }
        _ => {}
    }
    let cfg = load_config(&cli)?;
    match &cli.cmd {
        Cmd::Init { from_height } => cmd_init(&cfg, *from_height),
        Cmd::Run => cmd_run(&cli, cfg),
        Cmd::Check => cmd_check(&cfg),
        Cmd::Status => cmd_status(&cfg),
        Cmd::Voters { all } => cmd_voters(&cfg, *all),
        Cmd::Payouts { id, limit } => cmd_payouts(&cfg, *id, *limit),
        Cmd::Blocks { limit } => cmd_blocks(&cfg, *limit),
        Cmd::Share { action } => cmd_share(&cfg, action.as_ref()),
        Cmd::PayNow => queue(&cfg, "pay-now", None),
        Cmd::CancelPayout { id } => queue(&cfg, "cancel-payout", Some(id.to_string())),
        Cmd::Payees => cmd_payees(&cfg),
        Cmd::Unblock { address } => cmd_unblock(&cfg, address),
        _ => unreachable!(),
    }
}

fn open_db(cfg: &Config) -> Result<Connection> {
    let p = cfg.db_path();
    if !p.exists() {
        bail!("no database at {}: run  ark-tbw init", p.display());
    }
    let c = db::open(&p)?;
    if let Some(id) = db::get_u64(&c, "chain_id")? {
        config::set_chain(id);
    }
    Ok(c)
}

fn api_for(cfg: &Config) -> Api {
    Api::new(&cfg.api, &cfg.rpc, &cfg.tx_api, cfg.requests_per_minute, cfg.rpc_requests_per_minute)
}

fn signer_for(cfg: &Config, me: &Me, api: &mut Api) -> Result<payout::Signer> {
    let secret = config::load_secret(cfg)?;
    let keys = crypto::KeyPair::from_passphrase(secret.passphrase.trim()).map_err(|e| anyhow!(e))?;
    if keys.public_hex() != me.public_key.trim_start_matches("0x").to_lowercase() || evm::address_from_public_key(&keys.public).ok().as_deref() != Some(me.address.as_str()) {
        bail!(Fatal("the passphrase in the secret file does not belong to the validator wallet".into()));
    }
    let w = api.wallet(&me.address)?.ok_or_else(|| anyhow!("validator wallet not found"))?;
    let second = match (&w.second_public_key, &secret.second_passphrase) {
        (Some(spk), Some(p)) => {
            let k = crypto::KeyPair::from_passphrase(p.trim()).map_err(|e| anyhow!(e))?;
            if k.public_hex() != spk.trim_start_matches("0x").to_lowercase() {
                bail!(Fatal("second_passphrase in the secret file is not the wallet's second passphrase".into()));
            }
            Some(k)
        }
        (Some(_), None) => bail!(Fatal("the validator wallet has a second passphrase: add second_passphrase to the secret file".into())),
        (None, Some(_)) => bail!(Fatal("the secret file has a second_passphrase but the wallet has none".into())),
        (None, None) => None,
    };
    if crypto::looks_like_secret(&cfg.memo, &[&keys.public]) || cfg.keep.iter().any(|k| crypto::looks_like_secret(&k.memo, &[&keys.public])) {
        bail!(Fatal("a payout memo looks like a passphrase; memos are public".into()));
    }
    payout::Signer::new(keys, second, me.chain_id)
}

fn cmd_init(cfg: &Config, from: Option<u64>) -> Result<()> {
    let _lock = db::lock(&cfg.data_dir)?;
    let mut conn = db::open(&cfg.db_path())?;
    let mut api = api_for(cfg);
    config::set_chain(api.chain_id()?);
    let h0 = sync::initialize(cfg, &mut api, &mut conn, from)?;
    info!("initialized at height {h0} with {} API and {} RPC requests. Next: ark-tbw check, then start the service", api.requests, api.rpc_requests);
    Ok(())
}

fn cmd_check(cfg: &Config) -> Result<()> {
    let mut api = api_for(cfg);
    let node = api.node_info()?;
    config::set_chain(node.chain_id);
    let (tip, _) = api.tip()?;
    println!("config      {} OK", cfg.path.display());
    println!("node        {} at height {tip} (chain id {}, {})", cfg.api, node.chain_id, sym());
    let d = api.validator(&cfg.validator)?.ok_or_else(|| anyhow!("validator {} not found", cfg.validator))?;
    println!("validator   {} {}{}", d.username.as_deref().unwrap_or("(no username)"), d.address, if d.resigned { "  RESIGNED" } else { "" });
    match api.balances_at(std::slice::from_ref(&d.address), tip.saturating_sub(cfg.confirmations)) {
        Ok(Some(_)) => println!("rpc         {} OK (account history of the last {} blocks)", cfg.rpc, api::HISTORY_BLOCKS),
        Ok(None) => println!("rpc         {} answers, but without account history at height {}", cfg.rpc, tip.saturating_sub(cfg.confirmations)),
        Err(e) => println!("rpc         {} UNREACHABLE: {e:#}", cfg.rpc),
    }
    let gas = payout::Gas::new(cfg, &node);
    println!("gas         price {} gwei (network minimum {}), up to {} gas per transaction", gas.price as f64 / 1e9, node.min_gas_price as f64 / 1e9, gas.max);
    println!(
        "split       share {} to voters, keep {} to {}",
        fmt_pct(cfg.voter_bps),
        fmt_pct(cfg.keep[0].bps),
        if cfg.keep[0].address.is_empty() { "the validator wallet" } else { cfg.keep[0].address.as_str() }
    );
    for b in std::iter::once(&cfg.tx_api).chain(&cfg.broadcast) {
        let mut a = Api::new(b, &cfg.rpc, b, 0, 0);
        match a.get_opt(&format!("{b}/transactions/unconfirmed?limit=1")) {
            Ok(Some(_)) => println!("tx pool     {b} OK"),
            Ok(None) => println!("tx pool     {b} NOT FOUND"),
            Err(e) => println!("tx pool     {b} UNREACHABLE: {e:#}"),
        }
    }
    if cfg.payout_enabled {
        let me = Me {
            username: d.username.clone().unwrap_or_default(),
            address: d.address.clone(),
            public_key: d.public_key.clone().unwrap_or_default(),
            chain_id: node.chain_id,
        };
        let s = signer_for(cfg, &me, &mut api)?;
        println!("secret      OK (signs as {}{})", s.address, if s.second.is_some() { ", with second passphrase" } else { "" });
        println!("balance     {} (next nonce {})", sync::ark(d.balance), d.nonce);
    } else {
        println!("payouts     SHADOW mode: payouts are computed and recorded, nothing is sent");
    }
    if cfg.db_path().exists() {
        let c = db::open(&cfg.db_path())?;
        if let Some(h) = db::get_u64(&c, "last_height")? {
            println!("database    {} processed to {h} ({} behind)", cfg.db_path().display(), tip.saturating_sub(h));
        }
    } else {
        println!("database    not created yet: run  ark-tbw init");
    }
    Ok(())
}

fn queue(cfg: &Config, cmd: &str, arg: Option<String>) -> Result<()> {
    let conn = open_db(cfg)?;
    conn.execute("INSERT INTO commands(cmd,arg,created_at) VALUES(?1,?2,?3)", params![cmd, arg, db::now()])?;
    if db::is_locked(&cfg.data_dir) {
        println!("queued; the running service picks it up within {} s (watch: ark-tbw logs -f)", cfg.poll_seconds);
    } else {
        println!("queued, but the service is not running: it runs when the service starts");
    }
    Ok(())
}

fn cmd_share(cfg: &Config, action: Option<&ShareCmd>) -> Result<()> {
    let conn = open_db(cfg)?;
    let last = db::get_u64(&conn, "last_height")?.unwrap_or(0);
    match action {
        None => {
            let start = db::get_u64(&conn, "start_height")?.unwrap_or(0);
            println!("from height {start:>10}  share {:>7}  keep {:>7}  (config)", fmt_pct(cfg.voter_bps), fmt_pct(BPS - cfg.voter_bps));
            for (h, bps) in db::share_changes(&conn)? {
                println!("from height {h:>10}  share {:>7}  keep {:>7}", fmt_pct(bps), fmt_pct(BPS - bps));
            }
            println!("processed up to {last}");
        }
        Some(ShareCmd::Set { percent, from_height }) => {
            let bps = config::parse_percent(&toml::Value::String(percent.clone()))?;
            cfg.with_voter_bps(bps)?;
            let h = from_height.unwrap_or(last + 1);
            if h <= last {
                bail!("blocks up to {last} are already split; choose a height above {last}");
            }
            conn.execute("INSERT OR REPLACE INTO share_changes(height,voter_bps,created_at) VALUES(?1,?2,?3)", params![h as i64, bps as i64, db::now()])?;
            println!("share {} / keep {} from block {h} on. The running service applies it without a restart.", fmt_pct(bps), fmt_pct(BPS - bps));
            println!("(share.percent in the config, {}, is only the starting value.)", fmt_pct(cfg.voter_bps));
        }
    }
    Ok(())
}

fn cmd_status(cfg: &Config) -> Result<()> {
    let conn = open_db(cfg)?;
    let me = Me::load(&conn)?;
    let last = db::get_u64(&conn, "last_height")?.unwrap_or(0);
    let start = db::get_u64(&conn, "start_height")?.unwrap_or(0);
    let lp = db::get_u64(&conn, "last_payout_height")?.unwrap_or(0);
    let own: i64 = conn.query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get(0))?;
    let own_since: i64 = conn.query_row("SELECT COUNT(*) FROM blocks WHERE height > ?1", params![lp as i64], |r| r.get(0))?;
    let mut api = api_for(cfg);
    let tip = api.tip().map(|t| t.0).ok();
    println!("validator    {} {}", me.username, me.address);
    println!("service      {}", if db::is_locked(&cfg.data_dir) { "running" } else { "not running" });
    println!("mode         {}", if cfg.payout_enabled { "live payouts" } else { "SHADOW (nothing is sent)" });
    match tip {
        Some(t) => println!("processed    {last} (tip {t}, {} behind; {} kept back for confirmations)", t.saturating_sub(last), cfg.confirmations),
        None => println!("processed    {last} (node unreachable)"),
    }
    println!("since        height {start}: {own} own blocks split");
    let sched = db::share_changes(&conn)?;
    let bps = sched.iter().rev().find(|(h, _)| *h <= last + 1).map(|x| x.1).unwrap_or(cfg.voter_bps);
    println!("share / keep {} / {}", fmt_pct(bps), fmt_pct(BPS - bps));
    if cfg.every_blocks > 0 {
        println!("next payout  after {} more own blocks ({own_since}/{})", (cfg.every_blocks as i64 - own_since).max(0), cfg.every_blocks);
    }
    if !cfg.at.is_empty() {
        println!("payout times {} UTC", cfg.at.iter().map(|(h, m)| format!("{h:02}:{m:02}")).collect::<Vec<_>>().join(", "));
    }
    let (vu, ku): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN kind='voter' THEN unpaid END),0), COALESCE(SUM(CASE WHEN kind='keep' THEN unpaid END),0) FROM accounts",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    println!("unpaid       voters {} {s}, keep {} {s}", fmt_ark(vu as i128), fmt_ark(ku as i128), s = sym());
    let voting: i64 = conn.query_row("SELECT COUNT(*) FROM wallets WHERE voting=1", [], |r| r.get(0))?;
    println!("voters       {voting}");
    let blocked: i64 = conn.query_row("SELECT COUNT(*) FROM payees WHERE mode='blocked'", [], |r| r.get(0))?;
    if blocked > 0 {
        println!("BLOCKED      {blocked} recipient(s) cannot be paid: see  ark-tbw payees");
    }
    if let Some(id) = payout::pending_payout(&conn)? {
        println!("IN PROGRESS  payout #{id}: see  ark-tbw payouts {id}");
    }
    let lt = db::get_u64(&conn, "last_payout_time")?.unwrap_or(0);
    println!("last payout  height {lp}, {}", chrono::DateTime::from_timestamp(lt as i64, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default());
    Ok(())
}

fn cmd_voters(cfg: &Config, all: bool) -> Result<()> {
    let conn = open_db(cfg)?;
    let mut st = conn.prepare(
        "SELECT w.address, w.balance, w.voting, COALESCE(a.unpaid,0), COALESCE(a.paid,0)
         FROM wallets w LEFT JOIN accounts a ON a.address = w.address WHERE ?1 OR w.voting = 1",
    )?;
    let mut rows: Vec<(String, u64, i64, i64, i64)> = st
        .query_map(params![all], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))?
        .map(|r| r.map(|(a, b, v, u, p)| (a, sync::weight(b.parse().unwrap_or(0)), v, u, p)))
        .collect::<rusqlite::Result<_>>()?;
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let payees = db::payees(&conn)?;
    let s = sym();
    println!("{:<42} {:>18} {:>14} {:>14}  note", "address", format!("weight {s}"), format!("unpaid {s}"), format!("paid {s}"));
    for (a, w, v, u, p) in rows {
        let mut note = Vec::new();
        if v == 0 {
            note.push("not voting".to_string());
        }
        if cfg.blacklist.contains(&a) {
            note.push("blacklisted".into());
        }
        if !cfg.whitelist.is_empty() && !cfg.whitelist.contains(&a) {
            note.push("not whitelisted".into());
        }
        if w < cfg.min_vote {
            note.push("below minimum".into());
        }
        if cfg.max_vote > 0 && w > cfg.max_vote {
            note.push("capped".into());
        }
        if let Some(c) = cfg.custom.get(&a) {
            note.push(format!("rate {}", fmt_pct(*c)));
        }
        if let Some(t) = cfg.redirect.get(&a) {
            note.push(format!("paid to {t}"));
        }
        match payees.get(cfg.payee(&a)).map(String::as_str) {
            Some("blocked") => note.push("BLOCKED: payments fail, set a redirect".into()),
            Some("direct") => note.push("paid by own transfer".into()),
            _ => {}
        }
        println!("{:<42} {:>18} {:>14} {:>14}  {}", a, fmt_ark(w as i128), fmt_ark(u as i128), fmt_ark(p as i128), note.join(", "));
    }
    Ok(())
}

fn cmd_payees(cfg: &Config) -> Result<()> {
    let conn = open_db(cfg)?;
    let mut st = conn.prepare("SELECT address, mode, reason, at FROM payees ORDER BY mode, address")?;
    let rows: Vec<(String, String, String, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        println!("every recipient so far could be paid through the multipayment contract");
        return Ok(());
    }
    for (a, mode, reason, at) in rows {
        let t = chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
        let what = if mode == "blocked" { "BLOCKED (skipped; set a [redirect] for the voter)" } else { "paid by its own transfer" };
        println!("{a}  {what}  since {t}: {reason}");
    }
    println!("ark-tbw unblock <address> tries the multipayment for it again");
    Ok(())
}

fn cmd_unblock(cfg: &Config, address: &str) -> Result<()> {
    let conn = open_db(cfg)?;
    let a = evm::norm(address).ok_or_else(|| anyhow!("{address:?} is not a 0x address"))?;
    let n = conn.execute("DELETE FROM payees WHERE address=?1", params![a])?;
    if n == 0 {
        println!("{a} was not blocked");
    } else {
        println!("{a} goes back to normal payments from the next payout");
    }
    Ok(())
}

fn cmd_blocks(cfg: &Config, limit: usize) -> Result<()> {
    let conn = open_db(cfg)?;
    let mut st = conn.prepare("SELECT height, ts, reward + fee, voter_bps, voters, to_voters, to_keep FROM blocks ORDER BY height DESC LIMIT ?1")?;
    let rows = st.query_map(params![limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?)))?;
    println!("{:>10}  {:<20} {:>12} {:>7} {:>6} {:>14} {:>14}", "height", "time (UTC)", "reward", "share", "voters", "shared", "kept");
    for r in rows {
        let (h, ts, total, bps, n, v, res) = r?;
        let t = chrono::DateTime::from_timestamp_millis(ts).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
        println!("{h:>10}  {t:<20} {:>12} {:>7} {n:>6} {:>14} {:>14}", fmt_ark(total as i128), fmt_pct(bps as u64), fmt_ark(v as i128), fmt_ark(res as i128));
    }
    Ok(())
}

fn cmd_payouts(cfg: &Config, id: Option<i64>, limit: usize) -> Result<()> {
    let conn = open_db(cfg)?;
    match id {
        None => {
            let mut st = conn.prepare(
                "SELECT p.id, p.created_at, p.height, p.mode, p.status, p.reason, COUNT(i.account), COALESCE(SUM(i.amount),0)
                 FROM payouts p LEFT JOIN payout_items i ON i.payout_id = p.id GROUP BY p.id ORDER BY p.id DESC LIMIT ?1",
            )?;
            let rows = st.query_map(params![limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, i64>(6)?, r.get::<_, i64>(7)?))
            })?;
            println!("{:>5}  {:<17} {:>10} {:<7} {:<8} {:>6} {:>14}  reason", "id", "created (UTC)", "height", "mode", "status", "items", sym());
            for r in rows {
                let (id, at, h, mode, status, reason, n, sum) = r?;
                let t = chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                println!("{id:>5}  {t:<17} {h:>10} {mode:<7} {status:<8} {n:>6} {:>14}  {reason}", fmt_ark(sum as i128));
            }
        }
        Some(id) => {
            let mut st = conn.prepare("SELECT nonce, fee, status, COALESCE(tx_id,''), attempts, COALESCE(last_error,'') FROM slots WHERE payout_id=?1 ORDER BY nonce")?;
            let rows = st.query_map(params![id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?)))?;
            println!("transactions of payout #{id}:");
            for r in rows {
                let (n, fee, status, txid, att, err) = r?;
                let txid = if txid.is_empty() { txid } else { format!("0x{txid}") };
                println!("  nonce {n}  fee {} {}  {status}  {txid}  broadcasts {att}  {err}", fmt_ark(fee as i128), sym());
            }
            let mut st = conn.prepare("SELECT account, recipient, amount, COALESCE(slot,-1), status FROM payout_items WHERE payout_id=?1 ORDER BY amount DESC")?;
            let rows = st.query_map(params![id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)))?;
            println!("payments:");
            for r in rows {
                let (a, to, amt, slot, status) = r?;
                let to = if to == a { String::new() } else { format!(" -> {to}") };
                let slot = if slot < 0 { "no tx".into() } else { format!("nonce {slot}") };
                println!("  {a}{to}  {} {}  {slot}  {status}", fmt_ark(amt as i128), sym());
            }
        }
    }
    Ok(())
}

/// When the next time-based payout is due, given the last one.
fn time_due(cfg: &Config, last_payout: i64, now: i64) -> bool {
    if cfg.at.is_empty() {
        return false;
    }
    let Some(last) = chrono::DateTime::from_timestamp(last_payout, 0) else { return false };
    let day = last.date_naive();
    for d in 0..3 {
        let date = day + chrono::Days::new(d);
        for (h, m) in &cfg.at {
            let t = date.and_hms_opt(*h, *m, 0).unwrap().and_utc().timestamp();
            if t > last_payout && t <= now {
                return true;
            }
        }
    }
    false
}

fn cmd_run(cli: &Cli, mut cfg: Config) -> Result<()> {
    let _lock = db::lock(&cfg.data_dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    let hup = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGHUP, hup.clone())?;

    let mut conn = open_db(&cfg)?;
    let me = Me::load(&conn)?;
    cfg.bind_validator(&me.address);
    let mut api = api_for(&cfg);
    let mut node = api.node_info()?;
    if node.chain_id != me.chain_id || db::get(&conn, "nethash")?.as_deref() != Some(node.nethash.as_str()) {
        bail!(Fatal(format!("{} is on a different network than this database (chain id {}, database {})", cfg.api, node.chain_id, me.chain_id)));
    }
    let mut signer = if cfg.payout_enabled { Some(signer_for(&cfg, &me, &mut api)?) } else { None };
    info!(
        "ark-tbw {} started for {} ({}), {} mode, node {}",
        env!("CARGO_PKG_VERSION"),
        me.username,
        me.address,
        if cfg.payout_enabled { "live payout" } else { "SHADOW" },
        cfg.api
    );
    let mut last_reconcile: Option<Instant> = None;
    let mut last_node_refresh = Instant::now();

    'main: while !stop.load(Ordering::Relaxed) {
        if hup.swap(false, Ordering::Relaxed) {
            match Config::load(&config_path(cli)).and_then(|mut c| {
                c.bind_validator(&me.address);
                if c.api != cfg.api || c.rpc != cfg.rpc || c.tx_api != cfg.tx_api || c.validator != cfg.validator || c.data_dir != cfg.data_dir {
                    bail!("network.api, network.rpc, network.tx_api, validator.name and storage.data_dir need a restart");
                }
                Ok(c)
            }) {
                Ok(c) => {
                    let s = if c.payout_enabled { Some(signer_for(&c, &me, &mut api)?) } else { None };
                    cfg = c;
                    signer = s;
                    info!("config reloaded ({} mode); voter share changes go through  ark-tbw share set", if cfg.payout_enabled { "live" } else { "shadow" });
                }
                Err(e) => error!("config NOT reloaded, keeping the old one: {e:#}"),
            }
        }
        if last_node_refresh.elapsed() > Duration::from_secs(3600) {
            if let Ok(n) = api.node_info() {
                node = n;
            }
            last_node_refresh = Instant::now();
        }
        let step = (|| -> Result<()> {
            // commands from the CLI
            let cmds: Vec<(i64, String, Option<String>)> = {
                let mut st = conn.prepare("SELECT id, cmd, arg FROM commands WHERE done_at IS NULL ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut pay_now = false;
            for (id, cmd, arg) in cmds {
                let result = match cmd.as_str() {
                    "pay-now" => {
                        pay_now = true;
                        "ok".to_string()
                    }
                    "cancel-payout" => match &signer {
                        Some(s) => {
                            let pid: i64 = arg.as_deref().unwrap_or("0").parse().unwrap_or(0);
                            match payout::cancel(&cfg, &node, &mut api, &mut conn, &me, s, pid) {
                                Ok(n) => {
                                    info!("payout #{pid}: sent {n} cancelling transaction(s)");
                                    "ok".into()
                                }
                                Err(e) => {
                                    error!("cancel payout #{pid}: {e:#}");
                                    format!("{e:#}")
                                }
                            }
                        }
                        None => "shadow mode has nothing to cancel".into(),
                    },
                    _ => "unknown command".into(),
                };
                conn.execute("UPDATE commands SET done_at=?1, result=?2 WHERE id=?3", params![db::now(), result, id])?;
            }

            // settle transactions in flight
            if cfg.payout_enabled {
                payout::resolve(&cfg, &mut api, &mut conn, &me, signer.as_ref())?;
            }

            // follow the chain
            let (tip, _) = api.tip()?;
            let target = tip.saturating_sub(cfg.confirmations);
            loop {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let behind = target.saturating_sub(db::get_u64(&conn, "last_height")?.unwrap_or(0));
                let res = {
                    let mut eng = Engine::load(&cfg, &mut api, &mut conn)?;
                    eng.sync_chunk(target)?
                };
                let Some(r) = res else { break };
                if behind > 200 {
                    info!(
                        "caught up to {} ({} own blocks in this chunk, {} checked against the node, {} to go)",
                        r.height,
                        r.own_blocks,
                        r.verified,
                        target.saturating_sub(r.height)
                    );
                }
                if r.corrected > 0 {
                    warn!("{} voter balance(s) corrected from the node's account history", r.corrected);
                }
                if r.payout_due {
                    do_payout(&cfg, &node, &me, signer.as_ref(), &mut api, &mut conn, r.height, "block count")?;
                }
            }
            let last = db::get_u64(&conn, "last_height")?.unwrap_or(0);
            let lpt = db::get_u64(&conn, "last_payout_time")?.unwrap_or(0) as i64;
            if pay_now {
                do_payout(&cfg, &node, &me, signer.as_ref(), &mut api, &mut conn, last, "manual")?;
            } else if time_due(&cfg, lpt, db::now()) {
                do_payout(&cfg, &node, &me, signer.as_ref(), &mut api, &mut conn, last, "schedule")?;
            }
            let caught_up = last + cfg.confirmations + 5 >= tip;
            if caught_up && last_reconcile.map_or(true, |t| t.elapsed() > Duration::from_secs(3600)) {
                let mut eng = Engine::load(&cfg, &mut api, &mut conn)?;
                match eng.reconcile()? {
                    Some(0) => info!("voter check: all {} voters and balances match the node", eng.wallets.values().filter(|w| w.voting).count()),
                    Some(n) => warn!("voter check: repaired {n} wallet(s)"),
                    None => {}
                }
                last_reconcile = Some(Instant::now());
            }
            Ok(())
        })();
        if let Err(e) = step {
            if e.to_string().contains("reorganised") {
                return Err(anyhow!(Fatal(format!("{e:#}"))));
            }
            error!("{e:#}");
        }
        for _ in 0..cfg.poll_seconds {
            if stop.load(Ordering::Relaxed) || hup.load(Ordering::Relaxed) {
                continue 'main;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    info!("stopped cleanly");
    Ok(())
}

fn do_payout(cfg: &Config, node: &NodeInfo, me: &Me, signer: Option<&payout::Signer>, api: &mut Api, conn: &mut Connection, height: u64, reason: &str) -> Result<()> {
    match signer {
        Some(s) if cfg.payout_enabled => payout::run_live(cfg, node, me, s, api, conn, height, reason).context("payout"),
        _ => payout::run_shadow(cfg, node, me, conn, height, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_times() {
        let t = "[network]\napi=\"https://testnet.mainsailhq.com/api\"\n[validator]\nname=\"d\"\n[share]\npercent=50\n[payout]\nat=[\"00:00\",\"12:00\"]\n";
        let c = Config::parse(t, std::path::Path::new("/tmp/c.toml")).unwrap();
        let base = chrono::NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let ts = |h: u32, m: u32| base.and_hms_opt(h, m, 0).unwrap().and_utc().timestamp();
        assert!(!time_due(&c, ts(0, 0), ts(11, 59)));
        assert!(time_due(&c, ts(0, 0), ts(12, 0)));
        assert!(time_due(&c, ts(12, 0), ts(23, 59) + 61));
        assert!(!time_due(&c, ts(12, 0), ts(23, 59)));
        // down for two days: one payout, not one per missed slot
        assert!(time_due(&c, ts(12, 0) - 2 * 86400, ts(13, 0)));
    }
}
