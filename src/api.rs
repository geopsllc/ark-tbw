//! Mainsail API clients (blocking): the public REST API (blocks, transactions with receipts and
//! logs, wallets, validators), the transaction pool API (broadcast, unconfirmed) and JSON-RPC
//! (chain id, gas estimates and balances at a past block).
//!
//! Measured on the public testnet (Nov 2026): REST has no effective limit; JSON-RPC allows 150
//! requests per 60 s window per IP, and a JSON-RPC batch counts as ONE request, so hundreds of
//! balances cost one request. The node keeps account history for the last 256 blocks
//! (`historySize` in the node's EVM service): eth_getBalance at an older block returns 0.

use crate::evm::{self, norm};
use crate::warn;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Blocks of account history a Mainsail node keeps; we stay well inside it.
pub const HISTORY_BLOCKS: u64 = 256;
pub const HISTORY_SAFE: u64 = 230;

pub struct Pace {
    budget: u32,
    window_start: Instant,
    used: u32,
}

impl Pace {
    fn new(budget: u32) -> Pace {
        Pace { budget, window_start: Instant::now(), used: 0 }
    }
    fn wait(&mut self) {
        if self.budget == 0 {
            return;
        }
        if self.window_start.elapsed() >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.used = 0;
        }
        if self.used >= self.budget {
            std::thread::sleep(Duration::from_secs(60).saturating_sub(self.window_start.elapsed()));
            self.window_start = Instant::now();
            self.used = 0;
        }
        self.used += 1;
    }
}

pub struct Api {
    base: String,
    rpc: String,
    tx_api: String,
    agent: ureq::Agent,
    rest_pace: Pace,
    rpc_pace: Pace,
    pub requests: u64,
    pub rpc_requests: u64,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub height: u64,
    pub id: String,
    pub ts: u64,
    pub proposer: String,
    /// wei
    pub reward: u128,
    /// wei: gas fees of the block, paid to the proposer with the reward
    pub fee: u128,
}

