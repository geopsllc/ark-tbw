//! Mainsail (ARK on an EVM): addresses, transactions, legacy second signatures and the
//! multipayment call. Ported from ARK Wallet's tested signer (src-tauri/src/mainsail.rs).
//!
//! - Address = Ethereum address of the secp256k1 key (keccak256 of the uncompressed public key), EIP-55.
//! - Transactions are EIP-155 "legacy" Ethereum transactions:
//!   rlp([nonce, gasPrice, gasLimit, to, value, data, v, r, s]), v = recid + chainId * 2 + 35,
//!   signed over keccak(rlp([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0])).
//! - Wallets that registered a second passphrase on Core v3 add a 10th RLP field: a recoverable
//!   ECDSA signature (r || s || recid) by the second key over keccak(0x8f || "MAINSAIL_LSS_V1" || id),
//!   where id = keccak of the signed transaction without that field (the transaction hash).
//! - A transfer memo is the UTF-8 bytes in `data`; a multipayment is pay(address[],uint256[]) on
//!   the multipayment contract with value = the sum of the amounts.
//!
//! Checked byte for byte against the official SDK fixtures and live testnet transactions (tests).

use crate::crypto::KeyPair;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

pub const CONSENSUS: &str = "0x535B3D7A252fa034Ed71F0C53ec0C6F784cB64E1";
pub const MULTIPAYMENT: &str = "0x00EFd0D4639191C49908A7BddbB9A11A994A8527";

/// 1 ARK = 1e18 wei; the ledger counts in units of 1e-8 ARK ("arktoshi"), like Core v3.
pub const WEI_PER_UNIT: u128 = 10_000_000_000;

const LSS_DOMAIN: &[u8] = b"\x8fMAINSAIL_LSS_V1";

pub fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// topic0 of an event signature, e.g. "Voted(address,address)"
pub fn topic(sig: &str) -> String {
    format!("0x{}", hex::encode(keccak(sig.as_bytes())))
}

// ---------- addresses ----------

/// EIP-55 mixed-case checksum form of a 20-byte address.
pub fn checksum(raw: &[u8; 20]) -> String {
    let lower = hex::encode(raw);
    let h = keccak(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = (h[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0x0f;
        out.push(if c.is_ascii_alphabetic() && nibble >= 8 { c.to_ascii_uppercase() } else { c });
    }
    out
}

/// Mainsail address of a secp256k1 public key (compressed or not).
pub fn address_bytes(pubkey: &[u8]) -> Result<[u8; 20], String> {
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    let pk = k256::PublicKey::from_sec1_bytes(pubkey).map_err(|_| "Invalid public key")?;
    let point = pk.to_encoded_point(false);
    let h = keccak(&point.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Ok(a)
}

pub fn address_from_public_key(pubkey: &[u8]) -> Result<String, String> {
    Ok(checksum(&address_bytes(pubkey)?))
}

/// Parse a 0x address. Mixed case must carry a valid EIP-55 checksum (a typo is caught);
/// all-lower or all-upper case has no checksum to check.
pub fn parse_address(s: &str) -> Result<[u8; 20], String> {
    let s = s.trim();
    let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or("Invalid address")?;
    if body.len() != 40 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid address".into());
    }
    let raw: [u8; 20] = hex::decode(body).map_err(|_| "Invalid address")?.try_into().map_err(|_| "Invalid address")?;
    let mixed = body.chars().any(|c| c.is_ascii_lowercase()) && body.chars().any(|c| c.is_ascii_uppercase());
    if mixed && checksum(&raw)[2..] != *body {
        return Err("Address checksum does not match (typo?)".into());
    }
    Ok(raw)
}

/// The one spelling used for every address inside the app: EIP-55 checksum case.
pub fn norm(s: &str) -> Option<String> {
    parse_address(s).ok().map(|r| checksum(&r))
}

// ---------- RLP ----------

fn be_trim(v: u128) -> Vec<u8> {
    let b = v.to_be_bytes();
    let first = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    b[first..].to_vec()
}

fn rlp_len_prefix(len: usize, short: u8, long: u8, out: &mut Vec<u8>) {
    if len <= 55 {
        out.push(short + len as u8);
    } else {
        let l = be_trim(len as u128);
        out.push(long + l.len() as u8);
        out.extend_from_slice(&l);
    }
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() + 9);
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else {
        rlp_len_prefix(b.len(), 0x80, 0xb7, &mut out);
        out.extend_from_slice(b);
    }
    out
}

fn rlp_uint(v: u128) -> Vec<u8> {
    rlp_bytes(&be_trim(v))
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = Vec::with_capacity(payload.len() + 9);
    rlp_len_prefix(payload.len(), 0xc0, 0xf7, &mut out);
    out.extend_from_slice(&payload);
    out
}

fn strip_leading_zeros(b: &[u8]) -> &[u8] {
    let first = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    &b[first..]
}

// ---------- transactions ----------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmTx {
    pub chain_id: u64,
    pub nonce: u64,
    #[serde(with = "dec")]
    pub gas_price: u128,
    pub gas_limit: u64,
    pub to: String,
    /// wei
    #[serde(with = "dec")]
    pub value: u128,
    /// hex without 0x
    pub data: String,
}

