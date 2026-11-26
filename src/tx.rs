//! ARK Core v3 (AIP-11, version 2) transaction serialization and signing.
//! Serializer and verifier come from ARK Wallet (tested against real mainnet transactions).

use crate::crypto::{self, sha256, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const TYPE_TRANSFER: u16 = 0;
pub const TYPE_SECOND_SIGNATURE: u16 = 1;
pub const TYPE_DELEGATE_REGISTRATION: u16 = 2;
pub const TYPE_VOTE: u16 = 3;
pub const TYPE_MULTI_PAYMENT: u16 = 6;
pub const TYPE_DELEGATE_RESIGNATION: u16 = 7;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TxData {
    pub version: u8,
    #[serde(default)]
    pub network: u8,
    pub type_group: u32,
    #[serde(rename = "type")]
    pub tx_type: u16,
    pub nonce: String,
    pub sender_public_key: String,
    pub fee: String,
    pub amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "recipient")]
    pub recipient_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "signSignature")]
    pub second_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

fn parse_u64(s: &str, what: &str) -> Result<u64, String> {
    s.trim().parse::<u64>().map_err(|_| format!("Invalid {what}: {s}"))
}

fn hex33(s: &str, what: &str) -> Result<[u8; 33], String> {
    let v = hex::decode(s).map_err(|_| format!("Invalid {what}"))?;
    v.try_into().map_err(|_| format!("Invalid {what} length"))
}

/// Serialize per AIP-11. `with_sig` / `with_second` control which signatures are appended.
pub fn serialize(tx: &TxData, with_sig: bool, with_second: bool) -> Result<Vec<u8>, String> {
    if tx.type_group != 1 {
        return Err(format!("Unsupported type group {}", tx.type_group));
    }
    let mut b: Vec<u8> = Vec::with_capacity(256);
    b.push(0xff);
    b.push(tx.version);
    b.push(tx.network);
    b.extend_from_slice(&tx.type_group.to_le_bytes());
    b.extend_from_slice(&tx.tx_type.to_le_bytes());
    b.extend_from_slice(&parse_u64(&tx.nonce, "nonce")?.to_le_bytes());
    b.extend_from_slice(&hex33(&tx.sender_public_key, "sender public key")?);
    b.extend_from_slice(&parse_u64(&tx.fee, "fee")?.to_le_bytes());

    match tx.vendor_field.as_deref() {
        Some(v) if !v.is_empty() => {
            let bytes = v.as_bytes();
            if bytes.len() > 255 {
                return Err("Memo (vendor field) is longer than 255 bytes".into());
            }
            b.push(bytes.len() as u8);
            b.extend_from_slice(bytes);
        }
        _ => b.push(0),
    }

    let asset = tx.asset.as_ref();
    match tx.tx_type {
        TYPE_TRANSFER => {
            b.extend_from_slice(&parse_u64(&tx.amount, "amount")?.to_le_bytes());
            b.extend_from_slice(&tx.expiration.unwrap_or(0).to_le_bytes());
            let r = tx.recipient_id.as_deref().ok_or("Missing recipient")?;
            b.extend_from_slice(&crypto::decode_address(r, tx.network)?);
        }
        TYPE_SECOND_SIGNATURE => {
            let pk = asset
                .and_then(|a| a.pointer("/signature/publicKey"))
                .and_then(Value::as_str)
                .ok_or("Missing second public key")?;
            b.extend_from_slice(&hex33(pk, "second public key")?);
        }
        TYPE_DELEGATE_REGISTRATION => {
            let u = asset
                .and_then(|a| a.pointer("/delegate/username"))
                .and_then(Value::as_str)
                .ok_or("Missing validator username")?;
            b.push(u.len() as u8);
            b.extend_from_slice(u.as_bytes());
        }
        TYPE_VOTE => {
            let votes = asset
                .and_then(|a| a.get("votes"))
                .and_then(Value::as_array)
                .ok_or("Missing votes")?;
            b.push(votes.len() as u8);
            for v in votes {
                let v = v.as_str().ok_or("Invalid vote")?;
                let (prefix, pk) = match v.split_at(1) {
                    ("+", pk) => (1u8, pk),
                    ("-", pk) => (0u8, pk),
                    _ => return Err(format!("Invalid vote {v}")),
                };
                b.push(prefix);
                b.extend_from_slice(&hex33(pk, "validator public key")?);
            }
        }
        TYPE_MULTI_PAYMENT => {
            let payments = asset
                .and_then(|a| a.get("payments"))
                .and_then(Value::as_array)
                .ok_or("Missing payments")?;
            b.extend_from_slice(&(payments.len() as u16).to_le_bytes());
            for p in payments {
                let amount = p
                    .get("amount")
                    .and_then(|a| a.as_str().map(str::to_string).or_else(|| a.as_u64().map(|n| n.to_string())))
                    .ok_or("Invalid payment amount")?;
                b.extend_from_slice(&parse_u64(&amount, "payment amount")?.to_le_bytes());
                let r = p.get("recipientId").and_then(Value::as_str).ok_or("Invalid payment recipient")?;
                b.extend_from_slice(&crypto::decode_address(r, tx.network)?);
            }
        }
        TYPE_DELEGATE_RESIGNATION => {}
        t => return Err(format!("Unsupported transaction type {t}")),
    }

    if with_sig {
        if let Some(s) = &tx.signature {
            b.extend_from_slice(&hex::decode(s).map_err(|_| "Invalid signature hex")?);
        }
    }
    if with_second {
        if let Some(s) = &tx.second_signature {
            b.extend_from_slice(&hex::decode(s).map_err(|_| "Invalid second signature hex")?);
        }
    }
    Ok(b)
}

