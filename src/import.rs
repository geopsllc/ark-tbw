//! Carry over a core3-tbw installation: its unpaid balances and the last block it processed,
//! so ark-tbw continues exactly where it stopped. Stop core3-tbw (tbw and pay) first.

use crate::config::{fmt_ark, fmt_pct, Config};
use crate::{db, info, warn};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::path::Path;

pub struct Core3 {
    pub last_height: u64,
    pub voters: BTreeMap<String, (i64, i64, f64)>, // unpaid, paid, share
    pub validator: BTreeMap<String, (i64, i64)>,
    pub staged: BTreeMap<String, i64>,
}

pub fn read_core3(path: &Path) -> Result<Core3> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("cannot open {}", path.display()))?;
    let last: Option<i64> = c.query_row("SELECT MAX(height) FROM blocks WHERE processed_at IS NOT NULL", [], |r| r.get(0)).context("not a core3-tbw database (blocks table)")?;
    let Some(last) = last else { bail!("core3-tbw has not processed any block yet") };
    let unprocessed: i64 = c.query_row("SELECT COUNT(*) FROM blocks WHERE processed_at IS NULL AND height <= ?1", [last], |r| r.get(0))?;
    if unprocessed > 0 {
        bail!("core3-tbw left {unprocessed} block(s) unprocessed below {last}; let it finish first");
    }
    let mut voters = BTreeMap::new();
    let mut st = c.prepare("SELECT address, unpaid_bal, paid_bal, share FROM voters")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, f64>(3)?)))? {
        let (a, u, p, s) = r?;
        voters.insert(a, (u, p, s));
    }
    let mut validator = BTreeMap::new();
    let mut st = c.prepare("SELECT address, unpaid_bal, paid_bal FROM validator_rewards")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
        let (a, u, p) = r?;
        validator.insert(a, (u, p));
    }
    // staged but never sent: core3-tbw already took these off the unpaid balance
    let mut staged = BTreeMap::new();
    let mut st = c.prepare("SELECT address, payamt FROM staging WHERE processed_at IS NULL")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (a, amt) = r?;
        *staged.entry(a).or_insert(0) += amt;
    }
    Ok(Core3 { last_height: last as u64, voters, validator, staged })
}

pub fn apply(conn: &Connection, cfg: &Config, c: &Core3) -> Result<()> {
    let mut total = 0i64;
    for (a, (u, p, share)) in &c.voters {
        let u = u + c.staged.get(a).copied().unwrap_or(0);
        if u != 0 || *p != 0 {
            db::credit(conn, a, "voter", u, *p)?;
            total += u;
        }
        let bps = (share * 100.0).round() as u64;
        if bps != cfg.voter_bps && !cfg.custom.contains_key(a) {
            warn!("core3-tbw had a custom rate of {} for {a}; add it under [share.custom] to keep it", fmt_pct(bps));
        }
    }
    for (a, (u, p)) in &c.validator {
        let u = u + c.staged.get(a).copied().unwrap_or(0);
        if !cfg.keep.iter().any(|k| &k.address == a) {
            warn!("core3-tbw validator account {a} is not the keep address in the config; its {} ARK unpaid is imported anyway", fmt_ark(u as i128));
        }
        db::credit(conn, a, "keep", u, *p)?;
    }
    for (a, amt) in &c.staged {
        if !c.voters.contains_key(a) && !c.validator.contains_key(a) {
            warn!("core3-tbw staged {} ARK for {a}, which is neither a voter nor a validator account: imported as a voter balance", fmt_ark(*amt as i128));
            db::credit(conn, a, "voter", *amt, 0)?;
        }
    }
    info!("imported core3-tbw: {} voters, {} ARK unpaid to voters, continuing after block {}", c.voters.len(), fmt_ark(total as i128), c.last_height);
    Ok(())
}
