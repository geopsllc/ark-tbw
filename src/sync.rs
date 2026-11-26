//! The chain follower, event driven. For a height range it reads only:
//!   - the validator's own forged blocks (one request per 100 of them, ~2 per day of chain),
//!   - every transaction by timestamp (ARK mainnet carries a few hundred a day),
//!   - blocks forged by voters that are validators themselves (their rewards are balance).
//! Each block has its own timestamp and a transaction carries its block's, so ordering
//! transactions against our blocks needs no per-block reads at all. A month of catch-up
//! costs ~200 requests; the original script ran several full-table queries per voter per block.
//!
//! Each forged block is split by the votes as they stood after that block.

use crate::alloc::{self, VoterIn};
use crate::api::{Api, Block, Tx, Wallet};
use crate::chain;
use crate::config::{fmt_ark, Config};
use crate::db::{self, Account, WalletRow};
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
    pub network: u8,
}

impl Me {
    pub fn load(c: &Connection) -> Result<Me> {
        let g = |k: &str| -> Result<String> { db::get(c, k)?.ok_or_else(|| anyhow!("database is not initialized: run  ark-tbw init")) };
        Ok(Me { username: g("username")?, address: g("validator_address")?, public_key: g("validator_public_key")?, network: g("network")?.parse()? })
    }
}

type HtlcCache = HashMap<String, (String, String, u64)>;

fn htlc_lookup<'c>(api: &'c mut Api, cache: &'c mut HtlcCache) -> impl FnMut(&str) -> Result<(String, String, u64)> + 'c {
    move |id: &str| {
        if let Some(v) = cache.get(id) {
            return Ok(v.clone());
        }
        let t = api.tx(id)?.ok_or_else(|| anyhow!("HTLC lock transaction {id} not found"))?;
        let v = (t.sender.clone(), t.recipient.clone().unwrap_or_default(), t.amount);
        cache.insert(id.to_string(), v.clone());
        Ok(v)
    }
}

/// A wallet's state just before timestamp `from_ts` (after the block before it): its current
/// state minus its history since then. `history` must hold every transaction touching it
/// since from_ts, and `forged` every block it forged since then.
fn rebuild(api: &mut Api, htlc: &mut HtlcCache, validator_pk: &str, w: &Wallet, history: &[&Tx], forged: &[Block], from_height: u64) -> Result<WalletRow> {
    let mut bal = w.balance as i64;
    let mut locked = w.locked as i64;
    let mut look = htlc_lookup(api, htlc);
    let mut first_vote: Option<bool> = None;
    for t in history {
        let (b, l) = chain::delta(t, &w.address, &mut look)?;
        bal -= b;
        locked -= l;
        if t.sender == w.address && first_vote.is_none() {
            first_vote = chain::vote_change(t, validator_pk);
        }
    }
    for b in forged {
        bal -= (b.reward + b.fee) as i64;
    }
    // first vote event after that point: "+us" means it was not voting for us before, "-us" means it was
    let voting = match first_vote {
        Some(v) => !v,
        None => w.vote.as_deref() == Some(validator_pk),
    };
    if bal < 0 || locked < 0 {
        bail!("rebuilt a negative balance for {} at height {}: the API history looks incomplete", w.address, from_height.saturating_sub(1));
    }
    Ok(WalletRow { public_key: w.public_key.clone(), balance: bal, locked, voting, validator: w.username.is_some(), first_seen: from_height })
}