#[derive(Clone, Debug)]
pub struct Log {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Tx {
    pub id: String,
    pub block_id: String,
    pub ts: u64,
    pub from: String,
    pub sender_pk: String,
    /// None for a contract creation
    pub to: Option<String>,
    pub value: u128,
    pub gas_price: u128,
    pub gas_used: u64,
    /// receipt status: false = reverted (value did not move, gas was still paid)
    pub ok: bool,
    pub nonce: u64,
    pub data: String,
    pub logs: Vec<Log>,
    pub confirmations: u64,
}

impl Tx {
    /// what the sender paid for gas, in wei (the receipt's gasUsed already has refunds removed)
    pub fn fee(&self) -> u128 {
        self.gas_used as u128 * self.gas_price
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Wallet {
    pub address: String,
    pub public_key: Option<String>,
    /// wei
    pub balance: u128,
    /// the NEXT nonce to use (Core v3 reported the last used one)
    pub nonce: u64,
    pub vote: Option<String>,
    pub second_public_key: Option<String>,
    pub username: Option<String>,
    /// has registered as a validator (its blocks bring rewards)
    pub validator: bool,
    pub resigned: bool,
    /// registration fee held by the consensus contract, refunded on resignation (wei)
    pub validator_fee: u128,
    pub last_block: u64,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub chain_id: u64,
    pub nethash: String,
    pub min_gas_price: u128,
    pub max_gas_limit: u64,
}

#[derive(Debug, Default)]
pub struct BroadcastResult {
    pub accepted: Vec<String>,
    /// hash -> (error type, message)
    pub rejected: Vec<(String, String, String)>,
}

fn num(v: &Value) -> u128 {
    match v {
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0) as u128,
        _ => 0,
    }
}
fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}
fn addr(v: &Value) -> Option<String> {
    v.as_str().and_then(norm)
}
fn hexnum(v: &Value) -> Result<u128> {
    let h = v.as_str().ok_or_else(|| anyhow!("expected a hex number, got {v}"))?;
    u128::from_str_radix(h.trim_start_matches("0x"), 16).with_context(|| format!("bad hex number {h}"))
}

pub fn parse_block(b: &Value) -> Result<Block> {
    Ok(Block {
        height: b["number"].as_u64().ok_or_else(|| anyhow!("block without number"))?,
        id: s(b, "hash"),
        ts: num(&b["timestamp"]) as u64,
        proposer: addr(&b["proposer"]).ok_or_else(|| anyhow!("block without proposer"))?,
        reward: num(&b["reward"]),
        fee: num(&b["fee"]),
    })
}

pub fn parse_tx(t: &Value) -> Result<Tx> {
    let r = &t["receipt"];
    if !r.is_object() {
        bail!("transaction {} has no receipt", s(t, "hash"));
    }
    let logs = r["logs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|l| Log {
                    address: addr(&l["address"]).unwrap_or_default(),
                    topics: l["topics"].as_array().map(|x| x.iter().filter_map(|t| t.as_str().map(str::to_lowercase)).collect()).unwrap_or_default(),
                    data: s(l, "data"),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Tx {
        id: s(t, "hash"),
        block_id: s(t, "blockHash"),
        ts: num(&t["timestamp"]) as u64,
        from: addr(&t["from"]).ok_or_else(|| anyhow!("transaction without sender"))?,
        sender_pk: s(t, "senderPublicKey"),
        to: addr(&t["to"]),
        value: num(&t["value"]),
        gas_price: num(&t["gasPrice"]),
        gas_used: num(&r["gasUsed"]) as u64,
        ok: r["status"].as_u64() == Some(1),
        nonce: num(&t["nonce"]) as u64,
        data: s(t, "data"),
        logs,
        confirmations: t["confirmations"].as_u64().unwrap_or(0),
    })
}

fn parse_wallet(w: &Value) -> Wallet {
    let a = &w["attributes"];
    let nonempty = |k: &str| a.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string);
    Wallet {
        address: addr(&w["address"]).unwrap_or_else(|| s(w, "address")),
        public_key: w.get("publicKey").and_then(Value::as_str).map(str::to_string),
        balance: num(&w["balance"]),
        nonce: num(&w["nonce"]) as u64,
        vote: a.get("vote").and_then(addr),
        second_public_key: nonempty("secondPublicKey"),
        username: nonempty("username"),
        // migrated wallets can carry validator attributes with an empty key: not validators
        validator: nonempty("validatorPublicKey").is_some() || a.get("validatorRank").is_some(),
        resigned: a["validatorResigned"].as_bool().unwrap_or(false),
        validator_fee: num(&a["validatorFee"]),
        last_block: a["validatorLastBlock"]["number"].as_u64().unwrap_or(0),
    }
}

