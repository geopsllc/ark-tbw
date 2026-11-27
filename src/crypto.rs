//! Key handling. Mainsail keeps the Core v3 key scheme: private key = sha256(passphrase) on
//! secp256k1, so the validator's old passphrase controls its Mainsail (0x) address too.
//! Transactions are signed with recoverable ECDSA (see evm.rs).

use k256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub struct KeyPair {
    secret: Zeroizing<[u8; 32]>,
    pub public: [u8; 33],
}

impl KeyPair {
    pub fn from_passphrase(passphrase: &str) -> Result<Self, String> {
        let secret = Zeroizing::new(sha256(passphrase.as_bytes()));
        let sk = k256::SecretKey::from_slice(secret.as_ref()).map_err(|_| "invalid passphrase key")?;
        let public: [u8; 33] = sk.public_key().to_encoded_point(true).as_bytes().try_into().map_err(|_| "bad public key")?;
        Ok(Self { secret, public })
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.public)
    }

    /// Recoverable ECDSA over a 32-byte hash: (r || s, recovery id). Low-s (k256 normalises).
    pub fn sign_recoverable(&self, msg: &[u8; 32]) -> Result<([u8; 64], u8), String> {
        use k256::ecdsa::SigningKey;
        let sk = SigningKey::from_slice(self.secret.as_ref()).map_err(|_| "invalid key")?;
        let (sig, rid) = sk.sign_prehash_recoverable(msg).map_err(|_| "signing failed")?;
        let mut rs = [0u8; 64];
        rs.copy_from_slice(&sig.to_bytes());
        Ok((rs, rid.to_byte()))
    }
}

/// True when `text` is (or contains, as its whole content) a secret: a valid BIP39 mnemonic, or
/// the passphrase of one of `public_keys`. Used to stop a passphrase being published by accident,
/// e.g. pasted into a transaction memo, which is public.
pub fn looks_like_secret(text: &str, public_keys: &[&[u8]]) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let words = Zeroizing::new(t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase());
    if matches!(words.split(' ').count(), 12 | 15 | 18 | 21 | 24) && bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &words).is_ok() {
        return true;
    }
    match KeyPair::from_passphrase(t) {
        Ok(kp) => public_keys.iter().any(|pk| *pk == kp.public.as_slice()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_this_is_a_top_secret_passphrase() {
        // Canonical ARK test fixture
        let kp = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
        assert_eq!(kp.public_hex(), "034151a3ec46b5670a682b0a63394f863587d1bc97483b1b6c70eb58e7f0aed192");
    }

    #[test]
    fn detects_secrets() {
        let kp = KeyPair::from_passphrase("my own passphrase").unwrap();
        let mine: &[&[u8]] = &[&kp.public];
        assert!(looks_like_secret("  my own passphrase ", mine));
        assert!(!looks_like_secret("rent for march", mine));
        assert!(!looks_like_secret("", mine));
        // any valid BIP39 mnemonic, whoever it belongs to, any spacing or case
        let m = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        assert!(looks_like_secret(&m.to_uppercase().replace(' ', "   "), &[]));
        // 12 dictionary words with a bad checksum are just words
        assert!(!looks_like_secret("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon", &[]));
    }
}
