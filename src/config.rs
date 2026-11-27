//! Configuration file (TOML). Every rule is checked at load time, so a bad config never
//! reaches the ledger. Amounts are parsed exactly (no floats) and percentages are kept in
//! basis points (1% = 100 bps).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const BPS: u64 = 10_000;
pub const ARKTOSHI: u64 = 100_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    network: RawNetwork,
    validator: RawValidator,
    share: RawShare,
    payout: RawPayout,
    #[serde(default)]
    redirect: BTreeMap<String, String>,
    #[serde(default)]
    storage: RawStorage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    api: String,
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    tx_api: Option<String>,
    #[serde(default)]
    broadcast: Vec<String>,
    #[serde(default)]
    requests_per_minute: u32,
    #[serde(default = "default_rpc_rate")]
    rpc_requests_per_minute: u32,
    #[serde(default = "default_confirmations")]
    confirmations: u64,
    #[serde(default = "default_poll")]
    poll_seconds: u64,
}
fn default_confirmations() -> u64 {
    3
}
fn default_poll() -> u64 {
    15
}
fn default_rpc_rate() -> u32 {
    120
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawValidator {
    name: String,
    #[serde(default)]
    start_height: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShare {
    percent: toml::Value,
    #[serde(default)]
    fees: bool,
    #[serde(default)]
    min_vote: Option<toml::Value>,
    #[serde(default)]
    max_vote: Option<toml::Value>,
    #[serde(default = "yes")]
    anti_dilution: bool,
    #[serde(default)]
    include_self: bool,
    #[serde(default = "default_ineligible")]
    ineligible: String,
    #[serde(default)]
    blacklist: Vec<String>,
    #[serde(default)]
    whitelist: Vec<String>,
    #[serde(default)]
    custom: BTreeMap<String, toml::Value>,
    /// where the keep (100% - share) is paid; default: it stays in the validator wallet
    #[serde(default)]
    keep_address: Option<String>,
}
fn yes() -> bool {
    true
}
fn default_ineligible() -> String {
    "redistribute".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayout {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    every_blocks: u64,
    #[serde(default)]
    at: Vec<String>,
    #[serde(default)]
    min_amount: Option<toml::Value>,
    #[serde(default)]
    memo: String,
    #[serde(default)]
    keep_memo: Option<String>,
    #[serde(default = "yes")]
    multipayment: bool,
    #[serde(default)]
    max_fee: Option<toml::Value>,
    /// gwei; default: the network minimum
    #[serde(default)]
    gas_price: Option<toml::Value>,
    #[serde(default)]
    secret_file: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    #[serde(default)]
    data_dir: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// Weight of excluded / below-minimum voters and stake above the cap is left out of the
    /// total, so eligible voters split the whole voter share.
    Redistribute,
    /// That weight stays in the total; the part it would have earned goes to the keep.
    Keep,
}

#[derive(Clone, Debug)]
pub struct Keep {
    pub address: String,
    pub bps: u64,
    pub memo: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub path: PathBuf,
    pub api: String,
    pub rpc: String,
    pub tx_api: String,
    pub broadcast: Vec<String>,
    pub requests_per_minute: u32,
    pub rpc_requests_per_minute: u32,
    pub confirmations: u64,
    pub poll_seconds: u64,

    pub validator: String,
    pub start_height: u64,

    pub voter_bps: u64,
    pub share_fees: bool,
    pub min_vote: u64,
    pub max_vote: u64,
    pub anti_dilution: bool,
    pub include_self: bool,
    pub ineligible: Ineligible,
    pub blacklist: BTreeSet<String>,
    pub whitelist: BTreeSet<String>,
    pub custom: BTreeMap<String, u64>,
    /// The keep: 100% - share. It receives rounding dust, fees that are not shared and the
    /// difference left by custom rates, and it pays the transaction fees.
    pub keep: Vec<Keep>,
    pub redirect: BTreeMap<String, String>,

    pub payout_enabled: bool,
    pub every_blocks: u64,
    pub at: Vec<(u32, u32)>,
    pub min_amount: u64,
    pub memo: String,
    pub multipayment: bool,
    /// highest fee one transaction may cost (units of 1e-8 ARK)
    pub max_fee: u64,
    /// wei per gas; 0 = the network minimum
    pub gas_price: u128,
    pub secret_file: PathBuf,

    pub data_dir: PathBuf,
}

/// Parse a decimal ARK amount ("10", "0.5", 12.25) into arktoshi, exactly.
pub fn parse_ark(v: &toml::Value) -> Result<u64> {
    let s = match v {
        toml::Value::String(s) => s.trim().to_string(),
        toml::Value::Integer(i) if *i >= 0 => i.to_string(),
        toml::Value::Float(f) if *f >= 0.0 && f.is_finite() => format!("{f}"),
        _ => bail!("not a non-negative ARK amount: {v}"),
    };
    parse_decimal(&s, 8).with_context(|| format!("bad ARK amount {s:?}"))
}

/// Parse a percentage (50, 85.5, "99.25") into basis points. At most two decimals.
pub fn parse_percent(v: &toml::Value) -> Result<u64> {
    let s = match v {
        toml::Value::String(s) => s.trim().to_string(),
        toml::Value::Integer(i) if *i >= 0 => i.to_string(),
        toml::Value::Float(f) if *f >= 0.0 && f.is_finite() => format!("{f}"),
        _ => bail!("not a percentage: {v}"),
    };
    let bps = parse_decimal(&s, 2).with_context(|| format!("bad percentage {s:?} (at most 2 decimals)"))?;
    if bps > BPS {
        bail!("percentage {s} is above 100");
    }
    Ok(bps)
}

fn parse_decimal(s: &str, decimals: u32) -> Result<u64> {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int.is_empty() && frac.is_empty() || !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("not a number");
    }
    let frac = frac.trim_end_matches('0');
    if frac.len() as u32 > decimals {
        bail!("too many decimals");
    }
    let scale = 10u64.pow(decimals);
    let i: u64 = if int.is_empty() { 0 } else { int.parse()? };
    let f: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = decimals as usize).parse()? };
    i.checked_mul(scale).and_then(|x| x.checked_add(f)).ok_or_else(|| anyhow!("number too large"))
}

