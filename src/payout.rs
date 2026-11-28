//! Payouts on Mainsail. The invariant that rules out double payments: every set of payments is
//! bound to one nonce of the payout wallet when it is first signed, and is only ever re-signed
//! with that same nonce. A nonce can be used once on chain, so each set is paid at most once.
//! A set is given back to the ledger only when the nonce has been consumed by a transaction that
//! is not one of ours for that set (checked by hash), or when the chain says a payment failed.
//!
//! Transactions:
//!   - several recipients: MultiPayment.pay(address[], uint256[]). The contract has no
//!     recipient limit; gas does (5,000,000 per transaction). Measured on testnet: ~24,200 gas
//!     per recipient that already exists, ~38,800 per new address, ~42,000 base, so at most 205
//!     existing (130 new) recipients fit. Recipients are packed by that model with a margin and
//!     every transaction is checked with eth_estimateGas before it is signed.
//!   - one recipient: a plain transfer, the memo in its data.
//!   - the memo of a multipayment (payout.memo) is appended once after the ABI arguments: the
//!     contract ignores the extra bytes (checked on testnet with eth_call / eth_estimateGas and
//!     the pool), so it costs only its calldata gas plus a few gas, once per transaction.
//! The contract forwards each amount with a 5,000 gas stipend and reports success per recipient
//! (Payment logs); a failed amount comes back to the payout wallet in the same transaction.
//! Such a recipient (a contract that needs more gas to receive) goes back to the ledger and is
//! paid with its own transfer from then on; if that fails too, it is blocked until a redirect
//! is configured.
//!
//! Fees: the keep is charged gas limit x gas price when a transaction is signed and refunded the
//! difference to gas used x gas price once it confirms.

use crate::api::{Api, NodeInfo, Tx};
use crate::chain;
use crate::config::{fmt_ark, sym, Config};
use crate::crypto::KeyPair;
use crate::db;
use crate::evm::{self, EvmTx, SignedTx, WEI_PER_UNIT};
use crate::sync::Me;
use crate::{error, info, warn};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const REBROADCAST_SECS: i64 = 30;
pub const STUCK_ATTEMPTS: i64 = 30;

/// Hard cap on recipients per multipayment: what fits in 5,000,000 gas when all exist.
pub const MAX_RECIPIENTS: usize = 205;
/// Gas model of MultiPayment.pay (measured on testnet, calldata included).
pub const MP_BASE_GAS: u64 = 42_000;
/// Extra gas a memo appended to a multipayment costs beyond its calldata (measured: 6).
pub const MP_MEMO_EXTRA_GAS: u64 = 50;
pub const MP_EXISTING_GAS: u64 = 24_200;
pub const MP_NEW_GAS: u64 = 38_800;
/// Planned gas limits are the model plus this many percent.
const GAS_MARGIN_PCT: u64 = 3;

pub struct Signer {
    pub keys: KeyPair,
    pub second: Option<KeyPair>,
    pub chain_id: u64,
    pub address: String,
}

impl Signer {
    pub fn new(keys: KeyPair, second: Option<KeyPair>, chain_id: u64) -> Result<Signer> {
        let address = evm::address_from_public_key(&keys.public).map_err(|e| anyhow!(e))?;
        Ok(Signer { keys, second, chain_id, address })
    }

    pub fn sign(&self, t: &EvmTx) -> Result<SignedTx> {
        t.sign(&self.keys, self.second.as_ref()).map_err(|e| anyhow!("signing failed: {e}"))
    }
}

/// Gas price and limits in force for a payout.
#[derive(Debug, Clone, Copy)]
pub struct Gas {
    /// wei per gas
    pub price: u128,
    /// highest gas one transaction may use
    pub max: u64,
    /// per-recipient gas of the model, scaled up when estimates disagree
    pub scale_pct: u64,
}

impl Gas {
    pub fn new(cfg: &Config, node: &NodeInfo) -> Gas {
        Gas { price: cfg.gas_price.max(node.min_gas_price), max: node.max_gas_limit.min(5_000_000), scale_pct: 100 }
    }
}

/// Fee in ledger units (1e-8 ARK), rounded up: the keep never pays less than the chain takes.
pub fn fee_units(gas: u64, price: u128) -> u64 {
    let wei = gas as u128 * price;
    (wei.div_ceil(WEI_PER_UNIT)) as u64
}

#[derive(Debug, Clone)]
pub struct Item {
    pub account: String,
    pub recipient: String,
    pub amount: u64,
    /// index into Plan::txs, None when no transaction is needed (paid to the payout wallet itself)
    pub tx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct PlannedTx {
    /// memo: the data of a single transfer, or appended after a multipayment's arguments
    pub memo: String,
    /// (recipient, amount in units)
    pub payments: Vec<(String, u64)>,
    pub gas_limit: u64,
    /// gas_limit x price in units
    pub fee: u64,
}

impl PlannedTx {
    pub fn is_multi(&self) -> bool {
        self.payments.len() > 1
    }