pub fn compute_id(tx: &TxData) -> Result<String, String> {
    Ok(hex::encode(sha256(&serialize(tx, true, true)?)))
}

/// Verify the first (and second, when `second_public_key` is given) signature.
pub fn verify(tx: &TxData, second_public_key: Option<&str>) -> Result<bool, String> {
    let sender = hex::decode(&tx.sender_public_key).map_err(|_| "bad sender key")?;
    let sig = hex::decode(tx.signature.as_deref().ok_or("unsigned")?).map_err(|_| "bad sig")?;
    let h1 = sha256(&serialize(tx, false, false)?);
    if !crypto::verify_any(&h1, &sig, &sender) {
        return Ok(false);
    }
    if let Some(spk) = second_public_key {
        let spk = hex::decode(spk).map_err(|_| "bad second key")?;
        let sig2 = hex::decode(tx.second_signature.as_deref().ok_or("missing second signature")?)
            .map_err(|_| "bad second sig")?;
        let h2 = sha256(&serialize(tx, true, false)?);
        if !crypto::verify_any(&h2, &sig2, &spk) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Signing keys for the payout wallet. Built from the secret file once at start-up.
pub struct Signer {
    pub keys: KeyPair,
    pub second: Option<KeyPair>,
    pub network: u8,
}

impl Signer {
    pub fn address(&self) -> String {
        crypto::address_from_public_key(&self.keys.public, self.network)
    }

    fn sign(&self, mut tx: TxData) -> Result<TxData, String> {
        let h1 = sha256(&serialize(&tx, false, false)?);
        tx.signature = Some(hex::encode(self.keys.sign_schnorr(&h1)));
        if let Some(k2) = &self.second {
            let h2 = sha256(&serialize(&tx, true, false)?);
            tx.second_signature = Some(hex::encode(k2.sign_schnorr(&h2)));
        }
        tx.id = Some(compute_id(&tx)?);
        let spk = self.second.as_ref().map(|k| k.public_hex());
        if !verify(&tx, spk.as_deref())? {
            return Err("internal error: signature self-check failed".into());
        }
        Ok(tx)
    }

    fn base(&self, tx_type: u16, nonce: u64, fee: u64, memo: &str) -> TxData {
        let memo = memo.trim();
        TxData {
            version: 2,
            network: self.network,
            type_group: 1,
            tx_type,
            nonce: nonce.to_string(),
            sender_public_key: self.keys.public_hex(),
            fee: fee.to_string(),
            amount: "0".into(),
            expiration: None,
            recipient_id: None,
            vendor_field: if memo.is_empty() { None } else { Some(memo.to_string()) },
            asset: None,
            signature: None,
            second_signature: None,
            id: None,
        }
    }

    pub fn transfer(&self, nonce: u64, fee: u64, memo: &str, recipient: &str, amount: u64) -> Result<TxData, String> {
        if amount == 0 {
            return Err("transfer amount must be greater than zero".into());
        }
        crypto::decode_address(recipient, self.network)?;
        let mut tx = self.base(TYPE_TRANSFER, nonce, fee, memo);
        tx.amount = amount.to_string();
        tx.expiration = Some(0);
        tx.recipient_id = Some(recipient.to_string());
        self.sign(tx)
    }

    pub fn multipayment(&self, nonce: u64, fee: u64, memo: &str, payments: &[(String, u64)]) -> Result<TxData, String> {
        if payments.len() < 2 || payments.len() > 64 {
            return Err("a multipayment needs between 2 and 64 recipients".into());
        }
        let mut out = Vec::new();
        for (r, a) in payments {
            crypto::decode_address(r, self.network)?;
            if *a == 0 {
                return Err(format!("payment to {r} is zero"));
            }
            out.push(json!({ "amount": a.to_string(), "recipientId": r }));
        }
        let mut tx = self.base(TYPE_MULTI_PAYMENT, nonce, fee, memo);
        tx.asset = Some(json!({ "payments": out }));
        self.sign(tx)
    }
}

/// Serialized size of a signed transfer / multipayment (Schnorr signatures), in bytes.
pub fn size_transfer(memo: &str, second: bool) -> u64 {
    59 + memo.trim().len() as u64 + 33 + 64 + if second { 64 } else { 0 }
}

pub fn size_multipayment(memo: &str, n: usize, second: bool) -> u64 {
    59 + memo.trim().len() as u64 + 2 + 29 * n as u64 + 64 + if second { 64 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real mainnet transactions fetched from api.ark.io (tests/fixtures/mainnet_txs.json).
    /// Re-serializing must reproduce the on-chain id and the signatures must verify.
    #[test]
    fn mainnet_fixtures_roundtrip() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mainnet_txs.json"))
            .expect("fixture file");
        let list: Vec<Value> = serde_json::from_str(&raw).unwrap();
        assert!(!list.is_empty());
        for v in list {
            let mut tx: TxData = serde_json::from_value(v.clone()).unwrap();
            tx.network = 23;
            let want = v["id"].as_str().unwrap();
            assert_eq!(compute_id(&tx).unwrap(), want, "id mismatch for type {}", tx.tx_type);
            let spk = v.get("_secondPublicKey").and_then(Value::as_str);
            assert!(verify(&tx, spk).unwrap(), "signature failed for {want} (type {})", tx.tx_type);
        }
    }

    fn signer(second: bool) -> Signer {
        Signer {
            keys: KeyPair::from_passphrase("this is a top secret passphrase").unwrap(),
            second: if second { Some(KeyPair::from_passphrase("second").unwrap()) } else { None },
            network: 30,
        }
    }

    #[test]
    fn sizes_match_serializer() {
        for second in [false, true] {
            let s = signer(second);
            let t = s.transfer(1, 1000, "pool payout", "D61mfSggzbvQgTUe6JhYKH2doHaqJ3Dyib", 5).unwrap();
            assert_eq!(serialize(&t, true, true).unwrap().len() as u64, size_transfer("pool payout", second));
            for n in [2usize, 7, 64] {
                let p: Vec<(String, u64)> = (0..n).map(|i| ("D61mfSggzbvQgTUe6JhYKH2doHaqJ3Dyib".to_string(), i as u64 + 1)).collect();
                let m = s.multipayment(2, 1000, "pool payout", &p).unwrap();
                assert_eq!(serialize(&m, true, true).unwrap().len() as u64, size_multipayment("pool payout", n, second));
                let spk = s.second.as_ref().map(|k| k.public_hex());
                assert!(verify(&m, spk.as_deref()).unwrap());
            }
        }
    }

    #[test]
    fn rejects_bad_input() {
        let s = signer(false);
        assert!(s.transfer(1, 1, "", "D61mfSggzbvQgTUe6JhYKH2doHaqJ3Dyib", 0).is_err());
        assert!(s.transfer(1, 1, "", "AKdr5d9AMEnsKYxpDcoHdyyjSCKVx3r9Nj", 1).is_err()); // mainnet address on devnet
        assert!(s.multipayment(1, 1, "", &[("D61mfSggzbvQgTUe6JhYKH2doHaqJ3Dyib".into(), 1)]).is_err());
    }
}
