//! Payouts. The invariant that rules out double payments: every set of payments is bound to
//! one nonce of the payout wallet when it is first signed, and is only ever re-signed with that
//! same nonce. A nonce can be used once on chain, so each set is paid at most once. A set is
//! given back to the ledger only when the nonce has been consumed by a transaction that is not
//! one of ours for that set (checked by id).

use crate::api::{Api, NodeInfo};
use crate::config::{fmt_ark, Config};
use crate::db;
use crate::sync::Me;
use crate::tx::{self, Signer, TxData};
use crate::{error, info, warn};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::BTreeMap;

pub const REBROADCAST_SECS: i64 = 60;
pub const STUCK_ATTEMPTS: i64 = 30;

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
    pub memo: String,
    pub payments: Vec<(String, u64)>,
    pub fee: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub txs: Vec<PlannedTx>,
    pub total_fee: u64,
    /// what the keep account is charged: fees plus its own payment
    pub main_keep_charge: i64,
}

impl Plan {
    pub fn external_total(&self) -> u64 {
        self.txs.iter().flat_map(|t| t.payments.iter().map(|p| p.1)).sum()
    }
}

/// Core v3 minimum dynamic fee: (addonBytes + round(serialized bytes / 2)) * minFeePool,
/// as ARK Wallet verified on chain (the exact minimum is accepted, one arktoshi less is not).
pub fn fee_for(node: &NodeInfo, memo: &str, n: usize, second: bool) -> u64 {
    let half = |bytes: u64| (bytes + 1) / 2; // Math.round(bytes / 2)
    if n == 1 {
        if node.dynamic {
            (node.addon_transfer + half(tx::size_transfer(memo, second))) * node.per_byte
        } else {
            node.static_transfer
        }
    } else if node.dynamic {
        (node.addon_multipayment + half(tx::size_multipayment(memo, n, second))) * node.per_byte
    } else {
        node.static_multipayment
    }
}

fn build_txs(cfg: &Config, node: &NodeInfo, second: bool, items: &mut [Item]) -> Vec<PlannedTx> {
    // payments per memo, summed per recipient
    let mut groups: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut memo_of: Vec<String> = Vec::new();
    for it in items.iter() {
        let memo = if cfg.keep.iter().any(|k| k.address == it.account) { cfg.memo_for_keep(&it.account).to_string() } else { cfg.memo.clone() };
        memo_of.push(memo.clone());
        if it.tx.is_some() || it.recipient != "" {
            *groups.entry(memo).or_default().entry(it.recipient.clone()).or_default() += it.amount;
        }
    }
    let per_tx = if cfg.multipayment { node.multipayment_limit } else { 1 };
    let mut txs = Vec::new();
    let mut where_is: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (memo, recips) in groups {
        let mut list: Vec<(String, u64)> = recips.into_iter().collect();
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for chunk in list.chunks(per_tx) {
            let idx = txs.len();
            for (r, _) in chunk {
                where_is.insert((memo.clone(), r.clone()), idx);
            }
            txs.push(PlannedTx { memo: memo.clone(), payments: chunk.to_vec(), fee: fee_for(node, &memo, chunk.len(), second) });
        }
    }
    for (it, memo) in items.iter_mut().zip(memo_of) {
        it.tx = where_is.get(&(memo, it.recipient.clone())).copied();
    }
    txs
}