    fn evm(&self, chain_id: u64, nonce: u64, price: u128) -> Result<EvmTx> {
        let (to, value, data) = if self.is_multi() {
            let pays: Vec<(String, u128)> = self.payments.iter().map(|(a, v)| (a.clone(), *v as u128 * WEI_PER_UNIT)).collect();
            let value = pays.iter().map(|p| p.1).sum();
            let mut data = evm::pay_call(&pays).map_err(|e| anyhow!(e))?;
            data.extend_from_slice(self.memo.as_bytes());
            (evm::MULTIPAYMENT.to_string(), value, data)
        } else {
            let (a, v) = &self.payments[0];
            (a.clone(), *v as u128 * WEI_PER_UNIT, self.memo.as_bytes().to_vec())
        };
        Ok(EvmTx { chain_id, nonce, gas_price: price, gas_limit: self.gas_limit, to, value, data: hex::encode(data) })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub txs: Vec<PlannedTx>,
    pub total_fee: u64,
    /// what the keep account is charged: fees plus its own payment
    pub main_keep_charge: i64,
    /// recipients skipped because payments to them fail (they need a redirect)
    pub blocked: Vec<String>,
}

impl Plan {
    pub fn external_total(&self) -> u64 {
        self.txs.iter().flat_map(|t| t.payments.iter().map(|p| p.1)).sum()
    }
}

/// What the planner needs to know about recipients.
#[derive(Default, Clone)]
pub struct Recipients {
    /// addresses that already hold a balance (cheaper to pay in a multipayment)
    pub existing: HashSet<String>,
    /// payees table: address -> "direct" | "blocked"
    pub modes: HashMap<String, String>,
}

fn with_margin(g: u64) -> u64 {
    g * (100 + GAS_MARGIN_PCT) / 100
}

fn transfer_limit(memo: &str, gas: &Gas) -> u64 {
    // an account without code costs exactly this; estimates replace it for contracts
    evm::transfer_gas(memo).min(gas.max)
}

fn build_txs(cfg: &Config, gas: &Gas, rec: &Recipients, items: &mut [Item]) -> Vec<PlannedTx> {
    // summed per recipient; single transfers keep the memo of the account they pay
    let mut sums: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for it in items.iter() {
        let memo = if cfg.keep.iter().any(|k| k.address == it.account) { cfg.memo_for_keep(&it.account).to_string() } else { cfg.memo.clone() };
        let e = sums.entry(it.recipient.clone()).or_insert((0, memo));
        e.0 += it.amount;
    }
    let mut multi: Vec<(String, u64)> = Vec::new();
    let mut single: Vec<(String, u64, String)> = Vec::new();
    for (r, (amt, memo)) in sums {
        let direct = rec.modes.get(&r).map(String::as_str) == Some("direct");
        if cfg.multipayment && !direct {
            multi.push((r, amt));
        } else {
            // a payee that cannot receive through the contract gets no memo: its code would see it as a call
            single.push((r.clone(), amt, if direct { String::new() } else { memo }));
        }
    }
    multi.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let per = |a: &str| -> u64 { (if rec.existing.contains(a) { MP_EXISTING_GAS } else { MP_NEW_GAS }) * gas.scale_pct / 100 };
    let mut txs: Vec<PlannedTx> = Vec::new();
    let mut cur: Vec<(String, u64)> = Vec::new();
    // one memo per multipayment, paid once
    let mp_memo = cfg.memo.clone();
    let mp_base = MP_BASE_GAS + if mp_memo.is_empty() { 0 } else { evm::data_gas(mp_memo.as_bytes()) + MP_MEMO_EXTRA_GAS };
    let mut cur_gas = mp_base;
    let close = |cur: &mut Vec<(String, u64)>, cur_gas: u64, txs: &mut Vec<PlannedTx>| {
        if cur.is_empty() {
            return;
        }
        let p = std::mem::take(cur);
        if p.len() == 1 {
            let memo = cfg.memo.clone();
            let limit = transfer_limit(&memo, gas);
            txs.push(PlannedTx { memo, payments: p, gas_limit: limit, fee: fee_units(limit, gas.price) });
        } else {
            let limit = with_margin(cur_gas).min(gas.max);
            txs.push(PlannedTx { memo: mp_memo.clone(), payments: p, gas_limit: limit, fee: fee_units(limit, gas.price) });
        }
    };
    for (r, amt) in multi {
        let g = per(&r);
        if !cur.is_empty() && (cur.len() >= MAX_RECIPIENTS || with_margin(cur_gas + g) > gas.max) {
            close(&mut cur, cur_gas, &mut txs);
            cur_gas = mp_base;
        }
        cur.push((r, amt));
        cur_gas += g;
    }
    close(&mut cur, cur_gas, &mut txs);
    for (r, amt, memo) in single {
        let limit = transfer_limit(&memo, gas);
        txs.push(PlannedTx { memo, payments: vec![(r, amt)], gas_limit: limit, fee: fee_units(limit, gas.price) });
    }
    let mut where_is: HashMap<String, usize> = HashMap::new();
    for (i, t) in txs.iter().enumerate() {
        for (r, _) in &t.payments {
            where_is.insert(r.clone(), i);
        }
    }
    for it in items.iter_mut() {
        it.tx = where_is.get(&it.recipient).copied();
    }
    txs
}

/// Work out who gets paid now. `accounts` is the ledger (address -> unpaid).
pub fn plan(cfg: &Config, gas: &Gas, me: &Me, rec: &Recipients, accounts: &BTreeMap<String, i64>) -> Plan {
    let main_keep = cfg.main_keep().to_string();
    let min = cfg.min_amount.max(1) as i64;
    let mut internal: Vec<Item> = Vec::new();
    let mut external: Vec<Item> = Vec::new();
    let mut blocked: Vec<String> = Vec::new();
    for (a, unpaid) in accounts {
        if *a == main_keep {
            continue;
        }
        let is_keep = cfg.keep.iter().any(|k| &k.address == a);
        if *unpaid <= 0 || (!is_keep && *unpaid < min) {
            continue;
        }
        let recipient = cfg.payee(a).to_string();
        if recipient != me.address && rec.modes.get(&recipient).map(String::as_str) == Some("blocked") {
            blocked.push(recipient);
            continue;
        }
        let it = Item { account: a.clone(), recipient: recipient.clone(), amount: *unpaid as u64, tx: None };
        if recipient == me.address {
            internal.push(it);
        } else {
            external.push(it);
        }
    }

    let main_keep_unpaid = accounts.get(&main_keep).copied().unwrap_or(0);
    let main_keep_to = cfg.payee(&main_keep).to_string();
    let finish = |mut ext: Vec<Item>, internal: &Vec<Item>| -> (Vec<Item>, Vec<PlannedTx>, u64) {
        let txs = build_txs(cfg, gas, rec, &mut ext);
        let fee = txs.iter().map(|t| t.fee).sum();
        let mut all = ext;
        all.extend(internal.iter().cloned());
        (all, txs, fee)
    };

    let (mut items, mut txs, mut fee) = finish(external.clone(), &internal);
    let mut main_keep_paid: i64 = 0;
    let left = main_keep_unpaid - fee as i64;
    let keep_blocked = main_keep_to != me.address && rec.modes.get(&main_keep_to).map(String::as_str) == Some("blocked");
    if keep_blocked {
        blocked.push(main_keep_to.clone());
    }
    if left > 0 && !keep_blocked {
        if main_keep_to == me.address {
            // the keep is the payout wallet: its share simply stays there
            items.push(Item { account: main_keep.clone(), recipient: main_keep_to, amount: left as u64, tx: None });
            main_keep_paid = left;
        } else {
            let mut ext2 = external.clone();
            ext2.push(Item { account: main_keep.clone(), recipient: main_keep_to.clone(), amount: 1, tx: None });
            let (_, _, fee2) = finish(ext2.clone(), &internal);
            let left2 = main_keep_unpaid - fee2 as i64;
            if left2 > 0 {
                ext2.last_mut().unwrap().amount = left2 as u64;
                let r = finish(ext2, &internal);
                items = r.0;
                txs = r.1;
                fee = r.2;
                main_keep_paid = left2;
            }
        }
    }
    blocked.sort();
    blocked.dedup();
    Plan { items, txs, total_fee: fee, main_keep_charge: fee as i64 + main_keep_paid, blocked }
}

fn ledger(conn: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut st = conn.prepare("SELECT address, unpaid FROM accounts")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn recipients(conn: &Connection) -> Result<Recipients> {
    let mut st = conn.prepare("SELECT address FROM wallets WHERE balance != '0'")?;
    let existing = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(Recipients { existing, modes: db::payees(conn)? })
}

pub fn pending_payout(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT id FROM payouts WHERE status='pending' ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?)
}

fn kind_of(cfg: &Config, a: &str) -> &'static str {
    if cfg.keep.iter().any(|k| k.address == a) {
        "keep"
    } else {
        "voter"
    }
}

fn record_reset(conn: &Connection, height: u64) -> Result<()> {
    db::set(conn, "last_payout_height", &height.to_string())?;
    db::set(conn, "last_payout_time", &db::now().to_string())?;
    Ok(())
}

fn warn_blocked(p: &Plan) {
    for b in &p.blocked {
        warn!("payments to {b} fail on chain (it cannot receive {}); it is skipped until you set a [redirect] for it or run  ark-tbw unblock {b}", sym());
    }
}

/// Shadow payout: record what would be paid and mark it paid, without any transaction.
pub fn run_shadow(cfg: &Config, node: &NodeInfo, me: &Me, conn: &mut Connection, height: u64, reason: &str) -> Result<()> {
    let p = plan(cfg, &Gas::new(cfg, node), me, &recipients(conn)?, &ledger(conn)?);
    warn_blocked(&p);
    let tx = conn.transaction()?;
    tx.execute("INSERT INTO payouts(created_at,height,mode,status,reason) VALUES(?1,?2,'shadow','done',?3)", params![db::now(), height as i64, reason])?;
    let id = tx.last_insert_rowid();
    for it in &p.items {
        if it.account == cfg.main_keep() {
            continue;
        }
        tx.execute("INSERT INTO payout_items(payout_id,account,recipient,amount,slot,status) VALUES(?1,?2,?3,?4,NULL,'shadow')", params![id, it.account, it.recipient, it.amount as i64])?;
        db::credit(&tx, &it.account, kind_of(cfg, &it.account), -(it.amount as i64), it.amount as i64)?;
    }
    let main_keep_amt = p.main_keep_charge - p.total_fee as i64;
    if main_keep_amt > 0 {
        tx.execute("INSERT INTO payout_items(payout_id,account,recipient,amount,slot,status) VALUES(?1,?2,?3,?4,NULL,'shadow')", params![id, cfg.main_keep(), cfg.payee(cfg.main_keep()), main_keep_amt])?;
    }
    db::credit(&tx, cfg.main_keep(), "keep", -p.main_keep_charge, main_keep_amt.max(0))?;
    record_reset(&tx, height)?;
    tx.commit()?;
    info!(
        "shadow payout #{id} up to height {height}: {} payments in {} transaction(s), {} {} to voters and the keep, at most {} {} fees (nothing sent)",
        p.items.len(),
        p.txs.len(),
        fmt_ark(p.items.iter().map(|i| i.amount as i128).sum()),
        sym(),
        fmt_ark(p.total_fee as i128),
        sym()
    );
    Ok(())
}

/// Check every planned transaction with eth_estimateGas. Returns false when one needs more gas
/// than planned (the plan is then made again with a heavier gas model).
fn estimates_fit(api: &mut Api, chain_id: u64, from: &str, gas: &Gas, p: &Plan) -> Result<bool> {
    for t in &p.txs {
        let e = t.evm(chain_id, 0, gas.price)?;
        let data = hex::decode(&e.data)?;
        let est = api.estimate_gas(from, &e.to, e.value, &data).with_context(|| format!("estimating gas for a payment to {}", e.to))?;
        if est > gas.max {
            bail!("a payment transaction needs {est} gas, above the {} limit", gas.max);
        }
        if est > t.gas_limit {
            warn!("transaction with {} payment(s) needs {est} gas, planned {}", t.payments.len(), t.gas_limit);
            return Ok(false);
        }
    }
    Ok(true)
}

/// Live payout: plan, check, sign, persist, then broadcast.
pub fn run_live(cfg: &Config, node: &NodeInfo, me: &Me, signer: &Signer, api: &mut Api, conn: &mut Connection, height: u64, reason: &str) -> Result<()> {
    if pending_payout(conn)?.is_some() {
        return Ok(());
    }
    let mut gas = Gas::new(cfg, node);
    let rec = recipients(conn)?;
    let mut p = plan(cfg, &gas, me, &rec, &ledger(conn)?);
    warn_blocked(&p);
    if p.txs.is_empty() && p.items.is_empty() {
        info!("payout due at height {height} but nothing is owed");
        record_reset(conn, height)?;
        return Ok(());
    }
    let wallet = api.wallet(&me.address)?.ok_or_else(|| anyhow!("payout wallet not found"))?;
    let need = (p.external_total() + p.total_fee) as u128 * WEI_PER_UNIT;
    if wallet.balance < need {
        bail!("payout wallet holds {} but this payout needs {}; payout postponed", crate::sync::ark(wallet.balance), crate::sync::ark(need));
    }
    let pool = api.pool_from(&me.address)?;
    if !pool.is_empty() {
        bail!("the payout wallet has {} unconfirmed transaction(s) in the pool; payout postponed until they confirm", pool.len());
    }
    let mut fits = false;
    for _ in 0..4 {
        if estimates_fit(api, signer.chain_id, &me.address, &gas, &p)? {
            fits = true;
            break;
        }
        gas.scale_pct += 25;
        p = plan(cfg, &gas, me, &rec, &ledger(conn)?);
    }
    if !fits {
        bail!("gas estimates keep exceeding the plan; payout postponed");
    }
    for t in &p.txs {
        if t.fee > cfg.max_fee {
            bail!("transaction fee {} {} is above payout.max_fee {} {}", fmt_ark(t.fee as i128), sym(), fmt_ark(cfg.max_fee as i128), sym());
        }
    }

    let mut nonce = wallet.nonce;
    let mut signed: Vec<(u64, SignedTx, u64)> = Vec::new();
    for t in &p.txs {
        let s = signer.sign(&t.evm(signer.chain_id, nonce, gas.price)?)?;
        signed.push((nonce, s, t.fee));
        nonce += 1;
    }

    let dbtx = conn.transaction()?;
    dbtx.execute("INSERT INTO payouts(created_at,height,mode,status,reason) VALUES(?1,?2,'live','pending',?3)", params![db::now(), height as i64, reason])?;
    let id = dbtx.last_insert_rowid();
    for (n, t, fee) in &signed {
        dbtx.execute("INSERT INTO slots(nonce,payout_id,fee,status) VALUES(?1,?2,?3,'pending')", params![*n as i64, id, *fee as i64])?;
        dbtx.execute(
            "INSERT INTO variants(tx_id,nonce,kind,fee,json,created_at) VALUES(?1,?2,'pay',?3,?4,?5)",
            params![t.hash, *n as i64, *fee as i64, serde_json::to_string(t)?, db::now()],
        )?;
    }
    let main_keep_amt = p.main_keep_charge - p.total_fee as i64;
    for it in &p.items {
        let slot = it.tx.map(|i| signed[i].0 as i64);
        let status = if slot.is_some() { "pending" } else { "paid" };
        dbtx.execute(
            "INSERT INTO payout_items(payout_id,account,recipient,amount,slot,status) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, it.account, it.recipient, it.amount as i64, slot, status],
        )?;
        if it.account == cfg.main_keep() {
            continue; // charged below together with the fees
        }
        let paid_now = if slot.is_none() { it.amount as i64 } else { 0 };
        db::credit(&dbtx, &it.account, kind_of(cfg, &it.account), -(it.amount as i64), paid_now)?;
    }
    let main_keep_internal = p.items.iter().any(|i| i.account == cfg.main_keep() && i.tx.is_none());
    db::credit(&dbtx, cfg.main_keep(), "keep", -p.main_keep_charge, if main_keep_internal { main_keep_amt } else { 0 })?;
    record_reset(&dbtx, height)?;
    if signed.is_empty() {
        dbtx.execute("UPDATE payouts SET status='done' WHERE id=?1", params![id])?;
    }
    dbtx.commit()?;
    let multis = p.txs.iter().filter(|t| t.is_multi()).count();
    info!(
        "payout #{id} up to height {height}: {} payments in {} transaction(s) ({multis} multipayment), {} {}, fees at most {} {}",
        p.items.len(),
        signed.len(),
        fmt_ark(p.external_total() as i128),
        sym(),
        fmt_ark(p.total_fee as i128),
        sym()
    );
    if !signed.is_empty() {
        broadcast_pending(cfg, api, conn, signer, true)?;
    }
    Ok(())
}

fn variants_of(conn: &Connection, nonce: i64) -> Result<Vec<(String, String, i64, String)>> {
    let mut st = conn.prepare("SELECT tx_id, kind, fee, json FROM variants WHERE nonce=?1 ORDER BY created_at, rowid")?;
    let rows = st.query_map(params![nonce], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn broadcast_all(cfg: &Config, api: &mut Api, txs: &[(String, String)]) -> Result<crate::api::BroadcastResult> {
    let res = api.broadcast(txs);
    for extra in &cfg.broadcast {
        let mut other = Api::new(&cfg.api, &cfg.rpc, extra, 0, 0);
        if let Err(e) = other.broadcast(txs) {
            warn!("extra broadcast to {extra} failed: {e:#}");
        }
    }
    res
}

/// A rejection the pool gives for a gas price it will not take (too low, or a replacement
/// that does not pay more than the transaction it replaces).
fn price_rejection(kind: &str, msg: &str) -> bool {
    let k = kind.to_lowercase();
    let m = msg.to_lowercase();
    k.contains("low_fee") || k.contains("fee") || m.contains("gas price") || m.contains("fee") || m.contains("replace") || m.contains("underpriced")
}

/// (Re)broadcast the newest variant of every pending slot that is neither on chain nor in the
/// pool. A gas price rejection re-signs the same nonce at twice the price (up to payout.max_fee).
pub fn broadcast_pending(cfg: &Config, api: &mut Api, conn: &mut Connection, signer: &Signer, force: bool) -> Result<()> {
    let slots: Vec<(i64, i64, i64)> = {
        let mut st = conn.prepare("SELECT nonce, last_broadcast, fee FROM slots WHERE status='pending' ORDER BY nonce")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let now = db::now();
    let due: Vec<i64> = slots.iter().filter(|(_, last, _)| force || now - last >= REBROADCAST_SECS).map(|s| s.0).collect();
    if due.is_empty() {
        return Ok(());
    }
    // A transaction that left the pool because a block included it must not be sent again
    // (the node would refuse it: nonce already used). The wallet nonce shows that at once,
    // before the block is confirmed; resolve() settles it once it is.
    let next_nonce = if force { 0 } else { api.wallet(&signer.address)?.map(|w| w.nonce as i64).unwrap_or(0) };
    let mut batch: Vec<(i64, SignedTx)> = Vec::new();
    for nonce in &due {
        if *nonce < next_nonce {
            continue;
        }
        let vars = variants_of(conn, *nonce)?;
        let Some((id, _, _, json)) = vars.last() else { continue };
        if !force && api.in_pool(id)? {
            continue;
        }
        batch.push((*nonce, serde_json::from_str(json)?));
    }
    if batch.is_empty() {
        return Ok(());
    }
    for chunk in batch.chunks(40) {
        let txs: Vec<(String, String)> = chunk.iter().map(|b| (b.1.hash.clone(), b.1.serialized.clone())).collect();
        let res = match broadcast_all(cfg, api, &txs) {
            Ok(r) => r,
            Err(e) => {
                warn!("broadcast failed, will retry: {e:#}");
                continue;
            }
        };
        for (nonce, st) in chunk {
            let id = &st.hash;
            let rejected = res.rejected.iter().find(|r| &r.0 == id);
            let err = rejected.map(|r| format!("{} {}", r.1, r.2));
            conn.execute("UPDATE slots SET last_broadcast=?1, attempts=attempts+1, last_error=?2 WHERE nonce=?3", params![db::now(), err, nonce])?;
            match rejected {
                None => info!("broadcast 0x{} (nonce {nonce})", &id[..16]),
                Some((_, kind, msg)) if kind.contains("ALREADY") || msg.to_lowercase().contains("already") => {}
                Some((_, kind, msg)) if price_rejection(kind, msg) => {
                    let mut t = st.tx.clone();
                    let mut price = t.gas_price * 2;
                    if fee_units(t.gas_limit, price) > cfg.max_fee {
                        price = cfg.max_fee as u128 * WEI_PER_UNIT / t.gas_limit as u128;
                    }
                    if price <= t.gas_price {
                        error!("transaction 0x{} rejected for its gas price ({kind} {msg}) and payout.max_fee does not allow a higher one", &id[..16]);
                        continue;
                    }
                    t.gas_price = price;
                    let new = fee_units(t.gas_limit, price);
                    let t2 = signer.sign(&t)?;
                    let tx = conn.transaction()?;
                    tx.execute(
                        "INSERT INTO variants(tx_id,nonce,kind,fee,json,created_at) VALUES(?1,?2,'pay',?3,?4,?5)",
                        params![t2.hash, nonce, new as i64, serde_json::to_string(&t2)?, db::now()],
                    )?;
                    let prev_fee: i64 = tx.query_row("SELECT fee FROM slots WHERE nonce=?1", params![nonce], |r| r.get(0))?;
                    tx.execute("UPDATE slots SET fee=?1, last_broadcast=0 WHERE nonce=?2", params![new as i64, nonce])?;
                    db::credit(&tx, cfg.main_keep(), "keep", prev_fee - new as i64, 0)?;
                    tx.commit()?;
                    warn!("gas price too low for nonce {nonce} ({kind} {msg}): re-signed the same payments at {} gwei", price as f64 / 1e9);
                }
                // the nonce was used meanwhile (by this transaction in a block, or by another one):
                // resolve() finds out which once the block is confirmed
                Some((_, _, msg)) if sender_nonce(msg).is_some_and(|n| n > *nonce as u64) => {
                    info!("0x{} (nonce {nonce}) is no longer needed in the pool: the nonce is used, waiting for confirmation", &id[..16])
                }
                Some((_, kind, msg)) => warn!("node rejected 0x{} (nonce {nonce}): {kind} {msg}", &id[..16]),
            }
        }
    }
    Ok(())
}

/// The sender's nonce from a node rejection like "Cannot apply a transaction with nonce 4: the
/// sender 0x... has nonce 5."
fn sender_nonce(msg: &str) -> Option<u64> {
    let rest = &msg[msg.find("has nonce ")? + "has nonce ".len()..];
    rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// Settle pending slots: confirmed, failed (nonce taken by something else) or still waiting.
pub fn resolve(cfg: &Config, api: &mut Api, conn: &mut Connection, me: &Me, signer: Option<&Signer>) -> Result<()> {
    let slots: Vec<(i64, i64, i64, i64)> = {
        let mut st = conn.prepare("SELECT nonce, payout_id, fee, attempts FROM slots WHERE status='pending' ORDER BY nonce")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if slots.is_empty() {
        return Ok(());
    }
    // read the wallet nonce BEFORE looking the transactions up: if one of ours took the nonce,
    // the lookup that follows is guaranteed to find it. Mainsail's nonce is the next one to use.
    let next_nonce = api.wallet(&me.address)?.ok_or_else(|| anyhow!("payout wallet not found"))?.nonce as i64;
    let mut stuck = false;
    for (nonce, payout_id, slot_fee, attempts) in &slots {
        let vars = variants_of(conn, *nonce)?;
        let mut found: Option<(String, String, Tx)> = None;
        for (id, kind, _, _) in &vars {
            if let Some(t) = api.tx(id)? {
                found = Some((id.clone(), kind.clone(), t));
                break;
            }
        }
        match found {
            Some((id, kind, t)) if t.confirmations >= cfg.confirmations => {
                let fee = fee_units(t.gas_used, t.gas_price) as i64;
                let tx = conn.transaction()?;
                let status = if kind == "pay" && t.ok { "confirmed" } else { "failed" };
                tx.execute("UPDATE slots SET status=?1, tx_id=?2, fee=?3 WHERE nonce=?4", params![status, id, fee, nonce])?;
                // the keep was charged slot_fee; the chain took `fee`
                db::credit(&tx, cfg.main_keep(), "keep", slot_fee - fee, 0)?;
                if kind != "pay" {
                    settle_items(&tx, cfg, *nonce, &|_| false)?;
                    warn!("payout #{payout_id}: nonce {nonce} was cancelled; its payments go back to the ledger");
                } else if !t.ok {
                    let single = t.to.as_deref() != Some(evm::MULTIPAYMENT);
                    if single {
                        if let Some(r) = &t.to {
                            db::set_payee(&tx, r, "blocked", &format!("transfer 0x{} failed", &id[..16]))?;
                            warn!("payout #{payout_id}: the transfer to {r} failed on chain; its payment goes back to the ledger and {r} is skipped until it gets a [redirect]");
                        }
                    } else {
                        warn!("payout #{payout_id}: multipayment 0x{} (nonce {nonce}) failed on chain; its payments go back to the ledger", &id[..16]);
                    }
                    settle_items(&tx, cfg, *nonce, &|_| false)?;
                } else {
                    // per-recipient result of a multipayment; a transfer either works or reverts
                    let failed: HashSet<String> = chain::payments(&t).into_iter().filter(|p| !p.2).map(|p| p.0).collect();
                    for r in &failed {
                        db::set_payee(&tx, r, "direct", &format!("multipayment 0x{} could not pay it", &id[..16]))?;
                        warn!("payout #{payout_id}: {r} could not receive through the multipayment; it goes back to the ledger and is paid with its own transfer next time");
                    }
                    settle_items(&tx, cfg, *nonce, &|r| !failed.contains(r))?;
                    info!("payout #{payout_id}: transaction 0x{} (nonce {nonce}) confirmed, fee {} {}", &id[..16], fmt_ark(fee as i128), sym());
                }
                finish_payout(&tx, *payout_id)?;
                tx.commit()?;
            }
            Some(_) => {}
            None if next_nonce > *nonce => {
                let tx = conn.transaction()?;
                tx.execute("UPDATE slots SET status='failed', fee=0 WHERE nonce=?1", params![nonce])?;
                db::credit(&tx, cfg.main_keep(), "keep", *slot_fee, 0)?;
                settle_items(&tx, cfg, *nonce, &|_| false)?;
                finish_payout(&tx, *payout_id)?;
                tx.commit()?;
                warn!("payout #{payout_id}: nonce {nonce} was used by another transaction of the wallet; its payments go back to the ledger and will be paid next time");
            }
            None => {
                if *attempts >= STUCK_ATTEMPTS {
                    stuck = true;
                }
            }
        }
    }
    if stuck {
        error!("a payout transaction has not confirmed after {STUCK_ATTEMPTS} broadcasts; check  ark-tbw payouts  and, if the node keeps rejecting it, run  ark-tbw cancel-payout");
    }
    if let Some(s) = signer {
        broadcast_pending(cfg, api, conn, s, false)?;
    }
    Ok(())
}

/// Mark the items of a slot paid (when `paid(recipient)`) or give them back to the ledger.
fn settle_items(tx: &Connection, cfg: &Config, nonce: i64, paid: &dyn Fn(&str) -> bool) -> Result<()> {
    let items: Vec<(i64, String, String, i64)> = {
        let mut st = tx.prepare("SELECT payout_id, account, recipient, amount FROM payout_items WHERE slot=?1 AND status='pending'")?;
        let rows = st.query_map(params![nonce], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (pid, account, recipient, amount) in items {
        let ok = paid(&recipient);
        tx.execute("UPDATE payout_items SET status=?1 WHERE payout_id=?2 AND account=?3", params![if ok { "paid" } else { "failed" }, pid, account])?;
        if ok {
            db::credit(tx, &account, kind_of(cfg, &account), 0, amount)?;
        } else {
            db::credit(tx, &account, kind_of(cfg, &account), amount, 0)?;
        }
    }
    Ok(())
}

fn finish_payout(tx: &Connection, payout_id: i64) -> Result<()> {
    let open: i64 = tx.query_row("SELECT COUNT(*) FROM slots WHERE payout_id=?1 AND status='pending'", params![payout_id], |r| r.get(0))?;
    if open == 0 {
        tx.execute("UPDATE payouts SET status='done' WHERE id=?1", params![payout_id])?;
    }
    Ok(())
}

/// Consume every pending nonce of a payout with a zero-value transfer to the payout wallet
/// itself at a higher gas price (the pool only replaces a transaction for more). Whichever
/// transaction takes a nonce wins: if a payment does, it is paid; if the cancel does, the
/// payments return to the ledger. Either way nothing is paid twice.
pub fn cancel(cfg: &Config, node: &NodeInfo, api: &mut Api, conn: &mut Connection, me: &Me, signer: &Signer, payout_id: i64) -> Result<usize> {
    let nonces: Vec<i64> = {
        let mut st = conn.prepare("SELECT nonce FROM slots WHERE payout_id=?1 AND status='pending' ORDER BY nonce")?;
        let rows = st.query_map(params![payout_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if nonces.is_empty() {
        bail!("payout #{payout_id} has no pending transactions");
    }
    let gas = Gas::new(cfg, node);
    let mut out = Vec::new();
    for n in &nonces {
        let highest = variants_of(conn, *n)?.iter().filter_map(|v| serde_json::from_str::<SignedTx>(&v.3).ok()).map(|s| s.tx.gas_price).max().unwrap_or(gas.price);
        let price = (highest * 2).max(gas.price);
        let t = EvmTx { chain_id: signer.chain_id, nonce: *n as u64, gas_price: price, gas_limit: 21_000, to: me.address.clone(), value: 0, data: String::new() };
        let s = signer.sign(&t)?;
        let fee = fee_units(21_000, price);
        conn.execute(
            "INSERT INTO variants(tx_id,nonce,kind,fee,json,created_at) VALUES(?1,?2,'cancel',?3,?4,?5)",
            params![s.hash, n, fee as i64, serde_json::to_string(&s)?, db::now()],
        )?;
        out.push((s.hash.clone(), s.serialized.clone()));
    }
    let res = broadcast_all(cfg, api, &out).context("broadcasting the cancel transactions")?;
    for r in &res.rejected {
        warn!("cancel transaction 0x{} rejected: {} {}", &r.0[..16.min(r.0.len())], r.1, r.2);
    }
    Ok(nonces.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sender_nonce_from_rejection() {
        let m = "Cannot apply a transaction with nonce 4: the sender 0x7783464d965A1E6a13B19b8222C56C53d32414ad has nonce 5.";
        assert_eq!(sender_nonce(m), Some(5));
        assert_eq!(sender_nonce("insufficient balance"), None);
    }

    use super::*;
    use std::path::Path;

    const D: &str = "0x7783464d965A1E6a13B19b8222C56C53d32414ad";
    const A: &str = "0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22";
    const B: &str = "0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763";
    const R: &str = "0xe5a97E663158dEaF3b65bBF88897b8359Dc19F81";

    fn gas() -> Gas {
        Gas { price: 5_000_000_000, max: 5_000_000, scale_pct: 100 }
    }
    fn me() -> Me {
        Me { username: "d".into(), address: D.into(), public_key: "pk".into(), chain_id: 11812 }
    }
    fn cfg(main_keep: &str, extra: &str) -> Config {
        let t = format!(
            "[network]\napi=\"https://testnet.mainsailhq.com/api\"\n[validator]\nname=\"d\"\n[share]\npercent=50\nkeep_address=\"{main_keep}\"\n[payout]\nevery_blocks=1\nmemo=\"m\"\n{extra}"
        );
        Config::parse(&t, Path::new("/tmp/c.toml")).unwrap()
    }
    fn addr(i: usize) -> String {
        evm::checksum(&evm::keccak(format!("voter {i}").as_bytes())[..20].try_into().unwrap())
    }

    #[test]
    fn fees_round_up() {
        assert_eq!(fee_units(21_000, 5_000_000_000), 10_500); // 0.000105 ARK
        assert_eq!(fee_units(1, 1), 1);
        assert_eq!(fee_units(0, 1), 0);
    }

    #[test]
    fn main_keep_pays_fees_and_min_amount_carries() {
        let c = cfg(R, "min_amount = 1");
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 300_000_000i64);
        l.insert(B.to_string(), 50_000_000i64); // below 1 ARK: carried over
        l.insert(R.to_string(), 400_000_000i64);
        let p = plan(&c, &gas(), &me(), &Recipients::default(), &l);
        assert!(p.items.iter().all(|i| i.account != B));
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments.len(), 2); // A + keep in one multipayment
        // memo "m": 16 calldata gas plus the extra, once
        assert_eq!(p.txs[0].gas_limit, (MP_BASE_GAS + 16 + MP_MEMO_EXTRA_GAS + 2 * MP_NEW_GAS) * 103 / 100);
        let e = p.txs[0].evm(11812, 0, 1).unwrap();
        assert_eq!(e.data, format!("{}6d", hex::encode(evm::pay_call(&p.txs[0].payments.iter().map(|(a, v)| (a.clone(), *v as u128 * WEI_PER_UNIT)).collect::<Vec<_>>()).unwrap())));
        let main_keep = p.items.iter().find(|i| i.account == R).unwrap().amount as i64;
        assert_eq!(main_keep, 400_000_000 - p.total_fee as i64);
        assert_eq!(p.main_keep_charge, 400_000_000);
    }

    #[test]
    fn main_keep_in_payout_wallet_stays_internal() {
        let c = cfg(D, "");
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 300_000_000i64);
        l.insert(D.to_string(), 100_000_000i64);
        let p = plan(&c, &gas(), &me(), &Recipients::default(), &l);
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments, vec![(A.to_string(), 300_000_000)]); // single -> transfer
        assert_eq!(p.txs[0].gas_limit, 21_000 + 16); // memo "m"
        let r = p.items.iter().find(|i| i.account == D).unwrap();
        assert!(r.tx.is_none());
        assert_eq!(r.amount as i64, 100_000_000 - p.total_fee as i64);
        let e = p.txs[0].evm(11812, 3, 5_000_000_000).unwrap();
        assert_eq!((e.to.as_str(), e.value, e.data.as_str()), (A, 300_000_000 * WEI_PER_UNIT, "6d"));
    }

    #[test]
    fn redirect_and_aggregation() {
        let c = cfg(D, &format!("[redirect]\n\"{A}\" = \"{B}\"\n"));
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 5i64);
        l.insert(B.to_string(), 7i64);
        l.insert(D.to_string(), 100_000_000i64);
        let p = plan(&c, &gas(), &me(), &Recipients::default(), &l);
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments, vec![(B.to_string(), 12)]);
        assert_eq!(p.items.iter().filter(|i| i.tx == Some(0)).count(), 2);
    }

    #[test]
    fn packs_by_gas_up_to_the_limit() {
        let c = cfg(D, "");
        let mut l = BTreeMap::new();
        let mut rec = Recipients::default();
        for i in 0..450 {
            l.insert(addr(i), 1_000 + i as i64);
            rec.existing.insert(addr(i));
        }
        let p = plan(&c, &gas(), &me(), &rec, &l);
        let sizes: Vec<usize> = p.txs.iter().map(|t| t.payments.len()).collect();
        // (5M / 1.03 - 42k) / 24.2k = 198 existing recipients per multipayment
        assert_eq!(sizes, vec![198, 198, 54]);
        assert!(p.txs.iter().all(|t| t.gas_limit <= 5_000_000));
        // all new addresses: (5M / 1.03 - 42k) / 38.8k = 124
        let p = plan(&c, &gas(), &me(), &Recipients::default(), &l);
        assert_eq!(p.txs[0].payments.len(), 124);
        // a heavier model (after an estimate disagreed) packs fewer
        let g = Gas { scale_pct: 125, ..gas() };
        assert!(plan(&c, &g, &me(), &rec, &l).txs[0].payments.len() < 198);
        let call = p.txs[0].evm(11812, 0, 1).unwrap();
        assert_eq!(call.to, evm::MULTIPAYMENT);
        assert_eq!(&call.data[..8], "084ce708");
        assert_eq!(call.value, p.txs[0].payments.iter().map(|x| x.1 as u128 * WEI_PER_UNIT).sum::<u128>());
    }

    #[test]
    fn direct_and_blocked_payees() {
        let c = cfg(D, "");
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 1_000i64);
        l.insert(B.to_string(), 2_000i64);
        l.insert(R.to_string(), 3_000i64);
        l.insert(D.to_string(), 100_000_000i64);
        let mut rec = Recipients::default();
        rec.modes.insert(B.into(), "direct".into());
        rec.modes.insert(R.into(), "blocked".into());
        let p = plan(&c, &gas(), &me(), &rec, &l);
        assert_eq!(p.blocked, vec![R.to_string()]);
        assert!(p.items.iter().all(|i| i.account != R)); // stays in the ledger
        assert_eq!(p.txs.len(), 2);
        let b = p.txs.iter().find(|t| t.payments[0].0 == B).unwrap();
        assert_eq!(b.payments.len(), 1);
        assert_eq!(b.memo, ""); // no data for a contract payee
    }

    #[test]
    fn negative_main_keep() {
        let c = cfg(D, "");
        let mut l = BTreeMap::new();
        for (i, a) in [A, B, R].iter().enumerate() {
            l.insert(a.to_string(), (i as i64 + 1) * 1000);
        }
        l.insert(D.to_string(), 0);
        let p = plan(&c, &gas(), &me(), &Recipients::default(), &l);
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments.len(), 3);
        // the keep has nothing: the fees become keep debt, paid off by later blocks
        assert_eq!(p.main_keep_charge, p.total_fee as i64);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// `cargo test multipayment_memo_probe -- --ignored --nocapture`: is a multipayment with a
    /// memo appended after the ABI arguments executed like one without? Simulated by the node
    /// (eth_call / eth_estimateGas from a funded wallet, nothing is sent), then signed from an
    /// empty wallet and posted so the pool has to decode it.
    #[test]
    #[ignore]
    fn multipayment_memo_probe() {
        let api_url = "https://testnet.mainsailhq.com/api";
        let (rpc, pool) = crate::api::default_urls(api_url).unwrap();
        let mut api = Api::new(api_url, &rpc, &pool, 0, 60);
        let node = api.node_info().unwrap();
        let funded = "0x7783464d965A1E6a13B19b8222C56C53d32414ad"; // geop5, holds TARK
        let pays = vec![("0xe5a97E663158dEaF3b65bBF88897b8359Dc19F81".to_string(), 1u128), ("0x4F1D3A2f79AADeC9ecb53b6Ee66854bDE9665aE4".to_string(), 2u128)];
        let plain = evm::pay_call(&pays).unwrap();
        let memo = "geop5 - true block weight payout";
        let mut with_memo = plain.clone();
        with_memo.extend_from_slice(memo.as_bytes());
        let call = |api: &mut Api, value: u128, data: &[u8]| {
            api.rpc_call(
                "eth_call",
                serde_json::json!([{"from": funded, "to": evm::MULTIPAYMENT, "value": format!("0x{value:x}"), "data": format!("0x{}", hex::encode(data))}, "latest"]),
            )
        };
        let g_plain = api.estimate_gas(funded, evm::MULTIPAYMENT, 3, &plain).unwrap();
        let g_memo = api.estimate_gas(funded, evm::MULTIPAYMENT, 3, &with_memo).unwrap();
        println!("estimateGas plain {g_plain}, with {}-byte memo {g_memo} (+{})", memo.len(), g_memo - g_plain);
        println!("eth_call plain: {:?}", call(&mut api, 3, &plain));
        println!("eth_call memo:  {:?}", call(&mut api, 3, &with_memo));
        // control: the simulation is real, a wrong value must revert with or without the memo
        let bad = call(&mut api, 4, &with_memo);
        println!("eth_call memo, wrong value: {bad:?}");
        assert!(bad.is_err(), "a value that does not match the amounts must revert");
        // calldata gas plus a few gas of copying inside the call
        assert!(g_memo - g_plain >= evm::data_gas(memo.as_bytes()) && g_memo - g_plain < evm::data_gas(memo.as_bytes()) + 50);
        assert!(call(&mut api, 3, &with_memo).is_ok());

        // what the planner builds (memo appended, gas limit from the model) fits the node's estimate
        let pt = PlannedTx { memo: memo.into(), payments: vec![(pays[0].0.clone(), 1), (pays[1].0.clone(), 2)], gas_limit: 0, fee: 0 };
        let e = pt.evm(node.chain_id, 0, node.min_gas_price).unwrap();
        assert_eq!(hex::decode(&e.data).unwrap().len(), with_memo.len());
        let est = api.estimate_gas(funded, &e.to, e.value, &hex::decode(&e.data).unwrap()).unwrap();
        let planned = (MP_BASE_GAS + evm::data_gas(memo.as_bytes()) + MP_MEMO_EXTRA_GAS + 2 * MP_EXISTING_GAS) * 103 / 100;
        println!("planner multipayment: estimate {est}, planned limit {planned}");
        assert!(est <= planned);

        // the pool: signed with the memo from an empty wallet, it must get as far as the balance check
        let signer = Signer::new(KeyPair::from_passphrase("ark-tbw throwaway unfunded test wallet 7f3a").unwrap(), None, node.chain_id).unwrap();
        let t = EvmTx { chain_id: node.chain_id, nonce: 0, gas_price: node.min_gas_price, gas_limit: g_memo + 10_000, to: evm::MULTIPAYMENT.into(), value: 3, data: hex::encode(&with_memo) };
        let st = signer.sign(&t).unwrap();
        let res = api.broadcast(&[(st.hash.clone(), st.serialized.clone())]).unwrap();
        println!("pool: {res:?}");
        assert!(res.accepted.is_empty());
        assert!(res.rejected[0].2.contains("Insufficient balance"), "the pool should only object to the empty wallet");
    }

    /// `cargo test broadcast_unfunded -- --ignored --nocapture`: signs a multipayment from a
    /// throwaway, never-funded testnet wallet, estimates it and posts it. The pool must reject it
    /// (no funds can move); the point is that our signed transaction and the response parsing
    /// match the node.
    #[test]
    #[ignore]
    fn broadcast_unfunded() {
        let api_url = "https://testnet.mainsailhq.com/api";
        let (rpc, pool) = crate::api::default_urls(api_url).unwrap();
        let mut api = Api::new(api_url, &rpc, &pool, 0, 60);
        let node = api.node_info().unwrap();
        let signer = Signer::new(KeyPair::from_passphrase("ark-tbw throwaway unfunded test wallet 7f3a").unwrap(), None, node.chain_id).unwrap();
        let pt = PlannedTx {
            memo: String::new(),
            payments: vec![("0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22".into(), 1), ("0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763".into(), 2)],
            gas_limit: 200_000,
            fee: 0,
        };
        let t = signer.sign(&pt.evm(node.chain_id, 0, node.min_gas_price).unwrap()).unwrap();
        let res = api.broadcast(&[(t.hash.clone(), t.serialized.clone())]).unwrap();
        println!("from {} chain {}: {res:?}", signer.address, node.chain_id);
        assert!(res.accepted.is_empty());
        assert_eq!(res.rejected.len(), 1);
        assert_eq!(res.rejected[0].0, t.hash);
    }
}
