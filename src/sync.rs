//! The chain follower for Mainsail, event driven. For a height range it reads only:
//!   - the validator's own proposed blocks,
//!   - every transaction by timestamp, with its receipt and logs,
//!   - blocks proposed by voters that are validators themselves (reward + fees are balance).
//! Each block and its transactions share one timestamp, so ordering transactions against our
//! blocks needs no per-block reads.
//!
//! Balances follow from the transactions: gas, value, multipayment Payment logs (with their
//! success flag), validator registration and resignation refunds. Votes follow from the
//! consensus contract's Voted/Unvoted logs.
//!
//! The REST API has no historical balances, but the node's JSON-RPC keeps account history for
//! the last 256 blocks. Every own block within that window is checked against the node: one
//! batched eth_getBalance for all voters at that block. A difference (native value moved by a
//! third-party contract, which no log shows) is logged and the node's value is used. So live
//! following is exact; only a catch-up older than the window relies on the events alone, and
//! the reconcile at the tip shows anything that slipped through there.
//!
//! Each own block is split by the votes as they stood after that block.

use crate::alloc::{self, VoterIn};
use crate::api::{Api, Block, Tx, Wallet, HISTORY_SAFE};
use crate::chain;
use crate::config::{fmt_ark, sym, Config};
use crate::db::{self, Account, WalletRow};
use crate::evm::WEI_PER_UNIT;
use crate::{info, warn};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Blocks per chunk: progress is committed after each chunk.
pub const CHUNK: u64 = 20_000;

#[derive(Clone, Debug)]
pub struct Me {
    pub username: String,
    pub address: String,
    pub public_key: String,
    pub chain_id: u64,
}

impl Me {
    pub fn load(c: &Connection) -> Result<Me> {
        let g = |k: &str| -> Result<String> { db::get(c, k)?.ok_or_else(|| anyhow!("database is not initialized: run  ark-tbw init")) };
        Ok(Me { username: g("username")?, address: g("validator_address")?, public_key: g("validator_public_key")?, chain_id: g("chain_id")?.parse()? })
    }
}

/// Wei as an amount of ARK for messages.
pub fn ark(wei: u128) -> String {
    format!("{} {}", fmt_ark((wei / WEI_PER_UNIT) as i128), sym())
}

/// Vote weight of a balance: units of 1e-8 ARK, as on ARK Core.
pub fn weight(wei: u128) -> u64 {
    (wei / WEI_PER_UNIT).min(u64::MAX as u128) as u64
}

/// Block reward plus fees in ledger units (1e-8 ARK), rounded down: never share more than came in.
fn units(wei: u128) -> u64 {
    weight(wei)
}

/// Registration fees, for the refund a resigning validator gets back.
type FeeCache = HashMap<String, u128>;

/// The fee `addr` locked when it registered as a validator: the value of its last successful
/// registration. Validators that came with the chain's genesis paid none.
fn registration_fee(api: &mut Api, cache: &mut FeeCache, addr: &str) -> Result<u128> {
    if let Some(v) = cache.get(addr) {
        return Ok(*v);
    }
    let txs = api.wallet_txs(addr, 0)?;
    let v = txs.iter().rev().find(|t| t.from == addr && chain::registrations(t).iter().any(|r| r == addr)).map(|t| t.value).unwrap_or(0);
    cache.insert(addr.to_string(), v);
    Ok(v)
}

/// Look up the refund of every resignation in `txs` that touches a wallet in `tracked`.
fn prefetch_refunds<'t>(api: &mut Api, cache: &mut FeeCache, txs: impl Iterator<Item = &'t Tx>, tracked: &dyn Fn(&str) -> bool) -> Result<()> {
    for t in txs {
        for r in chain::resignations(t) {
            if tracked(&r) {
                registration_fee(api, cache, &r)?;
            }
        }
    }
    Ok(())
}

