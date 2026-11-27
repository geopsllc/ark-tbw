//! How a confirmed transaction or block changes a wallet's balance and vote on Mainsail.
//! Balances are wei (i128 deltas). The rules follow the node:
//!
//! - The sender always pays gasUsed x gasPrice, even when the call reverts (receipt status 0).
//! - Value moves only when the call succeeds: sender -value, recipient +value.
//! - Multipayment: the contract forwards each amount and emits Payment(recipient, amount, success);
//!   failed payments are refunded to the sender in the same call.
//! - Validator resignation refunds the registration fee (ValidatorResigned(addr)).
//! - Votes: Voted(voter, validator) / Unvoted(voter, validator) events of the consensus contract,
//!   whoever made the call (a contract voting for itself emits them too).
//! - The block proposer receives the block reward plus the block's gas fees.
//!
//! Native value that a third-party contract moves internally is not visible in any of this;
//! the chain follower checks balances against the node's own state to cover it.

use crate::api::{Log, Tx};
use crate::evm::{self, data_word, topic, word_to_address, word_to_u128};
use std::sync::OnceLock;

struct Topics {
    voted: String,
    unvoted: String,
    payment: String,
    resigned: String,
    registered: String,
}

fn topics() -> &'static Topics {
    static T: OnceLock<Topics> = OnceLock::new();
    T.get_or_init(|| Topics {
        voted: topic("Voted(address,address)"),
        unvoted: topic("Unvoted(address,address)"),
        payment: topic("Payment(address,uint256,bool)"),
        resigned: topic("ValidatorResigned(address)"),
        registered: topic("ValidatorRegistered(address,bytes)"),
    })
}

/// Successful multipayment transfers in a transaction: (recipient, amount wei).
pub fn payments(tx: &Tx) -> Vec<(String, u128, bool)> {
    if !tx.ok || tx.to.as_deref() != Some(evm::MULTIPAYMENT) {
        return Vec::new();
    }
    tx.logs
        .iter()
        .filter(|l| l.address == evm::MULTIPAYMENT && l.topics.first() == Some(&topics().payment))
        .filter_map(|l| {
            let to = word_to_address(l.topics.get(1)?)?;
            let amount = word_to_u128(data_word(&l.data, 0)?)?;
            let ok = word_to_u128(data_word(&l.data, 1)?)? != 0;
            Some((to, amount, ok))
        })
        .collect()
}

fn consensus_logs(tx: &Tx) -> impl Iterator<Item = &Log> {
    tx.logs.iter().filter(move |l| tx.ok && l.address == evm::CONSENSUS)
}

/// Validators that resigned in this transaction (their registration fee comes back).
pub fn resignations(tx: &Tx) -> Vec<String> {
    consensus_logs(tx).filter(|l| l.topics.first() == Some(&topics().resigned)).filter_map(|l| word_to_address(data_word(&l.data, 0)?)).collect()
}

/// Wallets that became validators in this transaction (ValidatorRegistered(addr, blsKey)).
pub fn registrations(tx: &Tx) -> Vec<String> {
    consensus_logs(tx).filter(|l| l.topics.first() == Some(&topics().registered)).filter_map(|l| word_to_address(data_word(&l.data, 0)?)).collect()
}

/// Balance change (wei) of `addr` caused by `tx`. `refund(a)` is the registration fee a
/// resigning validator gets back.
pub fn delta(tx: &Tx, addr: &str, refund: &mut dyn FnMut(&str) -> u128) -> i128 {
    let mut d: i128 = 0;
    let sender = tx.from == addr;
    if sender {
        d -= tx.fee() as i128;
    }
    if !tx.ok {
        return d;
    }
    if sender {
        d -= tx.value as i128;
    }
    if tx.to.as_deref() == Some(addr) {
        d += tx.value as i128;
    }
    let pays = payments(tx);
    if !pays.is_empty() {
        let sent: u128 = pays.iter().filter(|p| p.2).map(|p| p.1).sum();
        if sender {
            d += tx.value as i128 - sent as i128; // refund of failed payments
        }
        for (to, amount, ok) in &pays {
            if *ok && to == addr {
                d += *amount as i128;
            }
        }
    }
    for r in resignations(tx) {
        if r == addr {
            d += refund(addr) as i128;
        }
    }
    d
}

/// Vote changes for `validator` in this transaction, in order: (voter, now voting?).
pub fn vote_changes(tx: &Tx, validator: &str) -> Vec<(String, bool)> {
    let t = topics();
    let mut out = Vec::new();
    for l in consensus_logs(tx) {
        let on = match l.topics.first() {
            Some(x) if *x == t.voted => true,
            Some(x) if *x == t.unvoted => false,
            _ => continue,
        };
        let (Some(voter), Some(v)) = (data_word(&l.data, 0).and_then(word_to_address), data_word(&l.data, 1).and_then(word_to_address)) else { continue };
        if v == validator {
            out.push((voter, on));
        }
    }
    out
}

/// The last vote change for `validator` made by `voter` in this transaction, if any.
pub fn vote_change(tx: &Tx, validator: &str, voter: &str) -> Option<bool> {
    vote_changes(tx, validator).into_iter().filter(|(a, _)| a == voter).last().map(|x| x.1)
}

