//! secp256k1 keys as TRON uses them: the address of a key, signatures over raw_data, and
//! signed messages (TronLink `signMessageV2`) that prove control of a wallet.

use gtron_tron_address::TronAddress;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use sha3::Keccak256;

/// TRON signature: 65 bytes `r || s || v` over sha256(raw_data), hex-encoded.
pub fn sign_raw_data(key: &SigningKey, raw_data_hex: &str) -> Result<String, String> {
    let raw = hex::decode(raw_data_hex).map_err(|e| e.to_string())?;
    let digest = Sha256::digest(&raw);
    let (sig, rec): (Signature, RecoveryId) = key.sign_prehash_recoverable(&digest);
    let mut out = sig.to_bytes().to_vec();
    out.push(rec.to_byte());
    Ok(hex::encode(out))
}

/// TIP-191 style signed message, as TronLink `signMessageV2` makes it:
/// keccak256("\x19TRON Signed Message:\n" + len + message), 65 bytes `r || s || v` with
/// v = 27/28, hex. It cannot be replayed as a transaction: the prefix makes it no valid
/// raw_data.
pub fn sign_message(key: &SigningKey, message: &str) -> String {
    let mut h = Keccak256::new();
    h.update(b"\x19TRON Signed Message:\n");
    h.update(message.len().to_string().as_bytes());
    h.update(message.as_bytes());
    let (sig, rec): (Signature, RecoveryId) = key.sign_prehash_recoverable(&h.finalize());
    let mut out = sig.to_bytes().to_vec();
    out.push(rec.to_byte() + 27);
    hex::encode(out)
}

pub fn address_of(key: &SigningKey) -> TronAddress {
    address_of_verifying_key(key.verifying_key())
}

/// 0x41 || last 20 bytes of keccak256(uncompressed public key without its 0x04 prefix).
pub fn address_of_verifying_key(key: &VerifyingKey) -> TronAddress {
    let point = key.to_sec1_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    TronAddress::from_raw(&hash[12..]).expect("20-byte raw address")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_and_signature_of_a_known_key() {
        let key = SigningKey::from_slice(
            &hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
        )
        .unwrap();
        // Well-known address of private key 1 on TRON.
        assert_eq!(
            address_of(&key).as_str(),
            "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC"
        );
        let sig = sign_raw_data(&key, "0a02").unwrap();
        let bytes = hex::decode(&sig).unwrap();
        assert_eq!(bytes.len(), 65);
        let s = Signature::from_slice(&bytes[..64]).unwrap();
        let rec = RecoveryId::from_byte(bytes[64]).unwrap();
        let digest = Sha256::digest(hex::decode("0a02").unwrap());
        let vk = VerifyingKey::recover_from_prehash(&digest, &s, rec).unwrap();
        assert_eq!(&vk, key.verifying_key());
    }
}