/// A wallet's state just before timestamp `from_ts`: its current state minus its history since
/// then. `history` must hold every transaction touching it since from_ts, and `forged` every
/// block it proposed since then.
fn rebuild(fees: &FeeCache, validator: &str, w: &Wallet, history: &[&Tx], forged: &[Block], from_height: u64) -> Result<WalletRow> {
    let mut bal = w.balance as i128;
    let mut first_vote: Option<bool> = None;
    let mut validator_since = false;
    let mut refund = |a: &str| fees.get(a).copied().unwrap_or(0);
    for t in history {
        bal -= chain::delta(t, &w.address, &mut refund);
        if first_vote.is_none() {
            first_vote = chain::vote_change(t, validator, &w.address);
        }
        if chain::registrations(t).iter().any(|r| *r == w.address) {
            validator_since = true;
        }
    }
    for b in forged {
        bal -= (b.reward + b.fee) as i128;
    }
    // first vote event after that point: "voted for us" means it was not voting for us before
    let voting = match first_vote {
        Some(v) => !v,
        None => w.vote.as_deref() == Some(validator),
    };
    if bal < 0 {
        bail!("rebuilt a negative balance for {} at height {}: the API history looks incomplete", w.address, from_height.saturating_sub(1));
    }
    Ok(WalletRow { public_key: w.public_key.clone(), balance: bal as u128, voting, validator: w.validator && !validator_since, first_seen: from_height })
}

/// Bring in one wallet as of just before block `from_height` (timestamp `from_ts`). History and
/// state are read twice; any transaction landing in between makes it try again.
pub fn onboard(api: &mut Api, fees: &mut FeeCache, validator: &str, address: &str, from_ts: u64, from_height: u64) -> Result<WalletRow> {
    for attempt in 0..8u64 {
        let h1 = api.wallet_txs(address, from_ts)?;
        let w1 = api.wallet(address)?.ok_or_else(|| anyhow!("wallet {address} not found"))?;
        let forged: Vec<Block> = if w1.validator { api.forged_since(address, from_height)?.into_iter().filter(|b| b.ts >= from_ts).collect() } else { Vec::new() };
        let h2 = api.wallet_txs(address, from_ts)?;
        let w2 = api.wallet(address)?.ok_or_else(|| anyhow!("wallet {address} not found"))?;
        let same = h1.len() == h2.len() && h1.iter().zip(&h2).all(|(a, b)| a.id == b.id) && w1.balance == w2.balance && w1.nonce == w2.nonce && w1.last_block == w2.last_block;
        if same {
            prefetch_refunds(api, fees, h2.iter(), &|a| a == address)?;
            let refs: Vec<&Tx> = h2.iter().collect();
            return rebuild(fees, validator, &w2, &refs, &forged, from_height);
        }
        if attempt > 2 {
            warn!("wallet {address} keeps changing while it is read, retrying");
        }
        std::thread::sleep(std::time::Duration::from_secs(2 + attempt));
    }
    bail!("could not read a consistent state for wallet {address}")
}

/// Everything that can change our voters between two heights.
pub struct Events {
    pub to: Block,
    pub own: Vec<Block>,
    pub txs: Vec<Tx>,
    /// blocks proposed by tracked wallets that are validators (not us)
    pub forged: Vec<Block>,
}

pub fn fetch_events(api: &mut Api, me: &Me, validators: &BTreeSet<String>, from: u64, from_ts: u64, to: u64) -> Result<Events> {
    let end = api.block(to)?.ok_or_else(|| anyhow!("block {to} not found"))?;
    let own = api.forged_range(&me.address, from, to)?;
    let txs = if end.ts > from_ts { api.txs_by_time(from_ts + 1, end.ts)? } else { Vec::new() };
    let mut forged = Vec::new();
    for d in validators {
        if *d != me.address {
            forged.extend(api.forged_range(d, from, to)?);
        }
    }
    forged.sort_by_key(|b| b.height);
    Ok(Events { to: end, own, txs, forged })
}