/// Every address whose balance this transaction can change.
pub fn touched(tx: &Tx) -> Vec<String> {
    let mut v = vec![tx.from.clone()];
    if let Some(r) = &tx.to {
        v.push(r.clone());
    }
    for (r, _, _) in payments(tx) {
        v.push(r);
    }
    v.extend(resignations(tx));
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0x7783464d965A1E6a13B19b8222C56C53d32414ad";
    const B: &str = "0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22";
    const C: &str = "0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763";

    fn word(a: &str) -> String {
        format!("{:0>64}", a.trim_start_matches("0x").to_lowercase())
    }
    fn num(n: u128) -> String {
        format!("{n:064x}")
    }

    fn tx(from: &str, to: Option<&str>, value: u128, ok: bool) -> Tx {
        Tx {
            id: "h".into(),
            block_id: "b".into(),
            ts: 1,
            from: from.into(),
            sender_pk: String::new(),
            to: to.map(str::to_string),
            value,
            gas_price: 5,
            gas_used: 21_000,
            ok,
            nonce: 0,
            data: String::new(),
            logs: vec![],
            confirmations: 1,
        }
    }
    fn no_refund() -> impl FnMut(&str) -> u128 {
        |_| panic!("no refund expected")
    }

    #[test]
    fn transfer_failed_call_and_self_transfer() {
        let t = tx(A, Some(B), 1000, true);
        assert_eq!(delta(&t, A, &mut no_refund()), -1000 - 105_000);
        assert_eq!(delta(&t, B, &mut no_refund()), 1000);
        assert_eq!(delta(&t, C, &mut no_refund()), 0);
        let f = tx(A, Some(B), 1000, false);
        assert_eq!(delta(&f, A, &mut no_refund()), -105_000); // reverted: gas only
        assert_eq!(delta(&f, B, &mut no_refund()), 0);
        let s = tx(A, Some(A), 1000, true);
        assert_eq!(delta(&s, A, &mut no_refund()), -105_000);
        let create = tx(A, None, 7, true);
        assert_eq!(delta(&create, A, &mut no_refund()), -7 - 105_000);
    }

    #[test]
    fn multipayment_with_failed_payment_refunded() {
        let mut t = tx(A, Some(evm::MULTIPAYMENT), 600, true);
        let p = topics().payment.clone();
        t.logs = vec![
            Log { address: evm::MULTIPAYMENT.into(), topics: vec![p.clone(), format!("0x{}", word(B))], data: format!("0x{}{}", num(100), num(1)) },
            Log { address: evm::MULTIPAYMENT.into(), topics: vec![p.clone(), format!("0x{}", word(C))], data: format!("0x{}{}", num(200), num(0)) },
            Log { address: evm::MULTIPAYMENT.into(), topics: vec![p.clone(), format!("0x{}", word(B))], data: format!("0x{}{}", num(300), num(1)) },
        ];
        assert_eq!(delta(&t, B, &mut no_refund()), 400);
        assert_eq!(delta(&t, C, &mut no_refund()), 0); // failed: refunded to A
        assert_eq!(delta(&t, A, &mut no_refund()), -400 - 105_000);
        assert_eq!(touched(&t), {
            let mut v = vec![A.to_string(), B.to_string(), C.to_string(), evm::MULTIPAYMENT.to_string()];
            v.sort();
            v
        });
        // the same logs from another contract are not multipayments
        let mut fake = t.clone();
        fake.to = Some(C.into());
        assert_eq!(delta(&fake, B, &mut no_refund()), 0);
    }

    #[test]
    fn votes_and_resignation() {
        let mut t = tx(A, Some(evm::CONSENSUS), 0, true);
        t.logs = vec![
            Log { address: evm::CONSENSUS.into(), topics: vec![topics().unvoted.clone()], data: format!("0x{}{}", word(A), word(C)) },
            Log { address: evm::CONSENSUS.into(), topics: vec![topics().voted.clone()], data: format!("0x{}{}", word(A), word(B)) },
        ];
        assert_eq!(vote_change(&t, B, A), Some(true));
        assert_eq!(vote_change(&t, C, A), Some(false));
        assert_eq!(vote_change(&t, A, A), None);
        let mut reverted = t.clone();
        reverted.ok = false;
        assert_eq!(vote_change(&reverted, B, A), None);
        let mut r = tx(A, Some(evm::CONSENSUS), 0, true);
        r.logs = vec![Log { address: evm::CONSENSUS.into(), topics: vec![topics().resigned.clone()], data: format!("0x{}", word(A)) }];
        assert_eq!(delta(&r, A, &mut |_| 250), 250 - 105_000);
        assert!(registrations(&r).is_empty());
        let mut g = tx(A, Some(evm::CONSENSUS), 250, true);
        g.logs = vec![Log { address: evm::CONSENSUS.into(), topics: vec![topics().registered.clone()], data: format!("0x{}{}", word(A), num(64)) }];
        assert_eq!(registrations(&g), vec![A.to_string()]);
    }
}