pub fn fmt_ark(arktoshi: i128) -> String {
    let neg = arktoshi < 0;
    let a = arktoshi.unsigned_abs();
    let s = format!("{}{}.{:08}", if neg { "-" } else { "" }, a / ARKTOSHI as u128, a % ARKTOSHI as u128);
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

/// Name of the coin in messages: "TARK" on the Mainsail testnet (chain id 11812), else "ARK".
static SYMBOL: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

pub fn set_chain(chain_id: u64) {
    let _ = SYMBOL.set(if chain_id == 11812 { "TARK" } else { "ARK" });
}

pub fn sym() -> &'static str {
    SYMBOL.get().copied().unwrap_or("ARK")
}

pub fn fmt_pct(bps: u64) -> String {
    if bps % 100 == 0 {
        format!("{}%", bps / 100)
    } else {
        format!("{}.{:02}%", bps / 100, bps % 100).replace("0%", "%")
    }
}

/// A 0x address in checksum case. Mixed case must match its EIP-55 checksum (catches typos).
fn check_address(a: &str, what: &str) -> Result<String> {
    let a = a.trim();
    match crate::evm::parse_address(a) {
        Ok(raw) => Ok(crate::evm::checksum(&raw)),
        Err(e) if a.starts_with('A') || a.starts_with('D') => bail!("{what}: {a:?} is an ARK Core v3 address; Mainsail uses 0x addresses ({e})"),
        Err(e) => bail!("{what}: {a:?} is not a valid 0x address: {e}"),
    }
}

pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ark-tbw").join("config.toml")
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read config {}", path.display()))?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let raw: Raw = toml::from_str(text).context("config file")?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let resolve = |p: &str| -> PathBuf {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                dir.join(p)
            }
        };

        // network
        let api = raw.network.api.trim().trim_end_matches('/').to_string();
        if !(api.starts_with("http://") || api.starts_with("https://")) {
            bail!("network.api must start with http:// or https://");
        }
        let defaults = crate::api::default_urls(&api);
        let url = |v: &Option<String>, i: usize, what: &str| -> Result<String> {
            let u = match v {
                Some(u) => u.trim().trim_end_matches('/').to_string(),
                None => match &defaults {
                    Some(d) => if i == 0 { d.0.clone() } else { d.1.clone() },
                    None => bail!("set network.{what}: it cannot be derived from network.api"),
                },
            };
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                bail!("network.{what} must start with http:// or https://");
            }
            Ok(u)
        };
        let rpc = url(&raw.network.rpc, 0, "rpc")?;
        let tx_api = url(&raw.network.tx_api, 1, "tx_api")?;
        let broadcast: Vec<String> = raw.network.broadcast.iter().map(|s| s.trim().trim_end_matches('/').to_string()).collect();
        for b in &broadcast {
            if !(b.starts_with("http://") || b.starts_with("https://")) {
                bail!("network.broadcast entries (transaction pool APIs) must start with http:// or https://");
            }
        }
        if raw.network.confirmations == 0 || raw.network.confirmations > 1000 {
            bail!("network.confirmations must be between 1 and 1000");
        }
        if raw.network.poll_seconds < 5 {
            bail!("network.poll_seconds must be at least 5");
        }

        // share
        let s = raw.share;
        let voter_bps = parse_percent(&s.percent).context("share.percent")?;
        let min_vote = match &s.min_vote {
            Some(v) => parse_ark(v).context("share.min_vote")?,
            None => 0,
        };
        let max_vote = match &s.max_vote {
            Some(v) => parse_ark(v).context("share.max_vote")?,
            None => 0,
        };
        if max_vote > 0 && max_vote < min_vote {
            bail!("share.max_vote is below share.min_vote");
        }
        let ineligible = match s.ineligible.as_str() {
            "redistribute" => Ineligible::Redistribute,
            "keep" => Ineligible::Keep,
            o => bail!("share.ineligible must be \"redistribute\" or \"keep\", not {o:?}"),
        };
        let blacklist: BTreeSet<String> = s.blacklist.iter().map(|a| check_address(a, "share.blacklist")).collect::<Result<_>>()?;
        let whitelist: BTreeSet<String> = s.whitelist.iter().map(|a| check_address(a, "share.whitelist")).collect::<Result<_>>()?;
        let mut custom = BTreeMap::new();
        for (a, p) in &s.custom {
            let a = check_address(a, "share.custom")?;
            custom.insert(a.clone(), parse_percent(p).with_context(|| format!("share.custom {a}"))?);
        }

        // the keep is whatever the voters do not get
        let keep_address = match &s.keep_address {
            Some(a) if !a.trim().is_empty() => check_address(a, "share.keep_address")?,
            _ => String::new(),
        };
        let keep = vec![Keep { address: keep_address, bps: BPS - voter_bps, memo: raw.payout.keep_memo.clone().unwrap_or_default() }];

        // redirects
        let mut redirect = BTreeMap::new();
        for (from, to) in &raw.redirect {
            let from = check_address(from, "redirect")?;
            let to = check_address(to, "redirect target")?;
            if from == to {
                continue;
            }
            redirect.insert(from, to);
        }
        for (from, to) in &redirect {
            if redirect.contains_key(to) {
                bail!("redirect chain: {from} -> {to} -> {}; point {from} straight to the final address", redirect[to]);
            }
        }

        // payout
        let p = &raw.payout;
        let mut at = Vec::new();
        for t in &p.at {
            let (h, m) = t.trim().split_once(':').ok_or_else(|| anyhow!("payout.at {t:?}: use \"HH:MM\" (UTC)"))?;
            let (h, m): (u32, u32) = (h.parse().map_err(|_| anyhow!("payout.at {t:?}"))?, m.parse().map_err(|_| anyhow!("payout.at {t:?}"))?);
            if h > 23 || m > 59 {
                bail!("payout.at {t:?}: use \"HH:MM\" (UTC)");
            }
            at.push((h, m));
        }
        at.sort();
        at.dedup();
        if p.every_blocks == 0 && at.is_empty() {
            bail!("set payout.every_blocks (own proposed blocks between payouts) and/or payout.at (UTC times)");
        }
        if p.memo.trim().len() > 255 || keep.iter().any(|k| k.memo.trim().len() > 255) {
            bail!("memos are limited to 255 bytes");
        }
        let min_amount = match &p.min_amount {
            Some(v) => parse_ark(v).context("payout.min_amount")?,
            None => 0,
        };
        let max_fee = match &p.max_fee {
            Some(v) => parse_ark(v).context("payout.max_fee")?,
            None => ARKTOSHI / 10,
        };
        if max_fee == 0 {
            bail!("payout.max_fee must be above zero");
        }
        let gas_price = match &p.gas_price {
            Some(v) => parse_decimal(&toml_num(v)?, 9).context("payout.gas_price (gwei, up to 9 decimals)")? as u128,
            None => 0,
        };
        let secret_file = resolve(p.secret_file.as_deref().unwrap_or("secret.toml"));

        let data_dir = resolve(raw.storage.data_dir.as_deref().unwrap_or("."));

        Ok(Config {
            path: path.to_path_buf(),
            api,
            rpc,
            tx_api,
            broadcast,
            requests_per_minute: raw.network.requests_per_minute,
            rpc_requests_per_minute: raw.network.rpc_requests_per_minute,
            confirmations: raw.network.confirmations,
            poll_seconds: raw.network.poll_seconds,
            validator: raw.validator.name.trim().to_string(),
            start_height: raw.validator.start_height,
            voter_bps,
            share_fees: s.fees,
            min_vote,
            max_vote,
            anti_dilution: s.anti_dilution,
            include_self: s.include_self,
            ineligible,
            blacklist,
            whitelist,
            custom,
            keep,
            redirect,
            payout_enabled: p.enabled,
            every_blocks: p.every_blocks,
            at,
            min_amount,
            memo: p.memo.trim().to_string(),
            multipayment: p.multipayment,
            max_fee,
            gas_price,
            secret_file,
            data_dir,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("ark-tbw.db")
    }

    pub fn main_keep(&self) -> &str {
        &self.keep[0].address
    }

    /// Where a payout owed to `account` is sent.
    pub fn payee<'a>(&'a self, account: &'a str) -> &'a str {
        self.redirect.get(account).map(String::as_str).unwrap_or(account)
    }

    pub fn memo_for_keep(&self, address: &str) -> &str {
        match self.keep.iter().find(|k| k.address == address) {
            Some(k) if !k.memo.trim().is_empty() => k.memo.trim(),
            _ => &self.memo,
        }
    }

    /// Highest voter share a `share set` may use.
    pub fn max_voter_bps(&self) -> u64 {
        BPS
    }

    /// The keep goes to the validator wallet unless share.keep_address says otherwise.
    pub fn bind_validator(&mut self, address: &str) {
        if self.keep[0].address.is_empty() {
            self.keep[0].address = address.to_string();
        }
    }

    /// The config as it applies with a different voter share (the keep is always 100% - share).
    pub fn with_voter_bps(&self, bps: u64) -> Result<Config> {
        if bps > self.max_voter_bps() {
            bail!("voter share {} is above 100%", fmt_pct(bps));
        }
        let mut c = self.clone();
        c.voter_bps = bps;
        c.keep[0].bps = self.max_voter_bps() - bps;
        Ok(c)
    }

}

