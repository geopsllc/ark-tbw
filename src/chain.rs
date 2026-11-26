//! How a confirmed transaction or block changes a wallet's balance and vote. These rules
//! mirror Core v3's handlers for the transaction types live on ARK mainnet.

use crate::api::Tx;
use anyhow::Result;

pub type HtlcLookup<'a> = dyn FnMut(&str) -> Result<(String, String, u64)> + 'a;

/// (balance change, HTLC-locked balance change) of `addr` caused by `tx`.
pub fn delta(tx: &Tx, addr: &str, htlc: &mut HtlcLookup) -> Result<(i64, i64)> {
    let mut bal: i64 = 0;
    let mut locked: i64 = 0;
    let sender = tx.sender == addr;
    if sender {
        bal -= tx.fee as i64;
    }
    if tx.type_group == 1 {
        match tx.tx_type {
            0 => {
                if sender {
                    bal -= tx.amount as i64;
                }
                if tx.recipient.as_deref() == Some(addr) {
                    bal += tx.amount as i64;
                }
            }
            6 => {
                for (r, a) in &tx.payments {
                    if sender {
                        bal -= *a as i64;
                    }
                    if r == addr {
                        bal += *a as i64;
                    }
                }
            }
            8 => {
                // HTLC lock: the amount leaves the balance but still counts as vote weight
                if sender {
                    bal -= tx.amount as i64;
                    locked += tx.amount as i64;
                }
            }
            9 | 10 => {
                if let Some(lock_id) = &tx.lock_tx {
                    let (lock_sender, lock_recipient, amount) = htlc(lock_id)?;
                    if lock_sender == addr {
                        locked -= amount as i64;
                        if tx.tx_type == 10 {
                            bal += amount as i64;
                        }
                    }
                    if tx.tx_type == 9 && lock_recipient == addr {
                        bal += amount as i64;
                    }
                }
            }
            _ => {}
        }
    }
    Ok((bal, locked))
}

/// Does this transaction move the sender's vote to / away from `validator_pk`?
/// Returns the sender's vote status for the validator after the transaction, if it changed.
pub fn vote_change(tx: &Tx, validator_pk: &str) -> Option<bool> {
    if tx.type_group != 1 || tx.tx_type != 3 {
        return None;
    }
    let mut out = None;
    for v in &tx.votes {
        if v.len() > 1 && &v[1..] == validator_pk {
            out = Some(v.starts_with('+'));
        }
    }
    out
}

/// Every address whose balance this transaction can change.
pub fn touched(tx: &Tx) -> Vec<&str> {
    let mut v = vec![tx.sender.as_str()];
    if let Some(r) = &tx.recipient {
        v.push(r);
    }
    for (r, _) in &tx.payments {
        v.push(r);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(t: u16, sender: &str) -> Tx {
        Tx {
            id: "x".into(),
            block_id: "b".into(),
            ts: 1,
            type_group: 1,
            tx_type: t,
            sender: sender.into(),
            sender_pk: "pk".into(),
            recipient: None,
            amount: 0,
            fee: 10,
            nonce: 1,
            payments: vec![],
            votes: vec![],
            lock_tx: None,
            confirmations: 1,
        }
    }
    fn no_htlc() -> impl FnMut(&str) -> Result<(String, String, u64)> {
        |_| panic!("no htlc expected")
    }

    #[test]
    fn transfer_and_self_transfer() {
        let mut t = tx(0, "A");
        t.recipient = Some("B".into());
        t.amount = 100;
        assert_eq!(delta(&t, "A", &mut no_htlc()).unwrap(), (-110, 0));
        assert_eq!(delta(&t, "B", &mut no_htlc()).unwrap(), (100, 0));
        assert_eq!(delta(&t, "C", &mut no_htlc()).unwrap(), (0, 0));
        t.recipient = Some("A".into());
        assert_eq!(delta(&t, "A", &mut no_htlc()).unwrap(), (-10, 0));
    }

    #[test]
    fn multipayment_with_repeated_recipient() {
        let mut t = tx(6, "A");
        t.payments = vec![("B".into(), 5), ("C".into(), 7), ("B".into(), 1), ("A".into(), 2)];
        assert_eq!(delta(&t, "A", &mut no_htlc()).unwrap(), (-10 - 15 + 2, 0));
        assert_eq!(delta(&t, "B", &mut no_htlc()).unwrap(), (6, 0));
    }

    #[test]
    fn fee_only_types_and_other_groups() {
        for ty in [1u16, 2, 3, 4, 5, 7] {
            assert_eq!(delta(&tx(ty, "A"), "A", &mut no_htlc()).unwrap(), (-10, 0));
        }
        let mut t = tx(0, "A");
        t.type_group = 2;
        t.amount = 999;
        assert_eq!(delta(&t, "A", &mut no_htlc()).unwrap(), (-10, 0));
    }

    #[test]
    fn htlc_lock_claim_refund() {
        let mut lock = tx(8, "A");
        lock.amount = 50;
        lock.recipient = Some("B".into());
        assert_eq!(delta(&lock, "A", &mut no_htlc()).unwrap(), (-60, 50));
        assert_eq!(delta(&lock, "B", &mut no_htlc()).unwrap(), (0, 0));
        let mut look = |_: &str| Ok(("A".to_string(), "B".to_string(), 50u64));
        let mut claim = tx(9, "B");
        claim.fee = 0;
        claim.lock_tx = Some("L".into());
        assert_eq!(delta(&claim, "A", &mut look).unwrap(), (0, -50));
        assert_eq!(delta(&claim, "B", &mut look).unwrap(), (50, 0));
        let mut refund = tx(10, "A");
        refund.fee = 0;
        refund.lock_tx = Some("L".into());
        assert_eq!(delta(&refund, "A", &mut look).unwrap(), (50, -50));
    }

    #[test]
    fn votes() {
        let mut t = tx(3, "A");
        t.votes = vec!["+us".into()];
        assert_eq!(vote_change(&t, "us"), Some(true));
        t.votes = vec!["-us".into(), "+other".into()];
        assert_eq!(vote_change(&t, "us"), Some(false));
        t.votes = vec!["-other".into(), "+us".into()];
        assert_eq!(vote_change(&t, "us"), Some(true));
        t.votes = vec!["-x".into(), "+y".into()];
        assert_eq!(vote_change(&t, "us"), None);
        assert_eq!(vote_change(&tx(0, "A"), "us"), None);
    }
}
