//! SQLite storage. One file, WAL mode, every state change in a transaction: a crash or a
//! stop at any point leaves the database at the last fully processed chunk.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- every wallet that has voted for the validator since the start height, with its state at last_height
CREATE TABLE IF NOT EXISTS wallets (
  address TEXT PRIMARY KEY,
  public_key TEXT,
  balance INTEGER NOT NULL,
  locked INTEGER NOT NULL DEFAULT 0,
  voting INTEGER NOT NULL,
  validator INTEGER NOT NULL DEFAULT 0,   -- forges blocks: its rewards change its balance
  first_seen INTEGER NOT NULL
);

-- voter share changes made with  ark-tbw share set : from `height` on, blocks use voter_bps
CREATE TABLE IF NOT EXISTS share_changes (
  height INTEGER PRIMARY KEY,
  voter_bps INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

-- what each voter / keep has earned (unpaid may go negative for the keep: fee debt)
CREATE TABLE IF NOT EXISTS accounts (
  address TEXT PRIMARY KEY,
  kind TEXT NOT NULL,           -- voter | keep
  unpaid INTEGER NOT NULL DEFAULT 0,
  paid INTEGER NOT NULL DEFAULT 0
);

-- the validator's own forged blocks and how each was split
CREATE TABLE IF NOT EXISTS blocks (
  height INTEGER PRIMARY KEY,
  id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  reward INTEGER NOT NULL,
  fee INTEGER NOT NULL,
  voter_bps INTEGER NOT NULL,   -- voter share used for this block
  voters INTEGER NOT NULL,      -- voters paid for this block
  weight INTEGER NOT NULL,      -- total weight the voter share was divided by
  to_voters INTEGER NOT NULL,
  to_keep INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS allocations (
  height INTEGER NOT NULL,
  address TEXT NOT NULL,
  weight INTEGER NOT NULL,
  amount INTEGER NOT NULL,
  PRIMARY KEY (height, address)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS payouts (
  id INTEGER PRIMARY KEY,
  created_at INTEGER NOT NULL,
  height INTEGER NOT NULL,      -- rewards earned up to and including this height
  mode TEXT NOT NULL,           -- live | shadow
  status TEXT NOT NULL,         -- pending | done
  reason TEXT NOT NULL
);

-- one row per account paid; slot = nonce of the transaction carrying it (NULL = no transaction needed)
CREATE TABLE IF NOT EXISTS payout_items (
  payout_id INTEGER NOT NULL,
  account TEXT NOT NULL,
  recipient TEXT NOT NULL,
  amount INTEGER NOT NULL,
  slot INTEGER,
  status TEXT NOT NULL,         -- pending | paid | failed | shadow
  PRIMARY KEY (payout_id, account)
);

-- a nonce of the payout wallet that carries one set of payments. Its payments are bound to
-- that nonce for good: re-signing only ever reuses the same nonce, so they can be paid once.
CREATE TABLE IF NOT EXISTS slots (
  nonce INTEGER PRIMARY KEY,
  payout_id INTEGER NOT NULL,
  fee INTEGER NOT NULL,         -- fee charged to the keep for this slot
  status TEXT NOT NULL,         -- pending | confirmed | failed
  tx_id TEXT,                   -- the variant that confirmed
  confirmed_height INTEGER,
  last_broadcast INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

-- signed transactions for a slot (the first one, fee bumps, and a cancelling self-transfer)
CREATE TABLE IF NOT EXISTS variants (
  tx_id TEXT PRIMARY KEY,
  nonce INTEGER NOT NULL,
  kind TEXT NOT NULL,           -- pay | cancel
  fee INTEGER NOT NULL,
  json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS commands (
  id INTEGER PRIMARY KEY,
  cmd TEXT NOT NULL,
  arg TEXT,
  created_at INTEGER NOT NULL,
  done_at INTEGER,
  result TEXT
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let c = Connection::open(path).with_context(|| format!("cannot open database {}", path.display()))?;
    c.busy_timeout(std::time::Duration::from_secs(15))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "FULL")?;
    c.execute_batch(SCHEMA)?;
    let v: Option<String> = c.query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0)).optional()?;
    match v {
        None => {
            c.execute("INSERT INTO meta(key,value) VALUES('schema',?1)", params![SCHEMA_VERSION.to_string()])?;
        }
        Some(v) if v.parse::<i64>().unwrap_or(0) > SCHEMA_VERSION => {
            bail!("database {} was written by a newer ark-tbw", path.display())
        }
        _ => {}
    }
    Ok(c)
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn get(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| r.get(0)).optional()?)
}

pub fn get_u64(c: &Connection, key: &str) -> Result<Option<u64>> {
    Ok(get(c, key)?.and_then(|v| v.parse().ok()))
}

pub fn set(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct WalletRow {
    pub public_key: Option<String>,
    pub balance: i64,
    pub locked: i64,
    pub voting: bool,
    pub validator: bool,
    pub first_seen: u64,
}

pub fn load_wallets(c: &Connection) -> Result<HashMap<String, WalletRow>> {
    let mut st = c.prepare("SELECT address, public_key, balance, locked, voting, validator, first_seen FROM wallets")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            WalletRow {
                public_key: r.get(1)?,
                balance: r.get(2)?,
                locked: r.get(3)?,
                voting: r.get::<_, i64>(4)? != 0,
                validator: r.get::<_, i64>(5)? != 0,
                first_seen: r.get::<_, i64>(6)? as u64,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn save_wallet(c: &Connection, address: &str, w: &WalletRow) -> Result<()> {
    c.execute(
        "INSERT INTO wallets(address, public_key, balance, locked, voting, validator, first_seen) VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(address) DO UPDATE SET public_key=excluded.public_key, balance=excluded.balance, locked=excluded.locked, voting=excluded.voting, validator=excluded.validator",
        params![address, w.public_key, w.balance, w.locked, w.voting as i64, w.validator as i64, w.first_seen as i64],
    )?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Account {
    pub kind: String,
    pub unpaid: i64,
    pub paid: i64,
}

pub fn load_accounts(c: &Connection) -> Result<HashMap<String, Account>> {
    let mut st = c.prepare("SELECT address, kind, unpaid, paid FROM accounts")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, Account { kind: r.get(1)?, unpaid: r.get(2)?, paid: r.get(3)? })))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn save_account(c: &Connection, address: &str, a: &Account) -> Result<()> {
    c.execute(
        "INSERT INTO accounts(address, kind, unpaid, paid) VALUES(?1,?2,?3,?4)
         ON CONFLICT(address) DO UPDATE SET kind=excluded.kind, unpaid=excluded.unpaid, paid=excluded.paid",
        params![address, a.kind, a.unpaid, a.paid],
    )?;
    Ok(())
}

/// Add to an account's balances inside the caller's transaction.
pub fn credit(c: &Connection, address: &str, kind: &str, unpaid: i64, paid: i64) -> Result<()> {
    c.execute(
        "INSERT INTO accounts(address, kind, unpaid, paid) VALUES(?1,?2,?3,?4)
         ON CONFLICT(address) DO UPDATE SET unpaid=unpaid+excluded.unpaid, paid=paid+excluded.paid",
        params![address, kind, unpaid, paid],
    )?;
    Ok(())
}

/// Single-instance lock next to the database (released when the process exits).
pub struct Lock(#[allow(dead_code)] std::fs::File);

pub fn lock(dir: &Path) -> Result<Lock> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(dir)?;
    let p = dir.join("ark-tbw.lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p).with_context(|| format!("cannot open {}", p.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another ark-tbw process is using {} (is the service running?)", dir.display());
    }
    Ok(Lock(f))
}

pub fn is_locked(dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let p = dir.join("ark-tbw.lock");
    let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) else { return false };
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

/// Voter share changes, ascending by height.
pub fn share_changes(c: &Connection) -> Result<Vec<(u64, u64)>> {
    let mut st = c.prepare("SELECT height, voter_bps FROM share_changes ORDER BY height")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