/// Work out who gets paid now. `accounts` is the ledger (address -> unpaid).
pub fn plan(cfg: &Config, node: &NodeInfo, me: &Me, second: bool, accounts: &BTreeMap<String, i64>) -> Plan {
    let main_keep = cfg.main_keep().to_string();
    let min = cfg.min_amount.max(1) as i64;
    let mut internal: Vec<Item> = Vec::new();
    let mut external: Vec<Item> = Vec::new();
    for (a, unpaid) in accounts {
        if *a == main_keep {
            continue;
        }
        let is_keep = cfg.keep.iter().any(|k| &k.address == a);
        if *unpaid <= 0 || (!is_keep && *unpaid < min) {
            continue;
        }
        let recipient = cfg.payee(a).to_string();
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
        let txs = build_txs(cfg, node, second, &mut ext);
        let fee = txs.iter().map(|t| t.fee).sum();
        let mut all = ext;
        all.extend(internal.iter().cloned());
        (all, txs, fee)
    };

    let (mut items, mut txs, mut fee) = finish(external.clone(), &internal);
    let mut main_keep_paid: i64 = 0;
    let left = main_keep_unpaid - fee as i64;
    if left > 0 {
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
    Plan { items, txs, total_fee: fee, main_keep_charge: fee as i64 + main_keep_paid }
}

fn ledger(conn: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut st = conn.prepare("SELECT address, unpaid FROM accounts")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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

/// Shadow payout: record what would be paid and mark it paid, without any transaction.
pub fn run_shadow(cfg: &Config, node: &NodeInfo, me: &Me, conn: &mut Connection, height: u64, reason: &str) -> Result<()> {
    let p = plan(cfg, node, me, false, &ledger(conn)?);
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
        "shadow payout #{id} up to height {height}: {} payments, {} ARK to voters and the keep, {} ARK fees (nothing sent)",
        p.items.len(),
        fmt_ark(p.items.iter().map(|i| i.amount as i128).sum()),
        fmt_ark(p.total_fee as i128)
    );
    Ok(())
}

/// Live payout: plan, sign, persist, then broadcast.
pub fn run_live(cfg: &Config, node: &NodeInfo, me: &Me, signer: &Signer, api: &mut Api, conn: &mut Connection, height: u64, reason: &str) -> Result<()> {
    if pending_payout(conn)?.is_some() {
        return Ok(());
    }
    let p = plan(cfg, node, me, signer.second.is_some(), &ledger(conn)?);
    if p.txs.is_empty() && p.items.is_empty() {
        info!("payout due at height {height} but nothing is owed");
        record_reset(conn, height)?;
        return Ok(());
    }
    for t in &p.txs {
        if t.fee > cfg.max_fee {
            bail!("transaction fee {} ARK is above payout.max_fee {} ARK", fmt_ark(t.fee as i128), fmt_ark(cfg.max_fee as i128));
        }
    }
    let wallet = api.wallet(&me.address)?.ok_or_else(|| anyhow!("payout wallet not found"))?;
    let need = p.external_total() + p.total_fee;
    if wallet.balance < need {
        bail!("payout wallet holds {} ARK but this payout needs {} ARK; payout postponed", fmt_ark(wallet.balance as i128), fmt_ark(need as i128));
    }
    let pool = api.pool_from(&me.public_key)?;
    if !pool.is_empty() {
        bail!("the payout wallet has {} unconfirmed transaction(s) in the pool; payout postponed until they confirm", pool.len());
    }

    let mut nonce = wallet.nonce + 1;
    let mut signed: Vec<(u64, TxData, u64)> = Vec::new();
    for t in &p.txs {
        let s = if t.payments.len() == 1 {
            signer.transfer(nonce, t.fee, &t.memo, &t.payments[0].0, t.payments[0].1)
        } else {
            signer.multipayment(nonce, t.fee, &t.memo, &t.payments)
        }
        .map_err(|e| anyhow!("signing failed: {e}"))?;
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
            params![t.id.as_ref().unwrap(), *n as i64, *fee as i64, serde_json::to_string(t)?, db::now()],
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
    info!(
        "payout #{id} up to height {height}: {} payments in {} transaction(s), {} ARK, fees {} ARK",
        p.items.len(),
        signed.len(),
        fmt_ark(p.external_total() as i128),
        fmt_ark(p.total_fee as i128)
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

fn broadcast_all(cfg: &Config, api: &mut Api, txs: &[Value]) -> Result<crate::api::BroadcastResult> {
    let res = api.broadcast(txs);
    for extra in &cfg.broadcast {
        let mut other = Api::new(extra, 0);
        if let Err(e) = other.broadcast(txs) {
            warn!("extra broadcast to {extra} failed: {e:#}");
        }
    }
    res
}

/// (Re)broadcast the newest variant of every pending slot that is neither on chain nor in the
/// pool. A fee rejection re-signs the same nonce with twice the fee (up to payout.max_fee).
pub fn broadcast_pending(cfg: &Config, api: &mut Api, conn: &mut Connection, signer: &Signer, force: bool) -> Result<()> {
    let slots: Vec<(i64, i64, i64)> = {
        let mut st = conn.prepare("SELECT nonce, last_broadcast, fee FROM slots WHERE status='pending' ORDER BY nonce")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let now = db::now();
    let mut batch: Vec<(i64, String, Value)> = Vec::new();
    for (nonce, last, _) in &slots {
        if !force && now - last < REBROADCAST_SECS {
            continue;
        }
        let vars = variants_of(conn, *nonce)?;
        let Some((id, _, _, json)) = vars.last() else { continue };
        if !force && api.in_pool(id)? {
            continue;
        }
        batch.push((*nonce, id.clone(), serde_json::from_str(json)?));
    }
    if batch.is_empty() {
        return Ok(());
    }
    let per = 40usize;
    for chunk in batch.chunks(per) {
        let txs: Vec<Value> = chunk.iter().map(|b| b.2.clone()).collect();
        let res = match broadcast_all(cfg, api, &txs) {
            Ok(r) => r,
            Err(e) => {
                warn!("broadcast failed, will retry: {e:#}");
                continue;
            }
        };
        for (nonce, id, json) in chunk {
            let rejected = res.rejected.iter().find(|r| &r.0 == id);
            let err = rejected.map(|r| format!("{} {}", r.1, r.2));
            conn.execute(
                "UPDATE slots SET last_broadcast=?1, attempts=attempts+1, last_error=?2 WHERE nonce=?3",
                params![db::now(), err, nonce],
            )?;
            match rejected {
                None => info!("broadcast {} (nonce {nonce})", &id[..16]),
                Some((_, kind, msg)) if kind.contains("LOW_FEE") || msg.to_lowercase().contains("fee") => {
                    let mut t: TxData = serde_json::from_value(json.clone())?;
                    let old: u64 = t.fee.parse()?;
                    let new = (old * 2).min(cfg.max_fee);
                    if new <= old {
                        error!("transaction {} rejected for its fee and payout.max_fee does not allow a higher one", &id[..16]);
                        continue;
                    }
                    t.fee = new.to_string();
                    let t2 = resign(signer, t)?;
                    let tx = conn.transaction()?;
                    tx.execute(
                        "INSERT INTO variants(tx_id,nonce,kind,fee,json,created_at) VALUES(?1,?2,'pay',?3,?4,?5)",
                        params![t2.id.as_ref().unwrap(), nonce, new as i64, serde_json::to_string(&t2)?, db::now()],
                    )?;
                    let prev_fee: i64 = tx.query_row("SELECT fee FROM slots WHERE nonce=?1", params![nonce], |r| r.get(0))?;
                    tx.execute("UPDATE slots SET fee=?1, last_broadcast=0 WHERE nonce=?2", params![new as i64, nonce])?;
                    db::credit(&tx, cfg.main_keep(), "keep", prev_fee - new as i64, 0)?;
                    tx.commit()?;
                    warn!("fee too low for nonce {nonce}: re-signed the same payments with fee {} ARK", fmt_ark(new as i128));
                }
                Some((_, kind, msg)) => warn!("node rejected {} (nonce {nonce}): {kind} {msg}", &id[..16]),
            }
        }
    }
    Ok(())
}

fn resign(signer: &Signer, mut t: TxData) -> Result<TxData> {
    let nonce: u64 = t.nonce.parse()?;
    let fee: u64 = t.fee.parse()?;
    let memo = t.vendor_field.take().unwrap_or_default();
    if t.tx_type == tx::TYPE_TRANSFER {
        let amount: u64 = t.amount.parse()?;
        signer.transfer(nonce, fee, &memo, t.recipient_id.as_deref().unwrap_or_default(), amount).map_err(|e| anyhow!(e))
    } else {
        let payments: Vec<(String, u64)> = t.asset.as_ref().and_then(|a| a["payments"].as_array()).ok_or_else(|| anyhow!("no payments"))?
            .iter()
            .map(|p| Ok((p["recipientId"].as_str().unwrap_or_default().to_string(), p["amount"].as_str().unwrap_or("0").parse::<u64>()?)))
            .collect::<Result<_>>()?;
        signer.multipayment(nonce, fee, &memo, &payments).map_err(|e| anyhow!(e))
    }
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
    // the lookup that follows is guaranteed to find it
    let wallet_nonce = api.wallet(&me.address)?.ok_or_else(|| anyhow!("payout wallet not found"))?.nonce as i64;
    let mut stuck = false;
    for (nonce, payout_id, slot_fee, attempts) in &slots {
        let vars = variants_of(conn, *nonce)?;
        let mut found = None;
        for (id, kind, fee, _) in &vars {
            if let Some(t) = api.tx(id)? {
                found = Some((id.clone(), kind.clone(), *fee, t.confirmations));
                break;
            }
        }
        match found {
            Some((id, kind, fee, conf)) if conf >= cfg.confirmations => {
                let tx = conn.transaction()?;
                tx.execute("UPDATE slots SET status=?1, tx_id=?2 WHERE nonce=?3", params![if kind == "pay" { "confirmed" } else { "failed" }, id, nonce])?;
                // the keep was charged slot_fee; the variant that made it cost `fee`
                db::credit(&tx, cfg.main_keep(), "keep", slot_fee - fee, 0)?;
                if kind == "pay" {
                    settle_items(&tx, cfg, *nonce, true)?;
                    info!("payout #{payout_id}: transaction {} (nonce {nonce}) confirmed", &id[..16]);
                } else {
                    settle_items(&tx, cfg, *nonce, false)?;
                    // a cancelling self-transfer costs its fee only (it sends 1 arktoshi to itself)
                    warn!("payout #{payout_id}: nonce {nonce} was cancelled; its payments go back to the ledger");
                }
                finish_payout(&tx, *payout_id)?;
                tx.commit()?;
            }
            Some(_) => {}
            None if wallet_nonce >= *nonce => {
                let tx = conn.transaction()?;
                tx.execute("UPDATE slots SET status='failed' WHERE nonce=?1", params![nonce])?;
                db::credit(&tx, cfg.main_keep(), "keep", *slot_fee, 0)?;
                settle_items(&tx, cfg, *nonce, false)?;
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

fn settle_items(tx: &Connection, cfg: &Config, nonce: i64, paid: bool) -> Result<()> {
    let items: Vec<(i64, String, i64)> = {
        let mut st = tx.prepare("SELECT payout_id, account, amount FROM payout_items WHERE slot=?1 AND status='pending'")?;
        let rows = st.query_map(params![nonce], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (pid, account, amount) in items {
        tx.execute(
            "UPDATE payout_items SET status=?1 WHERE payout_id=?2 AND account=?3",
            params![if paid { "paid" } else { "failed" }, pid, account],
        )?;
        if paid {
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

/// Consume every pending nonce of a payout with a 1-arktoshi transfer to the payout wallet
/// itself. Whichever transaction takes a nonce wins: if a payment does, it is paid; if the
/// cancel does, the payments return to the ledger. Either way nothing is paid twice.
pub fn cancel(cfg: &Config, node: &NodeInfo, api: &mut Api, conn: &mut Connection, me: &Me, signer: &Signer, payout_id: i64) -> Result<usize> {
    let nonces: Vec<i64> = {
        let mut st = conn.prepare("SELECT nonce FROM slots WHERE payout_id=?1 AND status='pending' ORDER BY nonce")?;
        let rows = st.query_map(params![payout_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if nonces.is_empty() {
        bail!("payout #{payout_id} has no pending transactions");
    }
    let fee = fee_for(node, "", 1, signer.second.is_some());
    let mut out = Vec::new();
    for n in &nonces {
        let t = signer.transfer(*n as u64, fee, "", &me.address, 1).map_err(|e| anyhow!(e))?;
        conn.execute(
            "INSERT INTO variants(tx_id,nonce,kind,fee,json,created_at) VALUES(?1,?2,'cancel',?3,?4,?5)",
            params![t.id.as_ref().unwrap(), n, fee as i64, serde_json::to_string(&t)?, db::now()],
        )?;
        out.push(serde_json::to_value(&t)?);
    }
    let res = broadcast_all(cfg, api, &out).context("broadcasting the cancel transactions")?;
    for r in &res.rejected {
        warn!("cancel transaction {} rejected: {} {}", &r.0[..16], r.1, r.2);
    }
    let _ = api;
    Ok(nonces.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const D: &str = "AU1TGCYoWzBnGMyMK9GTS8BG6EFbgc5xxC";
    const A: &str = "AKdr5d9AMEnsKYxpDcoHdyyjSCKVx3r9Nj";
    const B: &str = "AKfpFjpqUAGTjuAcwfQnVNuY7Z3tKz1RFy";
    const R: &str = "AMvXMDJorQpRqXXErDqKEfEAdjsoaYeeYW";

    fn node() -> NodeInfo {
        NodeInfo {
            pub_key_hash: 23,
            nethash: String::new(),
            multipayment_limit: 2,
            max_tx_per_request: 40,
            dynamic: true,
            per_byte: 3000,
            addon_transfer: 100,
            addon_multipayment: 500,
            static_transfer: 10_000_000,
            static_multipayment: 10_000_000,
        }
    }
    fn me() -> Me {
        Me { username: "d".into(), address: D.into(), public_key: "pk".into(), network: 23 }
    }
    fn cfg(main_keep: &str, extra: &str) -> Config {
        let t = format!(
            "[network]\napi=\"http://x/api\"\n[validator]\nname=\"d\"\n[share]\npercent=50\nkeep_address=\"{main_keep}\"\n[payout]\nevery_blocks=1\nmemo=\"m\"\n{extra}"
        );
        Config::parse(&t, Path::new("/tmp/c.toml")).unwrap()
    }

    #[test]
    fn fee_formula_matches_mainnet_example() {
        // geops' 64-recipient multipayments (memo "GEOPS", second signature) are 2050 bytes and
        // were accepted on mainnet with 4_578_000; the minimum is (500 + 1025) * 3000
        let mut n = node();
        n.multipayment_limit = 64;
        assert_eq!(tx::size_multipayment("GEOPS", 64, true), 2050);
        assert_eq!(fee_for(&n, "GEOPS", 64, true), 4_575_000);
        assert_eq!(fee_for(&n, "GEOPS", 42, true), (500 + (59 + 5 + 2 + 29 * 42 + 128 + 1) / 2) * 3000);
    }

    #[test]
    fn main_keep_pays_fees_and_min_amount_carries() {
        let c = cfg(R, "min_amount = 1");
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 300_000_000i64);
        l.insert(B.to_string(), 50_000_000i64); // below 1 ARK: carried over
        l.insert(R.to_string(), 400_000_000i64);
        let p = plan(&c, &node(), &me(), false, &l);
        assert!(p.items.iter().all(|i| i.account != B));
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments.len(), 2); // A + keep in one multipayment
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
        let p = plan(&c, &node(), &me(), false, &l);
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments, vec![(A.to_string(), 300_000_000)]); // single -> transfer
        let r = p.items.iter().find(|i| i.account == D).unwrap();
        assert!(r.tx.is_none());
        assert_eq!(r.amount as i64, 100_000_000 - p.total_fee as i64);
    }

    #[test]
    fn redirect_and_aggregation() {
        let c = cfg(D, &format!("[redirect]\n{A} = \"{B}\"\n"));
        let mut l = BTreeMap::new();
        l.insert(A.to_string(), 5i64);
        l.insert(B.to_string(), 7i64);
        l.insert(D.to_string(), 100_000_000i64);
        let p = plan(&c, &node(), &me(), false, &l);
        assert_eq!(p.txs.len(), 1);
        assert_eq!(p.txs[0].payments, vec![(B.to_string(), 12)]);
        assert_eq!(p.items.iter().filter(|i| i.tx == Some(0)).count(), 2);
    }

    #[test]
    fn chunks_respect_limit_and_negative_main_keep() {
        let c = cfg(D, "");
        let mut l = BTreeMap::new();
        for (i, a) in [A, B, R].iter().enumerate() {
            l.insert(a.to_string(), (i as i64 + 1) * 1000);
        }
        l.insert(D.to_string(), 0);
        let p = plan(&c, &node(), &me(), false, &l);
        assert_eq!(p.txs.len(), 2); // limit 2 -> multipayment of 2 + transfer of 1
        assert_eq!(p.txs.iter().map(|t| t.payments.len()).sum::<usize>(), 3);
        // the keep has nothing: the fees become keep debt, paid off by later blocks
        assert_eq!(p.main_keep_charge, p.total_fee as i64);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// `cargo test broadcast_unfunded -- --ignored --nocapture`: signs a multipayment from a
    /// throwaway, never-funded mainnet wallet and posts it. The node must reject it (no funds
    /// can move); the point is that our signed JSON and the response parsing match the node.
    #[test]
    #[ignore]
    fn broadcast_unfunded() {
        let signer = Signer { keys: crate::crypto::KeyPair::from_passphrase("ark-tbw throwaway unfunded test wallet 7f3a").unwrap(), second: None, network: 23 };
        let mut api = Api::new("https://api.ark.io/api", 0);
        let node = api.node_info().unwrap();
        let pays = vec![("AKdr5d9AMEnsKYxpDcoHdyyjSCKVx3r9Nj".to_string(), 1u64), ("AKfpFjpqUAGTjuAcwfQnVNuY7Z3tKz1RFy".to_string(), 2u64)];
        let fee = fee_for(&node, "ark-tbw test", 2, false);
        let t = signer.multipayment(1, fee, "ark-tbw test", &pays).unwrap();
        let res = api.broadcast(&[serde_json::to_value(&t).unwrap()]).unwrap();
        println!("from {} fee {fee}: {res:?}", signer.address());
        assert!(res.accepted.is_empty());
        assert_eq!(res.rejected.len(), 1);
        assert_eq!(res.rejected[0].0, *t.id.as_ref().unwrap());
    }
}
