//! ARK Core v3 public API client (blocking). Tuned to the behaviour measured on api.ark.io:
//! 200 requests per fixed 60 s window, 429 with Retry-After, occasional 500s under load.
//! A relay on localhost has no such limit: set requests_per_minute = 0.

use crate::warn;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct Api {
    base: String,
    agent: ureq::Agent,
    budget: u32,
    window_start: Instant,
    used: u32,
    pub requests: u64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Block {
    pub height: u64,
    pub id: String,
    pub previous: String,
    pub ts: u64,
    pub generator: String,
    pub generator_pk: String,
    pub reward: u64,
    pub fee: u64,
    pub ntx: u32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Tx {
    pub id: String,
    pub block_id: String,
    pub ts: u64,
    pub type_group: u32,
    pub tx_type: u16,
    pub sender: String,
    pub sender_pk: String,
    pub recipient: Option<String>,
    pub amount: u64,
    pub fee: u64,
    pub nonce: u64,
    pub payments: Vec<(String, u64)>,
    pub votes: Vec<String>,
    pub lock_tx: Option<String>,
    pub confirmations: u64,
}

#[derive(Clone, Debug)]
pub struct Wallet {
    pub address: String,
    pub public_key: Option<String>,
    pub balance: u64,
    pub locked: u64,
    pub nonce: u64,
    pub vote: Option<String>,
    pub second_public_key: Option<String>,
    pub username: Option<String>,
    pub produced_blocks: u64,
    pub resigned: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct NodeInfo {
    pub pub_key_hash: u8,
    pub nethash: String,
    pub multipayment_limit: usize,
    pub max_tx_per_request: usize,
    pub dynamic: bool,
    pub per_byte: u64,
    pub addon_transfer: u64,
    pub addon_multipayment: u64,
    pub static_transfer: u64,
    pub static_multipayment: u64,
}

#[derive(Debug, Default)]
pub struct BroadcastResult {
    pub accepted: Vec<String>,
    /// id -> error type (e.g. ERR_LOW_FEE)
    pub rejected: Vec<(String, String, String)>,
}

fn num(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}
fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

pub fn parse_block(b: &Value) -> Result<Block> {
    Ok(Block {
        height: b["height"].as_u64().ok_or_else(|| anyhow!("block without height"))?,
        id: s(b, "id"),
        previous: s(b, "previous"),
        ts: b["timestamp"]["epoch"].as_u64().ok_or_else(|| anyhow!("block without timestamp"))?,
        generator: s(&b["generator"], "address"),
        generator_pk: s(&b["generator"], "publicKey"),
        reward: num(&b["forged"]["reward"]),
        fee: num(&b["forged"]["fee"]),
        ntx: b["transactions"].as_u64().unwrap_or(0) as u32,
    })
}

pub fn parse_tx(t: &Value) -> Result<Tx> {
    let asset = &t["asset"];
    Ok(Tx {
        id: s(t, "id"),
        block_id: s(t, "blockId"),
        ts: t["timestamp"]["epoch"].as_u64().ok_or_else(|| anyhow!("transaction without timestamp"))?,
        type_group: t["typeGroup"].as_u64().unwrap_or(1) as u32,
        tx_type: t["type"].as_u64().ok_or_else(|| anyhow!("transaction without type"))? as u16,
        sender: s(t, "sender"),
        sender_pk: s(t, "senderPublicKey"),
        recipient: t.get("recipient").and_then(Value::as_str).map(str::to_string),
        amount: num(&t["amount"]),
        fee: num(&t["fee"]),
        nonce: num(&t["nonce"]),
        payments: asset["payments"]
            .as_array()
            .map(|a| a.iter().map(|p| (s(p, "recipientId"), num(&p["amount"]))).collect())
            .unwrap_or_default(),
        votes: asset["votes"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        lock_tx: asset
            .pointer("/claim/lockTransactionId")
            .or_else(|| asset.pointer("/refund/lockTransactionId"))
            .and_then(Value::as_str)
            .map(str::to_string),
        confirmations: t["confirmations"].as_u64().unwrap_or(0),
    })
}

fn parse_wallet(w: &Value) -> Wallet {
    let a = &w["attributes"];
    Wallet {
        address: s(w, "address"),
        public_key: w.get("publicKey").and_then(Value::as_str).map(str::to_string),
        balance: num(&w["balance"]),
        locked: num(&a["htlc"]["lockedBalance"]),
        nonce: num(&w["nonce"]),
        vote: a.get("vote").and_then(Value::as_str).map(str::to_string),
        second_public_key: a.get("secondPublicKey").and_then(Value::as_str).map(str::to_string),
        username: a["delegate"].get("username").and_then(Value::as_str).map(str::to_string),
        produced_blocks: a["delegate"]["producedBlocks"].as_u64().unwrap_or(0),
        resigned: a["delegate"]["resigned"].as_bool().unwrap_or(false),
    }
}

impl Api {
    pub fn new(base: &str, requests_per_minute: u32) -> Api {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("ark-tbw/", env!("CARGO_PKG_VERSION")))
            .build();
        Api { base: base.trim_end_matches('/').to_string(), agent, budget: requests_per_minute, window_start: Instant::now(), used: 0, requests: 0 }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn pace(&mut self) {
        self.requests += 1;
        if self.budget == 0 {
            return;
        }
        if self.window_start.elapsed() >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.used = 0;
        }
        if self.used >= self.budget {
            let wait = Duration::from_secs(60).saturating_sub(self.window_start.elapsed());
            std::thread::sleep(wait);
            self.window_start = Instant::now();
            self.used = 0;
        }
        self.used += 1;
    }

    /// GET returning the JSON body; Ok(None) on 404.
    pub fn get_opt(&mut self, path: &str) -> Result<Option<Value>> {
        let url = format!("{}{}", self.base, path);
        let mut server_retries = [0.5f64, 1.5, 4.0].into_iter();
        let mut net_retries = [0.5f64, 2.0, 5.0, 10.0].into_iter();
        let mut limited = 0;
        loop {
            self.pace();
            match self.agent.get(&url).call() {
                Ok(r) => return Ok(Some(r.into_json().with_context(|| format!("bad JSON from {path}"))?)),
                Err(ureq::Error::Status(404, _)) => return Ok(None),
                Err(ureq::Error::Status(429, r)) => {
                    let wait = r.header("retry-after").and_then(|v| v.parse::<f64>().ok()).unwrap_or(10.0).clamp(1.0, 120.0);
                    limited += 1;
                    if limited % 5 == 1 {
                        warn!("API rate limit hit, pausing {wait:.0}s (set network.requests_per_minute lower)");
                    }
                    std::thread::sleep(Duration::from_secs_f64(wait));
                }
                Err(ureq::Error::Status(code, r)) if code >= 500 => match server_retries.next() {
                    Some(w) => std::thread::sleep(Duration::from_secs_f64(w)),
                    None => bail!("API {code} on {path}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
                },
                Err(ureq::Error::Status(code, r)) => {
                    bail!("API {code} on {path}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>())
                }
                Err(e) => match net_retries.next() {
                    Some(w) => std::thread::sleep(Duration::from_secs_f64(w)),
                    None => bail!("API unreachable ({}): {e}", self.base),
                },
            }
        }
    }

    pub fn get(&mut self, path: &str) -> Result<Value> {
        self.get_opt(path)?.ok_or_else(|| anyhow!("API 404 on {path}"))
    }

    /// All rows of a paged endpoint (ascending endpoints only: new rows land at the end).
    pub fn all(&mut self, path: &str, max_rows: usize) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out = Vec::new();
        for page in 1.. {
            let d = self.get(&format!("{path}{sep}limit=100&page={page}"))?;
            let rows = d["data"].as_array().cloned().unwrap_or_default();
            let n = rows.len();
            out.extend(rows);
            if n < 100 || d["meta"]["next"].is_null() {
                break;
            }
            if out.len() > max_rows {
                bail!("{path}: more than {max_rows} rows");
            }
        }
        Ok(out)
    }

    pub fn tip(&mut self) -> Result<(u64, String)> {
        let d = self.get("/blockchain")?;
        let h = d["data"]["block"]["height"].as_u64().ok_or_else(|| anyhow!("no height in /blockchain"))?;
        Ok((h, s(&d["data"]["block"], "id")))
    }

    pub fn node_info(&mut self) -> Result<NodeInfo> {
        let d = self.get("/node/configuration")?;
        let d = &d["data"];
        let c = &d["constants"];
        let df = &d["transactionPool"]["dynamicFees"];
        let st = &c["fees"]["staticFees"];
        let pkh = d["version"].as_u64().ok_or_else(|| anyhow!("node configuration has no network version"))?;
        Ok(NodeInfo {
            pub_key_hash: pkh as u8,
            nethash: s(d, "nethash"),
            multipayment_limit: c["multiPaymentLimit"].as_u64().unwrap_or(64).clamp(2, 64) as usize,
            max_tx_per_request: d["transactionPool"]["maxTransactionsPerRequest"].as_u64().unwrap_or(40).max(1) as usize,
            dynamic: df["enabled"].as_bool().unwrap_or(false),
            per_byte: num(&df["minFeePool"]).max(num(&df["minFeeBroadcast"])).max(1),
            addon_transfer: num(&df["addonBytes"]["transfer"]),
            addon_multipayment: num(&df["addonBytes"]["multiPayment"]),
            static_transfer: num(&st["transfer"]).max(1),
            static_multipayment: num(&st["multiPayment"]).max(1),
        })
    }

    pub fn block(&mut self, height_or_id: &str) -> Result<Option<Block>> {
        match self.get_opt(&format!("/blocks/{height_or_id}"))? {
            Some(d) => Ok(Some(parse_block(&d["data"])?)),
            None => Ok(None),
        }
    }

    /// The validator's own blocks in from..=to, ascending.
    pub fn forged_range(&mut self, validator: &str, from: u64, to: u64) -> Result<Vec<Block>> {
        let rows = self.all(&format!("/delegates/{validator}/blocks?height.from={from}&height.to={to}&orderBy=height:asc"), 10_000_000)?;
        let out: Vec<Block> = rows.iter().map(parse_block).collect::<Result<_>>()?;
        if out.windows(2).any(|w| w[0].height >= w[1].height) {
            bail!("forged block list for {validator} is out of order");
        }
        Ok(out)
    }

    /// Every transaction with a timestamp in [ts_from, ts_to] (each block has its own timestamp,
    /// and a transaction carries its block's). The range is split until each part fits one page,
    /// so no row can be skipped or repeated across page boundaries; counts are checked.
    pub fn txs_by_time(&mut self, ts_from: u64, ts_to: u64) -> Result<Vec<Tx>> {
        let mut out = Vec::new();
        let mut stack = vec![(ts_from, ts_to)];
        while let Some((a, b)) = stack.pop() {
            let d = self.get(&format!("/transactions?timestamp.from={a}&timestamp.to={b}&orderBy=timestamp:asc,nonce:asc&limit=100&page=1"))?;
            let total = d["meta"]["totalCount"].as_u64().unwrap_or(0) as usize;
            let rows = d["data"].as_array().cloned().unwrap_or_default();
            if total <= 100 {
                if rows.len() != total {
                    bail!("transaction list for {a}..{b}: {} rows of {total}", rows.len());
                }
                // keep ascending order: push later halves first, so pop order is ascending
                out.push((a, rows.iter().map(parse_tx).collect::<Result<Vec<_>>>()?));
            } else if a < b {
                let mid = a + (b - a) / 2;
                stack.push((mid + 1, b));
                stack.push((a, mid));
            } else {
                // more than 100 transactions in one block: read that block's list directly
                let id = rows.first().and_then(|r| r["blockId"].as_str()).ok_or_else(|| anyhow!("no block id"))?.to_string();
                let list = self.block_txs(&id)?;
                if list.len() != total {
                    bail!("block {id}: {} transactions of {total}", list.len());
                }
                out.push((a, list));
            }
        }
        out.sort_by_key(|(a, _)| *a);
        let mut seen = std::collections::HashSet::new();
        let mut all = Vec::new();
        for (_, list) in out {
            for t in list {
                if seen.insert(t.id.clone()) {
                    all.push(t);
                }
            }
        }
        all.sort_by(|x, y| x.ts.cmp(&y.ts).then(x.sender_pk.cmp(&y.sender_pk)).then(x.nonce.cmp(&y.nonce)));
        Ok(all)
    }

    pub fn block_txs(&mut self, block_id: &str) -> Result<Vec<Tx>> {
        let rows = self.all(&format!("/blocks/{block_id}/transactions"), 10_000)?;
        rows.iter().map(parse_tx).collect()
    }

    /// Every transaction touching `address` (sent, received, multipayment) from ts_from on.
    pub fn wallet_txs(&mut self, address: &str, ts_from: u64) -> Result<Vec<Tx>> {
        let rows = self.all(&format!("/wallets/{address}/transactions?timestamp.from={ts_from}&orderBy=timestamp:asc,nonce:asc"), 1_000_000)?;
        rows.iter().map(parse_tx).collect()
    }

    /// Blocks forged by a validator from height `from` on.
    pub fn forged_since(&mut self, validator: &str, from: u64) -> Result<Vec<Block>> {
        let rows = self.all(&format!("/delegates/{validator}/blocks?height.from={from}&orderBy=height:asc"), 1_000_000)?;
        rows.iter().map(parse_block).collect()
    }

    pub fn wallet(&mut self, address: &str) -> Result<Option<Wallet>> {
        Ok(self.get_opt(&format!("/wallets/{address}"))?.map(|d| parse_wallet(&d["data"])))
    }

    pub fn validator(&mut self, name: &str) -> Result<Option<Wallet>> {
        match self.get_opt(&format!("/delegates/{name}"))? {
            Some(d) => {
                let addr = s(&d["data"], "address");
                self.wallet(&addr)
            }
            None => Ok(None),
        }
    }

    pub fn voters(&mut self, validator: &str) -> Result<Vec<Wallet>> {
        Ok(self.all(&format!("/delegates/{validator}/voters?orderBy=address:asc"), 1_000_000)?.iter().map(parse_wallet).collect())
    }

    pub fn tx(&mut self, id: &str) -> Result<Option<Tx>> {
        match self.get_opt(&format!("/transactions/{id}"))? {
            Some(d) => Ok(Some(parse_tx(&d["data"])?)),
            None => Ok(None),
        }
    }

    pub fn in_pool(&mut self, id: &str) -> Result<bool> {
        Ok(self.get_opt(&format!("/transactions/unconfirmed/{id}"))?.is_some())
    }

    /// Unconfirmed transactions sent by `public_key`.
    pub fn pool_from(&mut self, public_key: &str) -> Result<Vec<String>> {
        let rows = self.all("/transactions/unconfirmed", 100_000)?;
        Ok(rows.iter().filter(|t| t["senderPublicKey"].as_str() == Some(public_key)).map(|t| s(t, "id")).collect())
    }

    /// POST signed transactions. Safe to repeat: the same signed transaction can only be applied once.
    pub fn broadcast(&mut self, txs: &[Value]) -> Result<BroadcastResult> {
        let url = format!("{}/transactions", self.base);
        self.pace();
        let body = serde_json::json!({ "transactions": txs });
        let resp: Value = match self.agent.post(&url).send_json(body) {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.get("data").is_some() || v.get("errors").is_some() => v,
                    _ => bail!("broadcast to {} failed: HTTP {code} {}", self.base, text.chars().take(300).collect::<String>()),
                }
            }
            Err(e) => bail!("broadcast to {} failed: {e}", self.base),
        };
        let mut out = BroadcastResult::default();
        if let Some(a) = resp["data"]["accept"].as_array() {
            out.accepted = a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        }
        if let Some(errs) = resp["errors"].as_object() {
            for (id, e) in errs {
                let e = if e.is_array() { &e[0] } else { e };
                out.rejected.push((id.clone(), s(e, "type"), s(e, "message")));
            }
        }
        Ok(out)
    }
}