pub struct Engine<'a> {
    pub cfg: &'a Config,
    pub api: &'a mut Api,
    pub conn: &'a mut Connection,
    pub me: Me,
    pub wallets: HashMap<String, WalletRow>,
    pub accounts: HashMap<String, Account>,
    pub last_height: u64,
    pub last_id: String,
    pub last_ts: u64,
    fees: FeeCache,
    /// highest block the node is known to have (for the account history window)
    tip_hint: u64,
}

pub struct ChunkResult {
    pub height: u64,
    pub own_blocks: u64,
    /// own blocks checked against the node's balances / wallets corrected at them
    pub verified: u64,
    pub corrected: u64,
    pub payout_due: bool,
}

impl<'a> Engine<'a> {
    pub fn load(cfg: &'a Config, api: &'a mut Api, conn: &'a mut Connection) -> Result<Engine<'a>> {
        let me = Me::load(conn)?;
        let mut e =
            Engine { cfg, api, conn, me, wallets: HashMap::new(), accounts: HashMap::new(), last_height: 0, last_id: String::new(), last_ts: 0, fees: HashMap::new(), tip_hint: 0 };
        e.reload()?;
        Ok(e)
    }

    pub fn reload(&mut self) -> Result<()> {
        self.wallets = db::load_wallets(self.conn)?;
        self.accounts = db::load_accounts(self.conn)?;
        self.last_height = db::get_u64(self.conn, "last_height")?.ok_or_else(|| anyhow!("database is not initialized: run  ark-tbw init"))?;
        self.last_id = db::get(self.conn, "last_id")?.unwrap_or_default();
        self.last_ts = db::get_u64(self.conn, "last_ts")?.unwrap_or(0);
        Ok(())
    }

    pub fn own_blocks_since_payout(&self) -> Result<u64> {
        let from = db::get_u64(self.conn, "last_payout_height")?.unwrap_or(0);
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM blocks WHERE height > ?1", params![from as i64], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// What the payout wallet holds for others: unpaid rewards plus payments in flight.
    fn held_for_others(&self) -> Result<u64> {
        let unpaid: i64 = self.accounts.iter().filter(|(a, _)| **a != self.me.address).map(|(_, x)| x.unpaid.max(0)).sum();
        let in_flight: i64 = self.conn.query_row("SELECT COALESCE(SUM(amount),0) FROM payout_items WHERE status='pending'", [], |r| r.get(0))?;
        Ok((unpaid + in_flight).max(0) as u64)
    }

    fn validators(&self) -> BTreeSet<String> {
        self.wallets.iter().filter(|(_, w)| w.validator).map(|(a, _)| a.clone()).collect()
    }

    /// The config in force for a block: share changes made with `ark-tbw share set` apply from their height on.
    fn cfg_at(&self, schedule: &[(u64, u64)], height: u64) -> Result<Config> {
        match schedule.iter().rev().find(|(h, _)| *h <= height) {
            Some((_, bps)) if *bps != self.cfg.voter_bps => self.cfg.with_voter_bps(*bps),
            _ => Ok(self.cfg.clone()),
        }
    }

    /// Process the next chunk up to `target`. Stops right after the own block that makes a
    /// block-count payout due, so each payout covers an exact range of blocks.
    pub fn sync_chunk(&mut self, target: u64) -> Result<Option<ChunkResult>> {
        if self.last_height >= target {
            return Ok(None);
        }
        // the block we stopped at must still be on the chain
        let here = self.api.block(self.last_height)?.ok_or_else(|| anyhow!("block {} not found", self.last_height))?;
        if !self.last_id.is_empty() && here.id != self.last_id {
            bail!(
                "block {} changed since it was processed: the chain was reorganised deeper than network.confirmations ({}). Stopping; check your node.",
                self.last_height,
                self.cfg.confirmations
            );
        }
        self.tip_hint = self.tip_hint.max(target + self.cfg.confirmations);
        let to = target.min(self.last_height + CHUNK);
        let ev = fetch_events(self.api, &self.me, &self.validators(), self.last_height + 1, self.last_ts, to)?;
        let r = self.apply(ev);
        if r.is_err() {
            self.reload()?; // nothing was written: drop the half-applied in-memory state
        }
        r.map(Some)
    }

    /// Check the voters' balances after own block `height` against the node's account history
    /// and use the node's where they differ. None when the block is outside the node's window.
    fn verify(&mut self, height: u64) -> Result<Option<u64>> {
        if self.tip_hint > height + HISTORY_SAFE {
            return Ok(None);
        }
        let addrs: Vec<String> = self.wallets.iter().filter(|(_, w)| w.voting).map(|(a, _)| a.clone()).collect();
        if addrs.is_empty() {
            return Ok(Some(0));
        }
        let Some(node) = self.api.balances_at(&addrs, height)? else { return Ok(None) };
        let mut fixed = 0;
        for (a, b) in addrs.iter().zip(node) {
            let w = self.wallets.get_mut(a).expect("tracked");
            if w.balance != b {
                let diff = b as i128 - w.balance as i128;
                warn!("voter {a} at block {height}: ours {}, node {} ({}{}); using the node's", ark(w.balance), ark(b), if diff > 0 { "+" } else { "-" }, ark(diff.unsigned_abs()));
                w.balance = b;
                fixed += 1;
            }
        }
        Ok(Some(fixed))
    }

    fn apply(&mut self, ev: Events) -> Result<ChunkResult> {
        let schedule = db::share_changes(self.conn)?;
        // while a payout is still confirming, a due payout waits instead of cutting every chunk short
        let every = if crate::payout::pending_payout(self.conn)?.is_some() { 0 } else { self.cfg.every_blocks };
        let mut own_since = self.own_blocks_since_payout()?;
        let mut held = if self.cfg.include_self { self.held_for_others()? } else { 0 };
        let mut splits: Vec<(Block, u64, alloc::BlockSplit)> = Vec::new();
        let mut due = false;
        let mut stop: Option<(u64, String, u64)> = None;
        let (mut verified, mut corrected) = (0u64, 0u64);
        let me = self.me.address.clone();

        // group everything by block timestamp
        let mut stamps: BTreeSet<u64> = BTreeSet::new();
        let mut txs_at: BTreeMap<u64, Vec<&Tx>> = BTreeMap::new();
        for t in &ev.txs {
            txs_at.entry(t.ts).or_default().push(t);
            stamps.insert(t.ts);
        }
        let mut forged_at: BTreeMap<u64, Vec<&Block>> = BTreeMap::new();
        for b in &ev.forged {
            forged_at.entry(b.ts).or_default().push(b);
            stamps.insert(b.ts);
        }
        let own_at: BTreeMap<u64, &Block> = ev.own.iter().map(|b| (b.ts, b)).collect();
        stamps.extend(own_at.keys());
        // new voters that are validators: their blocks in this range are balance too (blocks
        // before they vote are ignored, as the wallet is not tracked yet at that point)
        let mut extra: Vec<Block> = Vec::new();
        let mut checked = HashSet::new();
        for t in &ev.txs {
            for (voter, on) in chain::vote_changes(t, &me) {
                if on && !self.wallets.contains_key(&voter) && voter != me && checked.insert(voter.clone()) {
                    if let Some(w) = self.api.wallet(&voter)? {
                        if w.validator {
                            extra.extend(self.api.forged_range(&voter, self.last_height + 1, ev.to.height)?);
                        }
                    }
                }
            }
        }
        for b in &extra {
            forged_at.entry(b.ts).or_default().push(b);
            stamps.insert(b.ts);
        }
        {
            let wallets = &self.wallets;
            prefetch_refunds(self.api, &mut self.fees, ev.txs.iter(), &|a| wallets.contains_key(a) || checked.contains(a))?;
        }

        for ts in stamps {
            let txs = txs_at.get(&ts).map(Vec::as_slice).unwrap_or(&[]);
            // a wallet that starts voting for us in this block: bring it in as of the block before,
            // then apply the whole block to it
            for t in txs.iter() {
                for (voter, on) in chain::vote_changes(t, &me) {
                    if on && !self.wallets.contains_key(&voter) {
                        let h = own_at.get(&ts).map(|b| b.height).unwrap_or(self.last_height + 1);
                        let w = onboard(self.api, &mut self.fees, &me, &voter, ts, h).with_context(|| format!("bringing in new voter {voter}"))?;
                        info!("new voter {voter} with {}", ark(w.balance));
                        self.wallets.insert(voter.clone(), w);
                    }
                }
            }
            for t in txs.iter() {
                let fees = &self.fees;
                let mut refund = |a: &str| fees.get(a).copied().unwrap_or(0);
                for a in chain::touched(t) {
                    if let Some(w) = self.wallets.get_mut(&a) {
                        let b = w.balance as i128 + chain::delta(t, &a, &mut refund);
                        if b < 0 {
                            bail!("wallet {a} would go negative at transaction {}: state is out of step with the chain", t.id);
                        }
                        w.balance = b as u128;
                    }
                }
                for (voter, on) in chain::vote_changes(t, &me) {
                    if let Some(w) = self.wallets.get_mut(&voter) {
                        w.voting = on;
                    }
                }
                for r in chain::registrations(t) {
                    if let Some(w) = self.wallets.get_mut(&r) {
                        w.validator = true;
                    }
                }
            }
            if let Some(list) = forged_at.get(&ts) {
                for b in list {
                    if let Some(w) = self.wallets.get_mut(&b.proposer) {
                        w.balance += b.reward + b.fee;
                    }
                }
            }
            if let Some(block) = own_at.get(&ts) {
                if let Some(w) = self.wallets.get_mut(&me) {
                    w.balance += block.reward + block.fee; // we vote for ourselves
                }
                if let Some(n) = self.verify(block.height)? {
                    verified += 1;
                    corrected += n;
                }
                let cfg = self.cfg_at(&schedule, block.height)?;
                let voters: Vec<VoterIn> = self
                    .wallets
                    .iter()
                    .filter(|(_, w)| w.voting)
                    .map(|(a, w)| VoterIn { address: a.clone(), weight: weight(w.balance), pending: self.accounts.get(a).map(|x| x.unpaid.max(0) as u64).unwrap_or(0) })
                    .collect();
                let holder = if cfg.include_self { Some((me.as_str(), held)) } else { None };
                let s = alloc::split(&cfg, &me, units(block.reward), units(block.fee), &voters, holder);
                for o in s.voters.iter().filter(|o| o.amount > 0) {
                    self.accounts.entry(o.address.clone()).or_insert_with(|| Account { kind: "voter".into(), ..Default::default() }).unpaid += o.amount as i64;
                }
                for (a, amt) in &s.keep {
                    let acc = self.accounts.entry(a.clone()).or_insert_with(|| Account { kind: "keep".into(), ..Default::default() });
                    acc.kind = "keep".into();
                    acc.unpaid += *amt as i64;
                }
                held += s.to_voters + s.keep.iter().filter(|(a, _)| *a != me).map(|k| k.1).sum::<u64>();
                splits.push(((*block).clone(), cfg.voter_bps, s));
                own_since += 1;
                if every > 0 && own_since >= every {
                    due = true;
                    stop = Some((block.height, block.id.clone(), block.ts));
                    break;
                }
            }
        }

        let (height, id, ts) = stop.unwrap_or((ev.to.height, ev.to.id.clone(), ev.to.ts));
        let tx = self.conn.transaction()?;
        for (a, w) in &self.wallets {
            db::save_wallet(&tx, a, w)?;
        }
        let mut changed: BTreeSet<&str> = BTreeSet::new();
        for (block, bps, s) in &splits {
            tx.execute(
                "INSERT OR REPLACE INTO blocks(height,id,ts,reward,fee,voter_bps,voters,weight,to_voters,to_keep) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    block.height as i64,
                    block.id,
                    block.ts as i64,
                    units(block.reward) as i64,
                    units(block.fee) as i64,
                    *bps as i64,
                    s.voters.iter().filter(|o| o.amount > 0).count() as i64,
                    s.total_weight as i64,
                    s.to_voters as i64,
                    s.keep[0].1 as i64
                ],
            )?;
            let mut ins = tx.prepare_cached("INSERT OR REPLACE INTO allocations(height,address,weight,amount) VALUES(?1,?2,?3,?4)")?;
            for o in s.voters.iter().filter(|o| o.amount > 0) {
                ins.execute(params![block.height as i64, o.address, o.counted as i64, o.amount as i64])?;
                changed.insert(&o.address);
            }
            for (a, amt) in &s.keep {
                ins.execute(params![block.height as i64, a, 0i64, *amt as i64])?;
                changed.insert(a);
            }
        }
        for a in changed {
            db::save_account(&tx, a, &self.accounts[a])?;
        }
        db::set(&tx, "last_height", &height.to_string())?;
        db::set(&tx, "last_id", &id)?;
        db::set(&tx, "last_ts", &ts.to_string())?;
        tx.commit()?;
        self.last_height = height;
        self.last_id = id;
        self.last_ts = ts;
        Ok(ChunkResult { height, own_blocks: splits.len() as u64, verified, corrected, payout_due: due })
    }

    /// Compare our voters with the node: the voter list at the node's current height (votes
    /// since our height projected from the logs), and every voter's balance at our height from
    /// the node's account history. Repairs any wallet that differs and returns how many, or
    /// None when the chain moved while reading or our height is outside the history window.
    pub fn reconcile(&mut self) -> Result<Option<usize>> {
        let (t1, _) = self.api.tip()?;
        let snap = self.api.voters(&self.me.address)?;
        let (t2, _) = self.api.tip()?;
        if t1 != t2 || t1 < self.last_height {
            return Ok(None);
        }
        self.tip_hint = self.tip_hint.max(t1);
        let me = self.me.address.clone();
        let mut voting: HashMap<String, bool> = self.wallets.iter().map(|(a, w)| (a.clone(), w.voting)).collect();
        let mut new_in_window: HashSet<String> = HashSet::new();
        if t1 > self.last_height {
            let end = self.api.block(t1)?.ok_or_else(|| anyhow!("block {t1} not found"))?;
            let txs = if end.ts > self.last_ts { self.api.txs_by_time(self.last_ts + 1, end.ts)? } else { Vec::new() };
            for t in &txs {
                for (voter, on) in chain::vote_changes(t, &me) {
                    match voting.get_mut(&voter) {
                        Some(v) => *v = on,
                        None => {
                            new_in_window.insert(voter);
                        }
                    }
                }
            }
        }
        let mut bad: BTreeSet<String> = BTreeSet::new();
        let on_node: HashSet<&str> = snap.iter().map(|w| w.address.as_str()).collect();
        for w in &snap {
            match voting.get(&w.address) {
                Some(true) => {}
                Some(false) => {
                    warn!("{} votes for us on the node but not in our state", w.address);
                    bad.insert(w.address.clone());
                }
                None if new_in_window.contains(&w.address) => {}
                None => {
                    warn!("voter {} is missing from our state", w.address);
                    bad.insert(w.address.clone());
                }
            }
        }
        for (a, v) in &voting {
            if *v && !on_node.contains(a.as_str()) {
                warn!("{a} votes for us in our state but not on the node");
                bad.insert(a.clone());
            }
        }
        let mut fixed: Vec<(String, WalletRow)> = Vec::new();
        for a in &bad {
            let mut w = onboard(self.api, &mut self.fees, &me, a, self.last_ts + 1, self.last_height + 1)?;
            if let Some(old) = self.wallets.get(a) {
                w.first_seen = old.first_seen;
                info!("repaired {a}: {} -> {}, voting {} -> {}", ark(old.balance), ark(w.balance), old.voting, w.voting);
            }
            fixed.push((a.clone(), w));
        }
        // balances at our height, exactly, from the node's account history
        let addrs: Vec<String> = self.wallets.keys().filter(|a| !bad.contains(*a)).cloned().chain(fixed.iter().map(|f| f.0.clone())).collect();
        let exact = if addrs.is_empty() { Some(Vec::new()) } else { self.api.balances_at(&addrs, self.last_height)? };
        let Some(exact) = exact else {
            if !fixed.is_empty() {
                self.save(&fixed)?;
            }
            return Ok(if bad.is_empty() { None } else { Some(bad.len()) });
        };
        let mut count = bad.len();
        for (a, b) in addrs.iter().zip(exact) {
            if let Some(f) = fixed.iter_mut().find(|f| f.0 == *a) {
                f.1.balance = b;
                continue;
            }
            let w = &self.wallets[a];
            if w.balance != b {
                warn!("wallet {a} at block {}: ours {}, node {}; using the node's", self.last_height, ark(w.balance), ark(b));
                let mut w = w.clone();
                w.balance = b;
                fixed.push((a.clone(), w));
                count += 1;
            }
        }
        self.save(&fixed)?;
        Ok(Some(count))
    }

    fn save(&mut self, fixed: &[(String, WalletRow)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (a, w) in fixed {
            db::save_wallet(&tx, a, w)?;
        }
        tx.commit()?;
        for (a, w) in fixed {
            self.wallets.insert(a.clone(), w.clone());
        }
        Ok(())
    }
}

/// Initialise a fresh database at `start`: the state right after that block. Voters are read
/// in bulk: two snapshots of the voter list around one pass over all transactions since
/// `start` give every voter's balance at `start` without a request per voter. Within the
/// node's account history window the balances are then taken from the node exactly.
pub fn initialize(cfg: &Config, api: &mut Api, conn: &mut Connection, start: Option<u64>) -> Result<u64> {
    if db::get(conn, "last_height")?.is_some() {
        bail!("database {} is already initialized", cfg.db_path().display());
    }
    let node = api.node_info()?;
    let me = api.validator(&cfg.validator)?.ok_or_else(|| anyhow!("validator {} not found on {}", cfg.validator, api.base()))?;
    let username = me.username.clone().unwrap_or_else(|| cfg.validator.clone());
    if me.resigned {
        bail!("validator {username} has resigned");
    }
    let pk = me.public_key.clone().ok_or_else(|| anyhow!("validator has no public key on the chain yet"))?;
    let (tip, _) = api.tip()?;
    let safe = tip.saturating_sub(cfg.confirmations);
    let h0 = start.or(if cfg.start_height > 0 { Some(cfg.start_height) } else { None }).unwrap_or(safe);
    if h0 > safe {
        bail!("start height {h0} is above the confirmed tip {safe}");
    }
    let b0 = api.block(h0)?.ok_or_else(|| anyhow!("block {h0} not found"))?;
    info!("initializing {username} ({}) from height {h0}, {} blocks behind the tip", me.address, tip - h0);

    let mut fees = HashMap::new();
    let mut rows: BTreeMap<String, WalletRow> = BTreeMap::new();
    let mut consistent = false;
    for attempt in 0..6u64 {
        let snap_a = api.voters(&me.address)?;
        let (tip_now, _) = api.tip()?;
        let head = api.block(tip_now)?.ok_or_else(|| anyhow!("tip block not found"))?;
        // everything since h0, up to and a little past the tip we just saw
        let txs = api.txs_by_time(b0.ts + 1, head.ts + 3_600_000)?;
        let snap_b = api.voters(&me.address)?;
        let key = |v: &[Wallet]| v.iter().map(|w| (w.address.clone(), w.balance, w.nonce, w.last_block)).collect::<BTreeSet<_>>();
        if key(&snap_a) != key(&snap_b) {
            if attempt > 1 {
                warn!("voters changed while reading, retrying");
            }
            std::thread::sleep(std::time::Duration::from_secs(3 + attempt * 2));
            continue;
        }
        info!("read {} voters and {} transactions since height {h0}", snap_b.len(), txs.len());
        // who was voting right after h0: the first vote event since then decides, else today's vote
        let mut first: HashMap<String, bool> = HashMap::new();
        for t in &txs {
            for (voter, on) in chain::vote_changes(t, &me.address) {
                first.entry(voter).or_insert(on);
            }
        }
        let mut by_addr: HashMap<String, Vec<&Tx>> = HashMap::new();
        for t in &txs {
            for a in chain::touched(t) {
                by_addr.entry(a).or_default().push(t);
            }
        }
        let current: HashMap<&str, &Wallet> = snap_b.iter().map(|w| (w.address.as_str(), w)).collect();
        let mut v0: BTreeSet<String> = current.keys().filter(|a| !first.contains_key(**a)).map(|a| a.to_string()).collect();
        v0.extend(first.iter().filter(|(_, v)| !**v).map(|(a, _)| a.clone()));
        info!("{} wallets voted for {username} at height {h0}", v0.len());
        prefetch_refunds(api, &mut fees, txs.iter(), &|a| v0.contains(a))?;
        rows.clear();
        for a in &v0 {
            let w = match current.get(a.as_str()) {
                Some(w) => {
                    let forged = if w.validator { api.forged_since(a, h0 + 1)? } else { Vec::new() };
                    let hist = by_addr.get(a).cloned().unwrap_or_default();
                    rebuild(&fees, &me.address, w, &hist, &forged, h0 + 1)?
                }
                // voted for us at h0 but not any more: read it on its own
                None => onboard(api, &mut fees, &me.address, a, b0.ts + 1, h0 + 1)?,
            };
            if w.voting {
                rows.insert(a.clone(), w);
            }
        }
        consistent = true;
        break;
    }
    if !consistent {
        bail!("could not read a consistent voter list; try again");
    }
    // within the node's account history: take the balances from the node itself
    let addrs: Vec<String> = rows.keys().cloned().collect();
    if !addrs.is_empty() {
        match api.balances_at(&addrs, h0)? {
            Some(exact) => {
                let mut fixed = 0;
                for (a, b) in addrs.iter().zip(exact) {
                    let w = rows.get_mut(a).expect("row");
                    if w.balance != b {
                        warn!("voter {a} at block {h0}: rebuilt {}, node {}; using the node's", ark(w.balance), ark(b));
                        w.balance = b;
                        fixed += 1;
                    }
                }
                info!("checked {} voter balances against the node at block {h0} ({fixed} differed)", addrs.len());
            }
            None => info!("block {h0} is older than the node's account history ({} blocks): balances rebuilt from the transactions", crate::api::HISTORY_BLOCKS),
        }
    }
    let total: u128 = rows.values().map(|w| w.balance).sum();
    info!("{} voters with {} at height {h0}", rows.len(), ark(total));

    let tx = conn.transaction()?;
    for (a, w) in &rows {
        db::save_wallet(&tx, a, w)?;
    }
    for (k, v) in [
        ("username", username.clone()),
        ("validator_address", me.address.clone()),
        ("validator_public_key", pk.clone()),
        ("chain_id", node.chain_id.to_string()),
        ("nethash", node.nethash.clone()),
        ("start_height", h0.to_string()),
        ("last_height", h0.to_string()),
        ("last_id", b0.id.clone()),
        ("last_ts", b0.ts.to_string()),
        ("last_payout_height", h0.to_string()),
        ("last_payout_time", db::now().to_string()),
    ] {
        db::set(&tx, k, &v)?;
    }
    tx.commit()?;
    Ok(h0)
}