fn toml_num(v: &toml::Value) -> Result<String> {
    Ok(match v {
        toml::Value::String(s) => s.trim().to_string(),
        toml::Value::Integer(i) if *i >= 0 => i.to_string(),
        toml::Value::Float(f) if *f >= 0.0 && f.is_finite() => format!("{f}"),
        _ => bail!("not a non-negative number: {v}"),
    })
}

/// Secret file: `passphrase = "..."` and optionally `second_passphrase = "..."`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub passphrase: String,
    #[serde(default)]
    pub second_passphrase: Option<String>,
}

impl Drop for Secret {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.passphrase.zeroize();
        if let Some(s) = self.second_passphrase.as_mut() {
            s.zeroize();
        }
    }
}

/// Load the payout passphrase. A systemd credential named `secret` wins
/// (LoadCredential= / LoadCredentialEncrypted=); otherwise the secret file, which must not be
/// readable by group or others.
pub fn load_secret(cfg: &Config) -> Result<Secret> {
    let path = match std::env::var_os("CREDENTIALS_DIRECTORY") {
        Some(d) if Path::new(&d).join("secret").exists() => PathBuf::from(d).join("secret"),
        _ => {
            use std::os::unix::fs::MetadataExt;
            let p = cfg.secret_file.clone();
            let md = std::fs::metadata(&p).with_context(|| format!("cannot read secret file {}", p.display()))?;
            if md.mode() & 0o077 != 0 {
                bail!("secret file {} is readable by other users: run  chmod 600 {}", p.display(), p.display());
            }
            if md.uid() != unsafe { libc::geteuid() } {
                bail!("secret file {} is not owned by the user running ark-tbw", p.display());
            }
            p
        }
    };
    let text = zeroize::Zeroizing::new(std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?);
    let s: Secret = toml::from_str(&text).map_err(|_| anyhow!("secret file {} must hold passphrase = \"...\" (and optionally second_passphrase)", path.display()))?;
    if s.passphrase.trim().is_empty() {
        bail!("secret file {}: passphrase is empty", path.display());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[network]
api = "http://127.0.0.1:4003/api"
[validator]
name = "geops"
[share]
percent = 50
min_vote = 10
max_vote = "200000"
[payout]
every_blocks = 211
memo = "pool"
"#;

    fn parse(s: &str) -> Result<Config> {
        Config::parse(s, Path::new("/tmp/x/config.toml"))
    }

    #[test]
    fn amounts_and_percent() {
        assert_eq!(parse_ark(&toml::Value::String("10".into())).unwrap(), 10 * ARKTOSHI);
        assert_eq!(parse_ark(&toml::Value::String("0.00000001".into())).unwrap(), 1);
        assert_eq!(parse_ark(&toml::Value::Float(0.1)).unwrap(), 10_000_000);
        assert!(parse_ark(&toml::Value::String("0.000000001".into())).is_err());
        assert!(parse_ark(&toml::Value::Integer(-1)).is_err());
        assert_eq!(parse_percent(&toml::Value::Float(85.5)).unwrap(), 8550);
        assert_eq!(parse_percent(&toml::Value::String("99.25".into())).unwrap(), 9925);
        assert!(parse_percent(&toml::Value::Float(33.333)).is_err());
        assert!(parse_percent(&toml::Value::Integer(101)).is_err());
        assert_eq!(fmt_ark(150_000_000), "1.5");
        assert_eq!(fmt_ark(-1), "-0.00000001");
        assert_eq!(fmt_ark(0), "0");
    }

    #[test]
    fn base_config() {
        let c = parse(BASE).unwrap();
        assert_eq!(c.voter_bps, 5000);
        assert_eq!(c.min_vote, 10 * ARKTOSHI);
        assert_eq!(c.max_vote, 200_000 * ARKTOSHI);
        assert_eq!(c.secret_file, PathBuf::from("/tmp/x/secret.toml"));
        assert!(c.anti_dilution);
        assert_eq!(c.ineligible, Ineligible::Redistribute);
    }

    #[test]
    fn keep_is_the_rest() {
        let mut c = parse(BASE).unwrap();
        assert_eq!(c.keep[0].bps, 5000);
        assert_eq!(c.keep[0].address, "");
        c.bind_validator("0x7783464d965A1E6a13B19b8222C56C53d32414ad");
        assert_eq!(c.main_keep(), "0x7783464d965A1E6a13B19b8222C56C53d32414ad");
        let c = parse(&BASE.replace("percent = 50", "percent = 85.5\nkeep_address = \"0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22\"")).unwrap();
        assert_eq!(c.keep[0].bps, 1450);
        assert_eq!(c.main_keep(), "0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22");
        let c2 = c.with_voter_bps(9000).unwrap();
        assert_eq!(c2.keep[0].bps, 1000);
        assert!(c.with_voter_bps(10001).is_err());
    }

    #[test]
    fn rejects_bad_settings() {
        assert!(parse(&BASE.replace("every_blocks = 211", "")).is_err());
        assert!(parse(&BASE.replace("max_vote = \"200000\"", "max_vote = 5")).is_err());
        assert!(parse(&format!("{BASE}\n[redirect]\n0x7783464d965A1E6a13B19b8222C56C53d32414ad = \"0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22\"\n0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22 = \"0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763\"\n")).is_err());
        assert!(parse(&BASE.replace("[share]", "[share]\nbogus = 1")).is_err());
        // custom rates up to 100% are fine (the keep absorbs the difference), above is not
        let c = BASE.replace("max_vote = \"200000\"", "max_vote = \"200000\"\n[share.custom]\n0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22 = 100");
        assert!(parse(&c).is_ok());
        assert!(parse(&c.replace("= 100", "= 101")).is_err());
        // ARK Core addresses and mistyped checksums are refused; all lowercase is fine
        assert!(parse(&format!("{BASE}\n[redirect]\n0x7783464d965A1E6a13B19b8222C56C53d32414ad = \"D61mfSggzbvQgTUe6JhYKH2doHaqJ3Dyib\"\n")).is_err());
        assert!(parse(&format!("{BASE}\n[redirect]\n0x7783464d965A1E6a13B19b8222C56C53d32414ad = \"0x6f0182a0cc707b055322CcF6d4CB6a5Aff1aEb22\"\n")).is_err());
        let c = parse(&format!("{BASE}\n[redirect]\n0x7783464d965a1e6a13b19b8222c56c53d32414ad = \"0x6f0182a0cc707b055322ccf6d4cb6a5aff1aeb22\"\n")).unwrap();
        assert_eq!(c.redirect.get("0x7783464d965A1E6a13B19b8222C56C53d32414ad").map(String::as_str), Some("0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22"));
        // RPC and pool URLs follow the API URL; gas price in gwei
        let c = parse(&BASE.replace("[payout]", "[payout]\ngas_price = 7.5")).unwrap();
        assert_eq!(c.rpc, "http://127.0.0.1:4008/api");
        assert_eq!(c.tx_api, "http://127.0.0.1:4007/api");
        assert_eq!(c.gas_price, 7_500_000_000);
        assert!(parse(&BASE.replace("[payout]", "[[keep]]\naddress = \"0x7783464d965A1E6a13B19b8222C56C53d32414ad\"\n[payout]")).is_err());
    }
}