impl Api {
    pub fn new(base: &str, rpc: &str, tx_api: &str, requests_per_minute: u32, rpc_per_minute: u32) -> Api {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("ark-tbw/", env!("CARGO_PKG_VERSION")))
            .build();
        Api {
            base: base.trim_end_matches('/').to_string(),
            rpc: rpc.trim_end_matches('/').to_string(),
            tx_api: tx_api.trim_end_matches('/').to_string(),
            agent,
            rest_pace: Pace::new(requests_per_minute),
            rpc_pace: Pace::new(rpc_per_minute),
            requests: 0,
            rpc_requests: 0,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// GET returning the JSON body; Ok(None) on 404.
    pub fn get_opt(&mut self, path: &str) -> Result<Option<Value>> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{}", self.base, path) };
        let mut server_retries = [0.5f64, 1.5, 4.0].into_iter();
        let mut net_retries = [0.5f64, 2.0, 5.0, 10.0].into_iter();
        let mut limited = 0;
        loop {
            self.rest_pace.wait();
            self.requests += 1;
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
        self.all_opt(path, max_rows)?.ok_or_else(|| anyhow!("API 404 on {path}"))
    }

    /// As `all`, but None when the resource does not exist (404 on the first page).
    pub fn all_opt(&mut self, path: &str, max_rows: usize) -> Result<Option<Vec<Value>>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out = Vec::new();
        for page in 1.. {
            let d = match self.get_opt(&format!("{path}{sep}limit=100&page={page}"))? {
                Some(d) => d,
                None if page == 1 => return Ok(None),
                None => bail!("API 404 on {path} page {page}"),
            };
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
        Ok(Some(out))
    }

    // ---------- JSON-RPC ----------

    fn rpc_post(&mut self, body: &Value) -> Result<Value> {
        let mut retries = [0.5f64, 2.0, 5.0, 10.0].into_iter();
        let mut limited = 0;
        loop {
            self.rpc_pace.wait();
            self.rpc_requests += 1;
            let res = self.agent.post(&self.rpc).send_json(body.clone());
            match res {
                Ok(r) => {
                    let v: Value = r.into_json().context("bad JSON from JSON-RPC")?;
                    // the limiter answers 429 inside a 200 JSON body on some gateways
                    if v.get("statusCode").and_then(Value::as_u64) == Some(429) {
                        limited += 1;
                        std::thread::sleep(Duration::from_secs(if limited > 3 { 30 } else { 10 }));
                        continue;
                    }
                    return Ok(v);
                }
                Err(ureq::Error::Status(429, r)) => {
                    let wait = r.header("retry-after").and_then(|v| v.parse::<f64>().ok()).unwrap_or(15.0).clamp(1.0, 120.0);
                    limited += 1;
                    if limited % 5 == 1 {
                        warn!("JSON-RPC rate limit hit, pausing {wait:.0}s (set network.rpc_requests_per_minute lower)");
                    }
                    std::thread::sleep(Duration::from_secs_f64(wait));
                }
                Err(ureq::Error::Status(code, r)) if code >= 500 => match retries.next() {
                    Some(w) => std::thread::sleep(Duration::from_secs_f64(w)),
                    None => bail!("JSON-RPC {code}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
                },
                Err(ureq::Error::Status(code, r)) => bail!("JSON-RPC {code}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>()),
                Err(e) => match retries.next() {
                    Some(w) => std::thread::sleep(Duration::from_secs_f64(w)),
                    None => bail!("JSON-RPC unreachable ({}): {e}", self.rpc),
                },
            }
        }
    }

    pub fn rpc_call(&mut self, method: &str, params: Value) -> Result<Value> {
        let v = self.rpc_post(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))?;
        if let Some(e) = v.get("error") {
            bail!("{method}: {} {}", s(e, "message"), e.get("data").and_then(Value::as_str).unwrap_or_default());
        }
        Ok(v["result"].clone())
    }

    pub fn chain_id(&mut self) -> Result<u64> {
        Ok(hexnum(&self.rpc_call("eth_chainId", json!([]))?)? as u64)
    }

    /// Balances (wei) of `addresses` right after block `height`, from the node's account
    /// history, in batches. The batch also asks the node for its height: a node that has not
    /// reached `height` yet, or has pruned it, gives no answer (None) rather than a wrong one.
    pub fn balances_at(&mut self, addresses: &[String], height: u64) -> Result<Option<Vec<u128>>> {
        let mut out = Vec::with_capacity(addresses.len());
        for chunk in addresses.chunks(250) {
            let mut batch = vec![json!({"jsonrpc": "2.0", "id": 0, "method": "eth_blockNumber", "params": []})];
            for (i, a) in chunk.iter().enumerate() {
                batch.push(json!({"jsonrpc": "2.0", "id": i + 1, "method": "eth_getBalance", "params": [a, format!("0x{height:x}")]}));
            }
            let v = self.rpc_post(&Value::Array(batch))?;
            let arr = v.as_array().ok_or_else(|| anyhow!("JSON-RPC batch answer is not a list"))?;
            let mut by_id = std::collections::HashMap::new();
            for r in arr {
                if let Some(id) = r["id"].as_u64() {
                    by_id.insert(id, r);
                }
            }
            let tip = match by_id.get(&0).map(|r| hexnum(&r["result"])) {
                Some(Ok(t)) => t as u64,
                _ => return Ok(None),
            };
            if tip < height || tip > height + HISTORY_SAFE {
                return Ok(None);
            }
            for i in 0..chunk.len() {
                match by_id.get(&(i as u64 + 1)).map(|r| hexnum(&r["result"])) {
                    Some(Ok(b)) => out.push(b),
                    _ => return Ok(None),
                }
            }
        }
        Ok(Some(out))
    }

    pub fn estimate_gas(&mut self, from: &str, to: &str, value: u128, data: &[u8]) -> Result<u64> {
        let r = self.rpc_call("eth_estimateGas", json!([{"from": from, "to": to, "value": format!("0x{value:x}"), "data": format!("0x{}", hex::encode(data))}]))?;
        Ok(hexnum(&r)? as u64)
    }

    // ---------- REST ----------

    pub fn tip(&mut self) -> Result<(u64, String)> {
        let d = self.get("/blockchain")?;
        let h = d["data"]["block"]["number"].as_u64().ok_or_else(|| anyhow!("no block number in /blockchain"))?;
        Ok((h, s(&d["data"]["block"], "hash")))
    }

    pub fn node_info(&mut self) -> Result<NodeInfo> {
        let d = self.get("/node/configuration")?;
        let d = &d["data"];
        let g = &d["constants"]["gas"];
        let chain_id = self.chain_id()?;
        Ok(NodeInfo {
            chain_id,
            nethash: s(d, "nethash"),
            min_gas_price: num(&g["minimumGasPrice"]).max(1),
            max_gas_limit: (num(&g["maximumGasLimit"]) as u64).max(21_000),
        })
    }

    pub fn block(&mut self, height: u64) -> Result<Option<Block>> {
        match self.get_opt(&format!("/blocks/{height}"))? {
            Some(d) => Ok(Some(parse_block(&d["data"])?)),
            None => Ok(None),
        }
    }

    /// Blocks proposed by `validator` in from..=to, ascending.
    pub fn forged_range(&mut self, validator: &str, from: u64, to: u64) -> Result<Vec<Block>> {
        // a wallet that is not (or no longer) a validator has no blocks
        let rows = self.all_opt(&format!("/validators/{validator}/blocks?number.from={from}&number.to={to}&orderBy=number:asc"), 10_000_000)?.unwrap_or_default();
        let out: Vec<Block> = rows.iter().map(parse_block).collect::<Result<_>>()?;
        if out.windows(2).any(|w| w[0].height >= w[1].height) {
            bail!("block list for {validator} is out of order");
        }
        Ok(out)
    }

    /// Every transaction (with its receipt and logs) whose block timestamp is in [ts_from, ts_to].
    /// A transaction carries its block's timestamp and every block has its own, so this is a
    /// height range without per-block reads. The range is split until each part fits one page,
    /// so no row can be skipped or repeated across pages; counts are checked.
    pub fn txs_by_time(&mut self, ts_from: u64, ts_to: u64) -> Result<Vec<Tx>> {
        let mut out = Vec::new();
        let mut stack = vec![(ts_from, ts_to)];
        while let Some((a, b)) = stack.pop() {
            let d = self.get(&format!(
                "/transactions?timestamp.from={a}&timestamp.to={b}&orderBy=timestamp:asc&orderBy=transactionIndex:asc&fullReceipt=true&limit=100&page=1"
            ))?;
            let total = d["meta"]["totalCount"].as_u64().unwrap_or(0) as usize;
            let rows = d["data"].as_array().cloned().unwrap_or_default();
            if total <= 100 {
                if rows.len() != total {
                    bail!("transaction list for {a}..{b}: {} rows of {total}", rows.len());
                }
                out.push((a, rows.iter().map(parse_tx).collect::<Result<Vec<_>>>()?));
            } else if a < b {
                let mid = a + (b - a) / 2;
                stack.push((mid + 1, b));
                stack.push((a, mid));
            } else {
                // more than 100 transactions in one block: read that block's list directly
                let id = rows.first().and_then(|r| r["blockHash"].as_str()).ok_or_else(|| anyhow!("no block hash"))?.to_string();
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
        // stable: keeps the node's transaction order inside a block
        all.sort_by_key(|t| t.ts);
        Ok(all)
    }

    pub fn block_txs(&mut self, block_id: &str) -> Result<Vec<Tx>> {
        let rows = self.all(&format!("/blocks/{block_id}/transactions?fullReceipt=true&orderBy=transactionIndex:asc"), 100_000)?;
        rows.iter().map(parse_tx).collect()
    }

    /// Every transaction touching `address` (sent, received, multipayment recipient) from ts_from on.
    pub fn wallet_txs(&mut self, address: &str, ts_from: u64) -> Result<Vec<Tx>> {
        let rows = self.all(&format!("/transactions?address={address}&timestamp.from={ts_from}&orderBy=timestamp:asc&orderBy=transactionIndex:asc&fullReceipt=true"), 1_000_000)?;
        let mut v: Vec<Tx> = rows.iter().map(parse_tx).collect::<Result<_>>()?;
        v.sort_by_key(|t| t.ts);
        Ok(v)
    }

    /// Blocks proposed by a validator from height `from` on.
    pub fn forged_since(&mut self, validator: &str, from: u64) -> Result<Vec<Block>> {
        let rows = self.all_opt(&format!("/validators/{validator}/blocks?number.from={from}&orderBy=number:asc"), 1_000_000)?.unwrap_or_default();
        rows.iter().map(parse_block).collect()
    }

    pub fn wallet(&mut self, address: &str) -> Result<Option<Wallet>> {
        Ok(self.get_opt(&format!("/wallets/{address}"))?.map(|d| parse_wallet(&d["data"])))
    }

    /// A validator by username or address.
    pub fn validator(&mut self, name: &str) -> Result<Option<Wallet>> {
        let w = match self.wallet(name)? {
            Some(w) => Some(w),
            None => self.all(&format!("/wallets?attributes.username={name}"), 10)?.first().map(parse_wallet),
        };
        Ok(w.filter(|w| w.validator))
    }

    pub fn voters(&mut self, validator: &str) -> Result<Vec<Wallet>> {
        Ok(self.all(&format!("/validators/{validator}/voters?orderBy=address:asc"), 1_000_000)?.iter().map(parse_wallet).collect())
    }

    pub fn tx(&mut self, hash: &str) -> Result<Option<Tx>> {
        match self.get_opt(&format!("/transactions/{hash}?fullReceipt=true"))? {
            Some(d) => Ok(Some(parse_tx(&d["data"])?)),
            None => Ok(None),
        }
    }

    pub fn in_pool(&mut self, hash: &str) -> Result<bool> {
        let url = format!("{}/transactions/unconfirmed/{hash}", self.tx_api);
        Ok(self.get_opt(&url)?.is_some())
    }

    /// Unconfirmed transactions sent by `address`.
    pub fn pool_from(&mut self, address: &str) -> Result<Vec<String>> {
        let url = format!("{}/transactions/unconfirmed", self.tx_api);
        let rows = self.all(&url, 100_000)?;
        Ok(rows.iter().filter(|t| t["from"].as_str().and_then(norm).as_deref() == Some(address)).map(|t| s(t, "hash")).collect())
    }

    /// POST signed transactions (serialized hex, no 0x) to the pool. The pool answers with list
    /// INDEXES ({accept:[0,..]}, errors keyed "0"), mapped back to the hashes here. Safe to
    /// repeat: a signed transaction can only be applied once.
    pub fn broadcast(&mut self, txs: &[(String, String)]) -> Result<BroadcastResult> {
        let url = format!("{}/transactions", self.tx_api);
        self.rest_pace.wait();
        self.requests += 1;
        let body = json!({ "transactions": txs.iter().map(|t| t.1.clone()).collect::<Vec<_>>() });
        let resp: Value = match self.agent.post(&url).send_json(body) {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.get("data").is_some() || v.get("errors").is_some() => v,
                    _ => bail!("broadcast to {url} failed: HTTP {code} {}", text.chars().take(300).collect::<String>()),
                }
            }
            Err(e) => bail!("broadcast to {url} failed: {e}"),
        };
        let hash_of = |v: &Value| -> Option<String> {
            let i = match v {
                Value::Number(n) => n.as_u64()? as usize,
                Value::String(s) => match s.parse::<usize>() {
                    Ok(i) => i,
                    Err(_) => return txs.iter().find(|t| t.0 == *s).map(|t| t.0.clone()),
                },
                _ => return None,
            };
            txs.get(i).map(|t| t.0.clone())
        };
        let mut out = BroadcastResult::default();
        if let Some(a) = resp["data"]["accept"].as_array() {
            out.accepted = a.iter().filter_map(hash_of).collect();
        }
        if let Some(errs) = resp["errors"].as_object() {
            for (k, e) in errs {
                let e = if e.is_array() { &e[0] } else { e };
                let h = hash_of(&Value::String(k.clone())).unwrap_or_else(|| k.clone());
                out.rejected.push((h, s(e, "type"), s(e, "message")));
            }
        }
        // anything the pool neither accepted nor rejected by name (e.g. "excess")
        for (h, _) in txs {
            if !out.accepted.contains(h) && !out.rejected.iter().any(|r| &r.0 == h) {
                let why = ["excess", "invalid"].iter().find(|k| resp["data"][**k].as_array().is_some_and(|a| a.iter().any(|v| hash_of(v).as_ref() == Some(h))));
                out.rejected.push((h.clone(), why.map(|w| w.to_string()).unwrap_or_else(|| "unknown".into()), String::new()));
            }
        }
        Ok(out)
    }
}

/// Default JSON-RPC and pool URLs next to a REST API URL: the public gateway layout
/// (https://host/api, /rpc/api, /tx/api) or a node's default ports (4003, 4008, 4007).
pub fn default_urls(api: &str) -> Option<(String, String)> {
    let a = api.trim_end_matches('/');
    if let Some(root) = a.strip_suffix(":4003/api") {
        return Some((format!("{root}:4008/api"), format!("{root}:4007/api")));
    }
    let root = a.strip_suffix("/api")?;
    if root.ends_with("/rpc") || root.ends_with("/tx") {
        return None;
    }
    Some((format!("{root}/rpc/api"), format!("{root}/tx/api")))
}

#[allow(dead_code)]
pub fn is_system_contract(a: &str) -> bool {
    a == evm::CONSENSUS || a == evm::MULTIPAYMENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(default_urls("https://testnet.mainsailhq.com/api").unwrap(), ("https://testnet.mainsailhq.com/rpc/api".into(), "https://testnet.mainsailhq.com/tx/api".into()));
        assert_eq!(default_urls("http://127.0.0.1:4003/api/").unwrap(), ("http://127.0.0.1:4008/api".into(), "http://127.0.0.1:4007/api".into()));
        assert!(default_urls("http://x:9/v1").is_none());
    }

    #[test]
    fn parses_testnet_shapes() {
        let t: Value = serde_json::from_str(r#"{"blockHash":"1ec5","confirmations":4033,"data":"0x6dd7d8ea0000000000000000000000007783464d965a1e6a13b19b8222c56c53d32414ad","from":"0x7783464d965A1E6a13B19b8222C56C53d32414ad","gas":"187540","gasPrice":"5000000000","hash":"641a","nonce":"2","senderPublicKey":"027f","to":"0x535B3D7A252fa034Ed71F0C53ec0C6F784cB64E1","value":"0","receipt":{"cumulativeGasUsed":147732,"gasRefunded":0,"gasUsed":147732,"status":1,"logs":[{"data":"0x0000000000000000000000007783464d965a1e6a13b19b8222c56c53d32414ad0000000000000000000000007783464d965a1e6a13b19b8222c56c53d32414ad","topics":["0xCE0C7A2A940807F7DC2CE7A615C2532E915E6C0AC9A08BC4ED9D515A710A53E2"],"address":"0x535b3d7a252fa034ed71f0c53ec0c6f784cb64e1"}],"output":"0x"},"timestamp":"1795759177455"}"#).unwrap();
        let x = parse_tx(&t).unwrap();
        assert_eq!(x.fee(), 147_732 * 5_000_000_000);
        assert!(x.ok);
        assert_eq!(x.ts, 1_795_759_177_455);
        assert_eq!(x.to.as_deref(), Some(evm::CONSENSUS));
        assert_eq!(x.logs[0].address, evm::CONSENSUS);
        assert_eq!(x.logs[0].topics[0], "0xce0c7a2a940807f7dc2ce7a615c2532e915e6c0ac9a08bc4ed9d515a710a53e2");
        let b: Value = serde_json::from_str(r#"{"fee":"160160000000000","hash":"ab","number":23296208,"proposer":"0x1e729fd3bc1d98fe32c98fe618919b79ec878c2f","reward":"2000000000000000000","timestamp":"1795799916998"}"#).unwrap();
        let b = parse_block(&b).unwrap();
        assert_eq!(b.proposer, "0x1e729Fd3Bc1D98fe32c98fE618919b79Ec878c2F");
        assert_eq!(b.reward + b.fee, 2_000_160_160_000_000_000);
    }
}