/// Bring in one wallet as of just before block `from_height` (timestamp `from_ts`). History and
/// state are read twice; any transaction landing in between makes it try again.
pub fn onboard(api: &mut Api, htlc: &mut HtlcCache, validator_pk: &str, address: &str, from_ts: u64, from_height: u64) -> Result<WalletRow> {
    for attempt in 0..8u64 {
        let h1 = api.wallet_txs(address, from_ts)?;
        let w1 = api.wallet(address)?.ok_or_else(|| anyhow!("wallet {address} not found"))?;
        // blocks it forged from that point on (from_height is only a lower bound)
        let forged: Vec<Block> =
            if w1.username.is_some() { api.forged_since(address, from_height)?.into_iter().filter(|b| b.ts >= from_ts).collect() } else { Vec::new() };
        let h2 = api.wallet_txs(address, from_ts)?;
        let w2 = api.wallet(address)?.ok_or_else(|| anyhow!("wallet {address} not found"))?;
        let same = h1.len() == h2.len()
            && h1.iter().zip(&h2).all(|(a, b)| a.id == b.id)
            && w1.balance == w2.balance
            && w1.locked == w2.locked
            && w1.produced_blocks == w2.produced_blocks;
        if same {
            let refs: Vec<&Tx> = h2.iter().collect();
            return rebuild(api, htlc, validator_pk, &w2, &refs, &forged, from_height);
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
    /// blocks forged by tracked wallets that are validators (not us)
    pub forged: Vec<Block>,
}

pub fn fetch_events(api: &mut Api, me: &Me, validators: &BTreeSet<String>, from: u64, from_ts: u64, to: u64) -> Result<Events> {
    let end = api.block(&to.to_string())?.ok_or_else(|| anyhow!("block {to} not found"))?;
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
    htlc: HtlcCache,
}

pub struct ChunkResult {
    pub height: u64,
    pub own_blocks: u64,
    pub payout_due: bool,
}

impl<'a> Engine<'a> {
    pub fn load(cfg: &'a Config, api: &'a mut Api, conn: &'a mut Connection) -> Result<Engine<'a>> {
        let me = Me::load(conn)?;
        let mut e = Engine { cfg, api, conn, me, wallets: HashMap::new(), accounts: HashMap::new(), last_height: 0, last_id: String::new(), last_ts: 0, htlc: HashMap::new() };
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
        let here = self.api.block(&self.last_height.to_string())?.ok_or_else(|| anyhow!("block {} not found", self.last_height))?;
        if !self.last_id.is_empty() && here.id != self.last_id {
            bail!(
                "block {} changed since it was processed: the chain was reorganised deeper than network.confirmations ({}). Stopping; check your node.",
                self.last_height,
                self.cfg.confirmations
            );
        }
        let to = target.min(self.last_height + CHUNK);
        let ev = fetch_events(self.api, &self.me, &self.validators(), self.last_height + 1, self.last_ts, to)?;
        let r = self.apply(ev);
        if r.is_err() {
            self.reload()?; // nothing was written: drop the half-applied in-memory state
        }
        r.map(Some)
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
            if chain::vote_change(t, &self.me.public_key) == Some(true) && !self.wallets.contains_key(&t.sender) && t.sender != self.me.address && checked.insert(t.sender.clone()) {
                if let Some(w) = self.api.wallet(&t.sender)? {
                    if w.username.is_some() {
                        extra.extend(self.api.forged_range(&t.sender, self.last_height + 1, ev.to.height)?);
                    }
                }
            }
        }
        for b in &extra {
            forged_at.entry(b.ts).or_default().push(b);
            stamps.insert(b.ts);
        }

        for ts in stamps {
            let txs = txs_at.get(&ts).map(Vec::as_slice).unwrap_or(&[]);
            // a wallet that starts voting for us in this block: bring it in as of the block before
            for t in txs {
                if chain::vote_change(t, &self.me.public_key) == Some(true) && !self.wallets.contains_key(&t.sender) {
                    let h = own_at.get(&ts).map(|b| b.height).unwrap_or(self.last_height + 1);
                    let w = onboard(self.api, &mut self.htlc, &self.me.public_key, &t.sender, ts, h).with_context(|| format!("bringing in new voter {}", t.sender))?;
                    info!("new voter {} with {} ARK", t.sender, fmt_ark(w.balance as i128));
                    self.wallets.insert(t.sender.clone(), w);
                }
            }
            let mut look = htlc_lookup(self.api, &mut self.htlc);
            for t in txs.iter() {
                let mut seen = HashSet::new();
                for a in chain::touched(t) {
                    if !seen.insert(a) {
                        continue;
                    }
                    if let Some(w) = self.wallets.get_mut(a) {
                        let (b, l) = chain::delta(t, a, &mut look)?;
                        w.balance += b;
                        w.locked += l;
                        if w.balance < 0 || w.locked < 0 {
                            bail!("wallet {a} would go negative at transaction {}: state is out of step with the chain", t.id);
                        }
                    }
                }
                if let Some(w) = self.wallets.get_mut(&t.sender) {
                    if let Some(v) = chain::vote_change(t, &self.me.public_key) {
                        w.voting = v;
                    }
                    if t.type_group == 1 && t.tx_type == 2 {
                        w.validator = true;
                    }
                }
            }
            drop(look);
            if let Some(list) = forged_at.get(&ts) {
                for b in list {
                    if let Some(w) = self.wallets.get_mut(&b.generator) {
                        w.balance += (b.reward + b.fee) as i64;
                    }
                }
            }
            if let Some(block) = own_at.get(&ts) {
                if let Some(w) = self.wallets.get_mut(&self.me.address) {
                    w.balance += (block.reward + block.fee) as i64; // we vote for ourselves
                }
                let cfg = self.cfg_at(&schedule, block.height)?;
                let voters: Vec<VoterIn> = self
                    .wallets
                    .iter()
                    .filter(|(_, w)| w.voting)
                    .map(|(a, w)| VoterIn {
                        address: a.clone(),
                        weight: (w.balance + w.locked).max(0) as u64,
                        pending: self.accounts.get(a).map(|x| x.unpaid.max(0) as u64).unwrap_or(0),
                    })
                    .collect();
                let holder = if cfg.include_self { Some((self.me.address.as_str(), held)) } else { None };
                let s = alloc::split(&cfg, &self.me.address, block.reward, block.fee, &voters, holder);
                for o in s.voters.iter().filter(|o| o.amount > 0) {
                    self.accounts.entry(o.address.clone()).or_insert_with(|| Account { kind: "voter".into(), ..Default::default() }).unpaid += o.amount as i64;
                }
                for (a, amt) in &s.keep {
                    let acc = self.accounts.entry(a.clone()).or_insert_with(|| Account { kind: "keep".into(), ..Default::default() });
                    acc.kind = "keep".into();
                    acc.unpaid += *amt as i64;
                }
                held += s.to_voters + s.keep.iter().filter(|(a, _)| *a != self.me.address).map(|k| k.1).sum::<u64>();
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
                    block.reward as i64,
                    block.fee as i64,
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
        Ok(ChunkResult { height, own_blocks: splits.len() as u64, payout_due: due })
    }

    /// Compare our voters with the node's at the node's current height and repair any wallet
    /// that differs. Returns the number of repaired wallets, or None when the chain moved while
    /// reading (try again later).
    pub fn reconcile(&mut self) -> Result<Option<usize>> {
        let (t1, _) = self.api.tip()?;
        let snap = self.api.voters(&self.me.address)?;
        let (t2, _) = self.api.tip()?;
        if t1 != t2 || t1 < self.last_height {
            return Ok(None);
        }
        let mut proj: HashMap<String, (i64, bool)> = self.wallets.iter().map(|(a, w)| (a.clone(), (w.balance + w.locked, w.voting))).collect();
        let mut new_in_window: HashSet<String> = HashSet::new();
        if t1 > self.last_height {
            let ev = fetch_events(self.api, &self.me, &self.validators(), self.last_height + 1, self.last_ts, t1)?;
            let mut look = htlc_lookup(self.api, &mut self.htlc);
            for t in &ev.txs {
                for a in chain::touched(t).into_iter().collect::<HashSet<_>>() {
                    if let Some(p) = proj.get_mut(a) {
                        let (b, l) = chain::delta(t, a, &mut look)?;
                        p.0 += b + l;
                    }
                }
                if let Some(v) = chain::vote_change(t, &self.me.public_key) {
                    match proj.get_mut(&t.sender) {
                        Some(p) => p.1 = v,
                        None => {
                            new_in_window.insert(t.sender.clone());
                        }
                    }
                }
            }
            for b in ev.forged.iter().chain(ev.own.iter()) {
                if let Some(p) = proj.get_mut(&b.generator) {
                    p.0 += (b.reward + b.fee) as i64;
                }
            }
        }
        let mut bad: BTreeSet<String> = BTreeSet::new();
        let on_node: HashSet<&str> = snap.iter().map(|w| w.address.as_str()).collect();
        for w in &snap {
            match proj.get(&w.address) {
                Some((bal, voting)) if *voting && *bal == (w.balance + w.locked) as i64 => {}
                Some((bal, voting)) => {
                    warn!("voter {} differs from the node: ours {} ARK (voting {voting}), node {} ARK", w.address, fmt_ark(*bal as i128), fmt_ark((w.balance + w.locked) as i128));
                    bad.insert(w.address.clone());
                }
                None if new_in_window.contains(&w.address) => {}
                None => {
                    warn!("voter {} is missing from our state", w.address);
                    bad.insert(w.address.clone());
                }
            }
        }
        for (a, (_, voting)) in &proj {
            if *voting && !on_node.contains(a.as_str()) {
                warn!("{a} votes for us in our state but not on the node");
                bad.insert(a.clone());
            }
        }
        if bad.is_empty() {
            return Ok(Some(0));
        }
        let mut fixed = Vec::new();
        for a in &bad {
            let mut w = onboard(self.api, &mut self.htlc, &self.me.public_key, a, self.last_ts + 1, self.last_height + 1)?;
            if let Some(old) = self.wallets.get(a) {
                w.first_seen = old.first_seen;
                info!("repaired {a}: {} -> {} ARK, voting {} -> {}", fmt_ark(old.balance as i128), fmt_ark(w.balance as i128), old.voting, w.voting);
            }
            fixed.push((a.clone(), w));
        }
        let tx = self.conn.transaction()?;
        for (a, w) in &fixed {
            db::save_wallet(&tx, a, w)?;
        }
        tx.commit()?;
        for (a, w) in fixed {
            self.wallets.insert(a, w);
        }
        Ok(Some(bad.len()))
    }
}

/// Initialise a fresh database at `start`: the state right after that block. Voters are read
/// in bulk: two snapshots of the voter list around one pass over all transactions since
/// `start` give every voter's balance at `start` without a request per voter.
pub fn initialize(cfg: &Config, api: &mut Api, conn: &mut Connection, start: Option<u64>) -> Result<u64> {
    if db::get(conn, "last_height")?.is_some() {
        bail!("database {} is already initialized", cfg.db_path().display());
    }
    let node = api.node_info()?;
    cfg.check_network(node.pub_key_hash)?;
    let me = api.validator(&cfg.validator)?.ok_or_else(|| anyhow!("validator {} not found on {}", cfg.validator, api.base()))?;
    let username = me.username.clone().ok_or_else(|| anyhow!("{} is not a validator", cfg.validator))?;
    if me.resigned {
        bail!("validator {username} has resigned");
    }
    let pk = me.public_key.clone().ok_or_else(|| anyhow!("validator has no public key"))?;
    let (tip, _) = api.tip()?;
    let safe = tip.saturating_sub(cfg.confirmations);
    let h0 = start.or(if cfg.start_height > 0 { Some(cfg.start_height) } else { None }).unwrap_or(safe);
    if h0 > safe {
        bail!("start height {h0} is above the confirmed tip {safe}");
    }
    let b0 = api.block(&h0.to_string())?.ok_or_else(|| anyhow!("block {h0} not found"))?;
    info!("initializing {username} ({}) from height {h0}, {} blocks behind the tip", me.address, tip - h0);

    let mut htlc = HashMap::new();
    let mut rows: BTreeMap<String, WalletRow> = BTreeMap::new();
    for attempt in 0..6u64 {
        let snap_a = api.voters(&me.address)?;
        let (tip_now, _) = api.tip()?;
        let head = api.block(&tip_now.to_string())?.ok_or_else(|| anyhow!("tip block not found"))?;
        // everything since h0, up to and a little past the tip we just saw
        let txs = api.txs_by_time(b0.ts + 1, head.ts + 3600)?;
        let snap_b = api.voters(&me.address)?;
        let key = |v: &[Wallet]| v.iter().map(|w| (w.address.clone(), w.balance, w.locked, w.produced_blocks)).collect::<BTreeSet<_>>();
        if key(&snap_a) != key(&snap_b) {
            if attempt > 1 {
                warn!("voters changed while reading, retrying");
            }
            std::thread::sleep(std::time::Duration::from_secs(3 + attempt * 2));
            continue;
        }
        info!("read {} voters and {} transactions since height {h0}", snap_b.len(), txs.len());
        // who was voting right after h0: first vote event since then decides, else today's vote
        let mut first: HashMap<&str, bool> = HashMap::new();
        for t in &txs {
            if let Some(v) = chain::vote_change(t, &pk) {
                first.entry(t.sender.as_str()).or_insert(v);
            }
        }
        let mut by_addr: HashMap<&str, Vec<&Tx>> = HashMap::new();
        for t in &txs {
            for a in chain::touched(t).into_iter().collect::<HashSet<_>>() {
                by_addr.entry(a).or_default().push(t);
            }
        }
        let current: HashMap<&str, &Wallet> = snap_b.iter().map(|w| (w.address.as_str(), w)).collect();
        let mut v0: BTreeSet<&str> = current.keys().filter(|a| !first.contains_key(*a)).copied().collect();
        v0.extend(first.iter().filter(|(_, v)| !**v).map(|(a, _)| *a));
        info!("{} wallets voted for {username} at height {h0}", v0.len());
        rows.clear();
        for a in v0 {
            let w = match current.get(a) {
                Some(w) => {
                    let forged = if w.username.is_some() { api.forged_since(a, h0 + 1)? } else { Vec::new() };
                    let hist = by_addr.get(a).cloned().unwrap_or_default();
                    rebuild(api, &mut htlc, &pk, w, &hist, &forged, h0 + 1)?
                }
                // voted for us at h0 but not any more: read it on its own
                None => onboard(api, &mut htlc, &pk, a, b0.ts + 1, h0 + 1)?,
            };
            if w.voting {
                rows.insert(a.to_string(), w);
            }
        }
        break;
    }
    if rows.is_empty() && !api.voters(&me.address)?.is_empty() {
        bail!("could not read a consistent voter list; try again");
    }
    let total: i64 = rows.values().map(|w| w.balance + w.locked).sum();
    info!("{} voters with {} ARK at height {h0}", rows.len(), fmt_ark(total as i128));

    let tx = conn.transaction()?;
    for (a, w) in &rows {
        db::save_wallet(&tx, a, w)?;
    }
    for (k, v) in [
        ("username", username.clone()),
        ("validator_address", me.address.clone()),
        ("validator_public_key", pk.clone()),
        ("network", node.pub_key_hash.to_string()),
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