/// u128 as a decimal string in JSON (wei amounts do not fit a JSON number)
mod dec {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedTx {
    /// transaction hash (id), hex without 0x
    pub hash: String,
    /// what is broadcast: hex without 0x
    pub serialized: String,
    pub from: String,
    /// the unsigned fields, kept so the same payments can be re-signed (same nonce, new gas price)
    pub tx: EvmTx,
}

impl EvmTx {
    fn fields(&self) -> Result<Vec<Vec<u8>>, String> {
        let to = parse_address(&self.to)?;
        let data = hex::decode(self.data.trim_start_matches("0x")).map_err(|_| "bad data")?;
        Ok(vec![rlp_uint(self.nonce as u128), rlp_uint(self.gas_price), rlp_uint(self.gas_limit as u128), rlp_bytes(&to), rlp_uint(self.value), rlp_bytes(&data)])
    }

    /// keccak(rlp([..., chainId, 0, 0])): what the sender signs (EIP-155)
    pub fn signing_hash(&self) -> Result<[u8; 32], String> {
        let mut f = self.fields()?;
        f.push(rlp_uint(self.chain_id as u128));
        f.push(rlp_uint(0));
        f.push(rlp_uint(0));
        Ok(keccak(&rlp_list(&f)))
    }

    fn signed_rlp(&self, rs: &[u8; 64], recid: u8, lss: Option<&[u8; 65]>) -> Result<Vec<u8>, String> {
        let mut f = self.fields()?;
        f.push(rlp_uint(recid as u128 + self.chain_id as u128 * 2 + 35));
        f.push(rlp_bytes(strip_leading_zeros(&rs[..32])));
        f.push(rlp_bytes(strip_leading_zeros(&rs[32..])));
        if let Some(l) = lss {
            f.push(rlp_bytes(l));
        }
        Ok(rlp_list(&f))
    }

    pub fn sign(&self, keys: &KeyPair, second: Option<&KeyPair>) -> Result<SignedTx, String> {
        let h = self.signing_hash()?;
        let (rs, recid) = keys.sign_recoverable(&h)?;
        let without_lss = self.signed_rlp(&rs, recid, None)?;
        let hash = keccak(&without_lss);
        let lss = match second {
            Some(k2) => {
                let mut msg = LSS_DOMAIN.to_vec();
                msg.extend_from_slice(&hash);
                let (rs2, recid2) = k2.sign_recoverable(&keccak(&msg))?;
                let mut out = [0u8; 65];
                out[..64].copy_from_slice(&rs2);
                out[64] = recid2;
                Some(out)
            }
            None => None,
        };
        let serialized = match &lss {
            Some(l) => self.signed_rlp(&rs, recid, Some(l))?,
            None => without_lss,
        };
        // check our own signature before anything leaves the machine
        if recover(&h, &rs, recid)? != keys.public {
            return Err("Signature self-check failed".into());
        }
        Ok(SignedTx { hash: hex::encode(hash), serialized: hex::encode(serialized), from: address_from_public_key(&keys.public)?, tx: self.clone() })
    }
}

/// Recover the compressed public key from a recoverable signature over a 32-byte hash.
pub fn recover(hash: &[u8; 32], rs: &[u8; 64], recid: u8) -> Result<[u8; 33], String> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let sig = Signature::from_slice(rs).map_err(|_| "Invalid signature")?;
    let rid = RecoveryId::from_byte(recid).ok_or("Invalid signature recovery id")?;
    let vk = VerifyingKey::recover_from_prehash(hash, &sig, rid).map_err(|_| "Invalid signature")?;
    vk.to_encoded_point(true).as_bytes().try_into().map_err(|_| "Invalid signature".to_string())
}

