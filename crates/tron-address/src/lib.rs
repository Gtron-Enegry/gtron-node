//! Validated TRON base58check address (`T…`, 34 chars, 0x41 version byte).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

const TRON_VERSION_BYTE: u8 = 0x41;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TronAddress {
    base58: String,
    raw: [u8; 21],
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("address is not valid base58")]
    NotBase58,
    #[error("address has wrong length")]
    WrongLength,
    #[error("address has wrong version byte (expected 0x41)")]
    WrongVersion,
    #[error("address checksum mismatch")]
    BadChecksum,
}

impl TronAddress {
    pub fn parse(s: &str) -> Result<Self, AddressError> {
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|_| AddressError::NotBase58)?;
        if bytes.len() != 25 {
            return Err(AddressError::WrongLength);
        }
        let (payload, checksum) = bytes.split_at(21);
        if payload[0] != TRON_VERSION_BYTE {
            return Err(AddressError::WrongVersion);
        }
        let hash = Sha256::digest(Sha256::digest(payload));
        if hash[..4] != checksum[..] {
            return Err(AddressError::BadChecksum);
        }
        let mut raw = [0u8; 21];
        raw.copy_from_slice(payload);
        Ok(Self {
            base58: s.to_owned(),
            raw,
        })
    }

    /// Build from the 21-byte raw form (0x41 + 20 bytes) that TRON nodes return
    /// when `visible=false`, or from the 20-byte EVM-style form.
    pub fn from_raw(raw: &[u8]) -> Result<Self, AddressError> {
        let payload: [u8; 21] = match raw.len() {
            21 if raw[0] == TRON_VERSION_BYTE => raw.try_into().unwrap(),
            20 => {
                let mut p = [0u8; 21];
                p[0] = TRON_VERSION_BYTE;
                p[1..].copy_from_slice(raw);
                p
            }
            21 => return Err(AddressError::WrongVersion),
            _ => return Err(AddressError::WrongLength),
        };
        let hash = Sha256::digest(Sha256::digest(payload));
        let mut full = Vec::with_capacity(25);
        full.extend_from_slice(&payload);
        full.extend_from_slice(&hash[..4]);
        Ok(Self {
            base58: bs58::encode(full).into_string(),
            raw: payload,
        })
    }

    /// Parse the hex form (`41…`, 42 hex chars) that TRON nodes return by default.
    pub fn from_hex(hex: &str) -> Result<Self, AddressError> {
        if hex.len() != 42 {
            return Err(AddressError::WrongLength);
        }
        let mut raw = [0u8; 21];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let s = std::str::from_utf8(chunk).map_err(|_| AddressError::NotBase58)?;
            raw[i] = u8::from_str_radix(s, 16).map_err(|_| AddressError::NotBase58)?;
        }
        Self::from_raw(&raw)
    }

    pub fn as_str(&self) -> &str {
        &self.base58
    }

    pub fn raw(&self) -> &[u8; 21] {
        &self.raw
    }

    pub fn to_hex(&self) -> String {
        self.raw.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for TronAddress {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::SchemaType::Type(
                utoipa::openapi::schema::Type::String,
            ))
            .description(Some("Base58check TRON address"))
            .examples(["TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t"])
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for TronAddress {}

impl fmt::Display for TronAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base58)
    }
}

impl fmt::Debug for TronAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TronAddress({})", self.base58)
    }
}

impl FromStr for TronAddress {
    type Err = AddressError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for TronAddress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.base58)
    }
}

impl<'de> Deserialize<'de> for TronAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // USDT (TRC20) contract address, a well-known valid mainnet address.
    const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

    #[test]
    fn accepts_valid_address() {
        let a = TronAddress::parse(USDT).unwrap();
        assert_eq!(a.as_str(), USDT);
        assert_eq!(a.raw()[0], 0x41);
    }

    #[test]
    fn hex_roundtrip() {
        let a = TronAddress::parse(USDT).unwrap();
        let hex = a.to_hex();
        assert_eq!(TronAddress::from_hex(&hex).unwrap(), a);
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut s = USDT.to_owned();
        s.replace_range(33..34, if &s[33..34] == "t" { "u" } else { "t" });
        assert!(TronAddress::parse(&s).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            TronAddress::parse("0xdeadbeef"),
            Err(AddressError::NotBase58)
        );
        assert!(TronAddress::parse("TRX").is_err());
    }
}
