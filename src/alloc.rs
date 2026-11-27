//! Splitting one proposed block between voters and the keep. Pure integer arithmetic:
//! the parts always add up to exactly reward + fee, and the keep takes the rounding dust.

use crate::config::{Config, Ineligible, BPS};

#[derive(Clone, Debug)]
pub struct VoterIn {
    pub address: String,
    /// Voting weight: the balance in units of 1e-8 ARK (wei / 1e10), as the consensus contract counts it.
    pub weight: u64,
    /// Rewards owed to this voter and not paid out yet (used for anti-dilution).
    pub pending: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    Eligible,
    Excluded,
    BelowMinimum,
    OwnWallet,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // `why` is kept for diagnostics and tests
pub struct VoterOut {
    pub address: String,
    pub weight: u64,
    pub counted: u64,
    pub amount: u64,
    pub why: Why,
}

#[derive(Clone, Debug)]
pub struct BlockSplit {
    pub voters: Vec<VoterOut>,
    /// the keep (a single entry)
    pub keep: Vec<(String, u64)>,
    pub total_weight: u64,
    pub to_voters: u64,
}

/// `holder` is the wallet that holds undistributed rewards (the payout wallet) together with
/// the amount it holds for others: when that wallet votes, those funds are not its own stake.
pub fn split(cfg: &Config, validator_address: &str, reward: u64, fee: u64, voters: &[VoterIn], holder: Option<(&str, u64)>) -> BlockSplit {
    let base = reward as u128 + if cfg.share_fees { fee as u128 } else { 0 };
    let mut outs: Vec<VoterOut> = Vec::with_capacity(voters.len());
    let mut sorted: Vec<&VoterIn> = voters.iter().collect();
    sorted.sort_by(|a, b| a.address.cmp(&b.address));

    for v in sorted {
        let mut own = v.weight;
        if let Some((h, held)) = holder {
            if h == v.address {
                own = own.saturating_sub(held);
            }
        }
        let why = if v.address == validator_address && !cfg.include_self {
            Why::OwnWallet
        } else if cfg.blacklist.contains(&v.address) || (!cfg.whitelist.is_empty() && !cfg.whitelist.contains(&v.address)) {
            Why::Excluded
        } else if own == 0 || own < cfg.min_vote {
            Why::BelowMinimum
        } else {
            Why::Eligible
        };
        let full = own.saturating_add(if cfg.anti_dilution && why == Why::Eligible { v.pending } else { 0 });
        let counted = if why != Why::Eligible {
            0
        } else if cfg.max_vote > 0 {
            full.min(cfg.max_vote)
        } else {
            full
        };
        // in "keep" mode the total also holds what is not counted; remember it in `weight`
        outs.push(VoterOut { address: v.address.clone(), weight: full, counted, amount: 0, why });
    }

    let total: u128 = match cfg.ineligible {
        Ineligible::Redistribute => outs.iter().map(|o| o.counted as u128).sum(),
        Ineligible::Keep => outs.iter().map(|o| o.weight as u128).sum(),
    };

    let mut to_voters: u128 = 0;
    if total > 0 {
        for o in outs.iter_mut().filter(|o| o.counted > 0) {
            let bps = *cfg.custom.get(&o.address).unwrap_or(&cfg.voter_bps) as u128;
            let a = base * bps * o.counted as u128 / (total * BPS as u128);
            o.amount = a as u64;
            to_voters += a;
        }
    }

    let mut keep = Vec::with_capacity(cfg.keep.len());
    let mut kept: u128 = 0;
    for k in cfg.keep.iter().skip(1) {
        let a = base * k.bps as u128 / BPS as u128;
        kept += a;
        keep.push((k.address.clone(), a as u64));
    }
    let all = reward as u128 + fee as u128;
    // config validation guarantees this never goes negative (custom rates are at most 100%)
    let main_keep = all.checked_sub(to_voters + kept).expect("block split exceeds the block total");
    keep.insert(0, (cfg.keep[0].address.clone(), main_keep as u64));

    BlockSplit { voters: outs, keep, total_weight: total.min(u64::MAX as u128) as u64, to_voters: to_voters as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ARKTOSHI};
    use std::path::Path;

    const D: &str = "0x7783464d965A1E6a13B19b8222C56C53d32414ad"; // validator + main_keep
    const A: &str = "0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22";
    const B: &str = "0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763";
    const C: &str = "0xe5a97E663158dEaF3b65bBF88897b8359Dc19F81";
    #[allow(dead_code)]
    const K: &str = "0x00EFd0D4639191C49908A7BddbB9A11A994A8527";

    fn cfg(extra_share: &str, keep: &str) -> Config {
        let t = format!(
            "[network]\napi=\"http://x/api\"\n[validator]\nname=\"d\"\n[share]\npercent=50\n{keep}\n{extra_share}\n[payout]\nevery_blocks=1\n"
        );
        Config::parse(&t, Path::new("/tmp/c.toml")).unwrap()
    }
    fn main_keep_only() -> String {
        format!("keep_address=\"{D}\"")
    }
    fn v(a: &str, ark: u64) -> VoterIn {
        VoterIn { address: a.into(), weight: ark * ARKTOSHI, pending: 0 }
    }
    fn sum(s: &BlockSplit) -> u64 {
        s.voters.iter().map(|o| o.amount).sum::<u64>() + s.keep.iter().map(|k| k.1).sum::<u64>()
    }
    fn amt(s: &BlockSplit, a: &str) -> u64 {
        s.voters.iter().find(|o| o.address == a).unwrap().amount
    }

    #[test]
    fn proportional_and_exact_total() {
        let c = cfg("", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 12345, &[v(A, 100), v(B, 300)], None);
        assert_eq!(amt(&s, A), 25_000_000);
        assert_eq!(amt(&s, B), 75_000_000);
        assert_eq!(s.keep[0].1, ARKTOSHI + 12345); // fees not shared -> main_keep
        assert_eq!(sum(&s), 2 * ARKTOSHI + 12345);
    }

    #[test]
    fn rounding_dust_goes_to_keep() {
        let c = cfg("", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 1), v(B, 1), v(C, 1)], None);
        assert_eq!(amt(&s, A), 33_333_333);
        assert_eq!(sum(&s), 2 * ARKTOSHI);
        assert_eq!(s.keep[0].1, 2 * ARKTOSHI - 3 * 33_333_333);
    }

    #[test]
    fn shares_fees_when_asked() {
        let c = cfg("fees=true", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 1000, &[v(A, 1)], None);
        assert_eq!(amt(&s, A), ARKTOSHI + 500);
        assert_eq!(sum(&s), 2 * ARKTOSHI + 1000);
    }

    #[test]
    fn minimum_is_inclusive_and_cap_redistributes() {
        let c = cfg("min_vote=10\nmax_vote=100", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 10), v(B, 9), v(C, 1000)], None);
        assert_eq!(s.voters.iter().find(|o| o.address == B).unwrap().why, Why::BelowMinimum);
        assert_eq!(amt(&s, B), 0);
        // A counts 10, C is capped to 100 -> 10/110 and 100/110 of 1 ARK
        assert_eq!(amt(&s, A), 9_090_909);
        assert_eq!(amt(&s, C), 90_909_090);
        assert_eq!(s.total_weight, 110 * ARKTOSHI);
        assert_eq!(sum(&s), 2 * ARKTOSHI);
    }

    #[test]
    fn keep_mode_sends_uncounted_part_to_keep() {
        let c = cfg("min_vote=10\nmax_vote=100\nineligible=\"keep\"", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 10), v(B, 5), v(C, 185)], None);
        // total = 200 (everything), A gets 10/200, C gets 100/200, B nothing
        assert_eq!(amt(&s, A), 5_000_000);
        assert_eq!(amt(&s, C), 50_000_000);
        assert_eq!(amt(&s, B), 0);
        assert_eq!(s.keep[0].1, 2 * ARKTOSHI - 55_000_000);
    }

    #[test]
    fn below_minimum_never_paid_even_with_pending() {
        // the original script added unpaid rewards after the minimum check, so a voter who
        // dropped below the minimum kept earning on their unpaid balance
        let c = cfg("min_vote=10", &main_keep_only());
        let mut b = v(B, 5);
        b.pending = 50 * ARKTOSHI;
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 10), b], None);
        assert_eq!(amt(&s, B), 0);
        assert_eq!(amt(&s, A), ARKTOSHI);
    }

    #[test]
    fn anti_dilution_respects_cap() {
        let c = cfg("max_vote=100", &main_keep_only());
        let mut a = v(A, 90);
        a.pending = 30 * ARKTOSHI;
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[a, v(B, 100)], None);
        assert_eq!(s.voters.iter().find(|o| o.address == A).unwrap().counted, 100 * ARKTOSHI);
        assert_eq!(amt(&s, A), amt(&s, B));
    }

    #[test]
    fn own_wallet_excluded_by_default_and_held_funds_not_counted() {
        let c = cfg("", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 100), v(D, 100)], None);
        assert_eq!(amt(&s, D), 0);
        assert_eq!(amt(&s, A), ARKTOSHI);
        let c = cfg("include_self=true", &main_keep_only());
        // the validator wallet holds 60 ARK owed to others: only 40 is its own stake
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 40), v(D, 100)], Some((D, 60 * ARKTOSHI)));
        assert_eq!(amt(&s, A), amt(&s, D));
    }

    #[test]
    fn blacklist_whitelist() {
        let c = cfg(&format!("blacklist=[\"{B}\"]"), &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 1), v(B, 1)], None);
        assert_eq!(amt(&s, A), ARKTOSHI);
        let c = cfg(&format!("whitelist=[\"{B}\"]"), &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 1), v(B, 1)], None);
        assert_eq!(amt(&s, A), 0);
        assert_eq!(amt(&s, B), ARKTOSHI);
    }

    #[test]
    fn custom_rate_difference_goes_to_keep() {
        let c = cfg(&format!("[share.custom]\n\"{A}\" = 75\n\"{C}\" = 0"), &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[v(A, 1), v(B, 1), v(C, 2)], None);
        assert_eq!(amt(&s, A), 37_500_000); // 75% instead of 50% of its quarter
        assert_eq!(amt(&s, B), 25_000_000);
        assert_eq!(amt(&s, C), 0);
        assert_eq!(s.keep[0].1, 2 * ARKTOSHI - 37_500_000 - 25_000_000);
        assert_eq!(sum(&s), 2 * ARKTOSHI);
    }

    #[test]
    fn no_eligible_voters_all_to_keep() {
        let c = cfg("min_vote=1000", &main_keep_only());
        let s = split(&c, D, 2 * ARKTOSHI, 7, &[v(A, 1)], None);
        assert_eq!(s.keep[0].1, 2 * ARKTOSHI + 7);
        let s = split(&c, D, 2 * ARKTOSHI, 7, &[], None);
        assert_eq!(s.keep[0].1, 2 * ARKTOSHI + 7);
    }

    #[test]
    fn huge_balances_no_overflow() {
        let c = cfg("", &main_keep_only());
        let big = VoterIn { address: A.into(), weight: 20_000_000_000_000_000, pending: 0 };
        let s = split(&c, D, 2 * ARKTOSHI, 0, &[big, v(B, 1)], None);
        assert_eq!(sum(&s), 2 * ARKTOSHI);
        assert!(amt(&s, A) > 99_999_000);
    }
}