// ---------- calls ----------

pub fn selector(sig: &str) -> [u8; 4] {
    let h = keccak(sig.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

fn word_uint(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_address(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}

/// Calldata of MultiPayment.pay(address[] recipients, uint256[] amounts).
pub fn pay_call(payments: &[(String, u128)]) -> Result<Vec<u8>, String> {
    let n = payments.len();
    let mut out = selector("pay(address[],uint256[])").to_vec();
    out.extend_from_slice(&word_uint(64));
    out.extend_from_slice(&word_uint(64 + 32 + 32 * n as u128));
    out.extend_from_slice(&word_uint(n as u128));
    for (a, _) in payments {
        out.extend_from_slice(&word_address(&parse_address(a)?));
    }
    out.extend_from_slice(&word_uint(n as u128));
    for (_, v) in payments {
        out.extend_from_slice(&word_uint(*v));
    }
    Ok(out)
}

/// Intrinsic gas of calldata (EIP-2028): 4 per zero byte, 16 per other byte.
pub fn data_gas(data: &[u8]) -> u64 {
    data.iter().map(|b| if *b == 0 { 4u64 } else { 16 }).sum()
}

/// Gas a plain transfer to an account without code costs: 21000 + its memo bytes.
pub fn transfer_gas(memo: &str) -> u64 {
    21_000 + data_gas(memo.as_bytes())
}

// ---------- log decoding ----------

/// The address in a 32-byte ABI word ("0x" + 64 hex), checksummed.
pub fn word_to_address(w: &str) -> Option<String> {
    let h = w.trim_start_matches("0x");
    if h.len() != 64 {
        return None;
    }
    norm(&format!("0x{}", &h[24..]))
}

/// The n-th 32-byte word of ABI data, as hex (no 0x).
pub fn data_word(data: &str, n: usize) -> Option<&str> {
    let h = data.trim_start_matches("0x");
    h.get(n * 64..(n + 1) * 64)
}

pub fn word_to_u128(w: &str) -> Option<u128> {
    let w = w.trim_start_matches("0x");
    let (hi, lo) = w.split_at(w.len().saturating_sub(32));
    if hi.chars().any(|c| c != '0') {
        return None;
    }
    u128::from_str_radix(lo, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const PASSPHRASE: &str = "found lobster oblige describe ready addict body brave live vacuum display salute lizard combine gift resemble race senior quality reunion proud tell adjust angle";
    const SECOND: &str = "gold favorite math anchor detect march purpose such sausage crucial reform novel connect misery update episode invite salute barely garbage exclude winner visa cruise";

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/mainsail/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn num(v: &Value) -> u128 {
        match v {
            Value::String(s) => s.parse().unwrap(),
            Value::Number(n) => n.as_u64().unwrap() as u128,
            _ => panic!("not a number"),
        }
    }

    fn tx_from(f: &Value) -> EvmTx {
        let d = &f["data"];
        EvmTx {
            chain_id: d["network"].as_u64().unwrap_or(11812),
            nonce: num(&d["nonce"]) as u64,
            gas_price: num(&d["gasPrice"]),
            gas_limit: num(&d["gasLimit"]) as u64,
            to: d["to"].as_str().unwrap().to_string(),
            value: num(&d["value"]),
            data: d["data"].as_str().unwrap().trim_start_matches("0x").to_string(),
        }
    }

    #[test]
    fn signs_sdk_fixtures_byte_for_byte() {
        let keys = KeyPair::from_passphrase(PASSPHRASE).unwrap();
        for name in ["transfer", "transfer-0", "transfer-large-amount", "multipayment", "multipayment-single"] {
            let f = fixture(name);
            let s = tx_from(&f).sign(&keys, None).unwrap();
            assert_eq!(s.serialized, f["serialized"].as_str().unwrap(), "{name} serialized");
            assert_eq!(s.hash, f["data"]["hash"].as_str().unwrap(), "{name} hash");
            assert_eq!(s.from, f["data"]["from"].as_str().unwrap(), "{name} from");
        }
    }

    #[test]
    fn legacy_second_signature_matches_sdk() {
        let keys = KeyPair::from_passphrase(PASSPHRASE).unwrap();
        let second = KeyPair::from_passphrase(SECOND).unwrap();
        let f = fixture("transfer-legacy-second-signature");
        let s = tx_from(&f).sign(&keys, Some(&second)).unwrap();
        assert_eq!(s.serialized, f["serialized"].as_str().unwrap());
        assert_eq!(s.hash, f["data"]["hash"].as_str().unwrap());
    }

    #[test]
    fn multipayment_call_matches_sdk() {
        let f = fixture("multipayment");
        let data = pay_call(&[("0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22".into(), 100_000), ("0xC3bBE9B1CeE1ff85Ad72b87414B0E9B7F2366763".into(), 200_000)]).unwrap();
        assert_eq!(hex::encode(data), f["data"]["data"].as_str().unwrap());
        assert_eq!(hex::encode(selector("pay(address[],uint256[])")), "084ce708");
    }

    #[test]
    fn matches_live_testnet_transactions() {
        let path = format!("{}/tests/fixtures/mainsail/testnet-live.json", env!("CARGO_MANIFEST_DIR"));
        let txs: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for t in txs {
            let tx = EvmTx {
                chain_id: 11812,
                nonce: num(&t["nonce"]) as u64,
                gas_price: num(&t["gasPrice"]),
                gas_limit: num(&t["gas"]) as u64,
                to: t["to"].as_str().unwrap().to_string(),
                value: num(&t["value"]),
                data: t["data"].as_str().unwrap().trim_start_matches("0x").to_string(),
            };
            let sig = hex::decode(t["signature"].as_str().unwrap()).unwrap();
            let mut rs = [0u8; 64];
            rs.copy_from_slice(&sig[..64]);
            let signed = tx.signed_rlp(&rs, sig[64], None).unwrap();
            assert_eq!(hex::encode(keccak(&signed)), t["hash"].as_str().unwrap());
            let pk = recover(&tx.signing_hash().unwrap(), &rs, sig[64]).unwrap();
            assert_eq!(address_from_public_key(&pk).unwrap(), t["from"].as_str().unwrap());
        }
    }

    #[test]
    fn addresses_and_words() {
        assert!(parse_address("0x6f0182a0cc707b055322ccf6d4cb6a5aff1aeb22").is_ok());
        assert!(parse_address("0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb2").is_err());
        assert!(parse_address("0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb23").is_err()); // bad checksum
        assert_eq!(norm("0x6f0182a0cc707b055322ccf6d4cb6a5aff1aeb22").unwrap(), "0x6F0182a0cc707b055322CcF6d4CB6a5Aff1aEb22");
        assert_eq!(word_to_address("0x0000000000000000000000007783464d965a1e6a13b19b8222c56c53d32414ad").unwrap(), "0x7783464d965A1E6a13B19b8222C56C53d32414ad");
        let d = "0x00000000000000000000000000000000000000000000000000000000000186a00000000000000000000000000000000000000000000000000000000000000001";
        assert_eq!(word_to_u128(data_word(d, 0).unwrap()), Some(100_000));
        assert_eq!(word_to_u128(data_word(d, 1).unwrap()), Some(1));
        assert_eq!(data_word(d, 2), None);
        // the events the chain follower reads, as seen on testnet
        assert_eq!(topic("Voted(address,address)"), "0xce0c7a2a940807f7dc2ce7a615c2532e915e6c0ac9a08bc4ed9d515a710a53e2");
        assert_eq!(topic("Payment(address,uint256,bool)"), "0xf62eb0089c279dea4693baf6185affe0fe98a35d3c5ddbf4a6a0a557636533bd");
        assert_eq!(transfer_gas(""), 21_000);
        assert_eq!(transfer_gas("a\0"), 21_020);
    }
}
