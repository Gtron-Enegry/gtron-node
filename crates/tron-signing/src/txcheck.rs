//! Decode a node-built `Transaction.raw` and compare it field by field with what we asked for.
//!
//! `txID == sha256(raw_data)` only proves the two fields belong together; a node (or anyone
//! between us and it) computes both. So before signing we parse the protobuf ourselves and
//! refuse anything that is not exactly the one contract we requested: other contract types,
//! other owner/recipient/amount, another token contract, call value, extra calldata, a large
//! fee limit, a far-future expiration, an unexpected permission, memo data or unknown fields.

use gtron_tron_address::TronAddress;

use crate::tron::trc20_transfer_param;

/// `ContractType` values from Tron.proto.
const TRANSFER: u64 = 1;
const TRIGGER_SMART_CONTRACT: u64 = 31;
const DELEGATE_RESOURCE: u64 = 57;
const UNDELEGATE_RESOURCE: u64 = 58;
/// `ResourceCode.ENERGY`.
const ENERGY: u64 = 1;
/// keccak256("transfer(address,uint256)")[..4]
const TRC20_TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxCheckError {
    #[error("malformed transaction protobuf: {0}")]
    Malformed(&'static str),
    #[error("unexpected field {field} in {message}")]
    UnexpectedField { message: &'static str, field: u32 },
    #[error("field {field} of {message} appears more than once")]
    Duplicate { message: &'static str, field: u32 },
    #[error("transaction must hold exactly one contract, found {0}")]
    ContractCount(usize),
    #[error("transaction carries {0}, which we never request")]
    Forbidden(&'static str),
    #[error("{field} differs from the request")]
    Mismatch { field: &'static str },
    #[error("fee limit {got} exceeds the cap {cap}")]
    FeeLimit { got: i64, cap: i64 },
    #[error("expiration is outside the accepted window")]
    Expiration,
}

/// What the transaction must do, exactly.
#[derive(Debug, Clone)]
pub enum Expected<'a> {
    Trx {
        owner: &'a TronAddress,
        to: &'a TronAddress,
        amount_sun: i64,
    },
    Trc20 {
        owner: &'a TronAddress,
        contract: &'a TronAddress,
        to: &'a TronAddress,
        amount_raw: u128,
    },
    Delegate {
        owner: &'a TronAddress,
        receiver: &'a TronAddress,
        balance_sun: i64,
        lock_blocks: Option<i64>,
    },
    Undelegate {
        owner: &'a TronAddress,
        receiver: &'a TronAddress,
        balance_sun: i64,
    },
}

/// Envelope limits that do not depend on the contract.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Max `fee_limit` (SUN). Only smart-contract calls may set one at all.
    pub max_fee_limit_sun: i64,
    /// Max distance from now to `expiration`. A signed transaction stays valid until it
    /// expires, so a node that sits on it can broadcast it late; keep that window short.
    pub max_expiration_ahead_ms: i64,
    /// The `Permission_id` the contract must carry (0 = owner permission).
    pub permission_id: i32,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_fee_limit_sun: 30_000_000,
            max_expiration_ahead_ms: 5 * 60 * 1000,
            permission_id: 0,
        }
    }
}

/// The parts of a checked transaction the caller keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    pub expiration_ms: i64,
}

/// Check `raw` (the bytes of `raw_data_hex`) against `expected`. `now_ms` is our clock.
pub fn check(
    raw: &[u8],
    expected: &Expected<'_>,
    bounds: &Bounds,
    now_ms: i64,
) -> Result<Checked, TxCheckError> {
    let tx = RawTx::decode(raw)?;
    if tx.contracts.len() != 1 {
        return Err(TxCheckError::ContractCount(tx.contracts.len()));
    }
    if !tx.auths.is_empty() {
        return Err(TxCheckError::Forbidden("authority entries"));
    }
    if tx.data.is_some_and(|d| !d.is_empty()) {
        return Err(TxCheckError::Forbidden("a memo (data)"));
    }
    if tx.scripts.is_some_and(|d| !d.is_empty()) {
        return Err(TxCheckError::Forbidden("scripts"));
    }
    let expiration = tx.expiration.ok_or(TxCheckError::Expiration)?;
    if expiration <= now_ms - 60_000 || expiration > now_ms + bounds.max_expiration_ahead_ms {
        return Err(TxCheckError::Expiration);
    }
    if let Some(ts) = tx.timestamp {
        if ts >= expiration {
            return Err(TxCheckError::Expiration);
        }
    }

    let c = Contract::decode(tx.contracts[0])?;
    if c.permission_id != i64::from(bounds.permission_id) {
        return Err(TxCheckError::Mismatch {
            field: "Permission_id",
        });
    }
    let fee_limit = tx.fee_limit.unwrap_or(0);
    let is_vm = matches!(expected, Expected::Trc20 { .. });
    if fee_limit < 0 || (!is_vm && fee_limit != 0) {
        return Err(TxCheckError::FeeLimit {
            got: fee_limit,
            cap: 0,
        });
    }
    if fee_limit > bounds.max_fee_limit_sun {
        return Err(TxCheckError::FeeLimit {
            got: fee_limit,
            cap: bounds.max_fee_limit_sun,
        });
    }

    match *expected {
        Expected::Trx {
            owner,
            to,
            amount_sun,
        } => {
            c.expect_type(TRANSFER, "TransferContract")?;
            let p = Fields::decode(c.value, "TransferContract", &[1, 2, 3])?;
            eq_addr(p.bytes(1), owner, "owner_address")?;
            eq_addr(p.bytes(2), to, "to_address")?;
            eq_int(p.int(3), amount_sun, "amount")?;
        }
        Expected::Trc20 {
            owner,
            contract,
            to,
            amount_raw,
        } => {
            c.expect_type(TRIGGER_SMART_CONTRACT, "TriggerSmartContract")?;
            let p = Fields::decode(c.value, "TriggerSmartContract", &[1, 2, 3, 4, 5, 6])?;
            eq_addr(p.bytes(1), owner, "owner_address")?;
            eq_addr(p.bytes(2), contract, "contract_address")?;
            eq_int(p.int(3), 0, "call_value")?;
            eq_int(p.int(5), 0, "call_token_value")?;
            eq_int(p.int(6), 0, "token_id")?;
            let mut want = TRC20_TRANSFER_SELECTOR.to_vec();
            want.extend(
                hex::decode(trc20_transfer_param(to, amount_raw)).expect("param is valid hex"),
            );
            if p.bytes(4) != Some(want.as_slice()) {
                return Err(TxCheckError::Mismatch { field: "data" });
            }
        }
        Expected::Delegate {
            owner,
            receiver,
            balance_sun,
            lock_blocks,
        } => {
            c.expect_type(DELEGATE_RESOURCE, "DelegateResourceContract")?;
            let p = Fields::decode(c.value, "DelegateResourceContract", &[1, 2, 3, 4, 5, 6])?;
            eq_addr(p.bytes(1), owner, "owner_address")?;
            eq_int(p.int(2), ENERGY as i64, "resource")?;
            eq_int(p.int(3), balance_sun, "balance")?;
            eq_addr(p.bytes(4), receiver, "receiver_address")?;
            eq_int(p.int(5), i64::from(lock_blocks.is_some()), "lock")?;
            eq_int(p.int(6), lock_blocks.unwrap_or(0), "lock_period")?;
        }
        Expected::Undelegate {
            owner,
            receiver,
            balance_sun,
        } => {
            c.expect_type(UNDELEGATE_RESOURCE, "UnDelegateResourceContract")?;
            let p = Fields::decode(c.value, "UnDelegateResourceContract", &[1, 2, 3, 4])?;
            eq_addr(p.bytes(1), owner, "owner_address")?;
            eq_int(p.int(2), ENERGY as i64, "resource")?;
            eq_int(p.int(3), balance_sun, "balance")?;
            eq_addr(p.bytes(4), receiver, "receiver_address")?;
        }
    }
    Ok(Checked {
        expiration_ms: expiration,
    })
}

fn eq_addr(
    got: Option<&[u8]>,
    want: &TronAddress,
    field: &'static str,
) -> Result<(), TxCheckError> {
    if got == Some(want.raw().as_slice()) {
        Ok(())
    } else {
        Err(TxCheckError::Mismatch { field })
    }
}

/// proto3 omits zero values, so an absent integer means 0.
fn eq_int(got: Option<i64>, want: i64, field: &'static str) -> Result<(), TxCheckError> {
    if got.unwrap_or(0) == want {
        Ok(())
    } else {
        Err(TxCheckError::Mismatch { field })
    }
}

// ---- minimal protobuf wire reader ----

#[derive(Clone, Copy)]
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64, TxCheckError> {
        let mut out = 0u64;
        for i in 0..10 {
            let b = *self
                .buf
                .get(self.pos)
                .ok_or(TxCheckError::Malformed("truncated varint"))?;
            self.pos += 1;
            if i == 9 && b > 1 {
                return Err(TxCheckError::Malformed("varint overflow"));
            }
            out |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(TxCheckError::Malformed("varint too long"))
    }

    /// Next `(field number, value)`, or None at the end. Fixed-width and group wire types
    /// never occur in the messages we accept, so they are rejected.
    fn next(&mut self) -> Result<Option<(u32, Value<'a>)>, TxCheckError> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| TxCheckError::Malformed("field number"))?;
        if field == 0 {
            return Err(TxCheckError::Malformed("field number 0"));
        }
        match key & 7 {
            0 => Ok(Some((field, Value::Varint(self.varint()?)))),
            2 => {
                let len = usize::try_from(self.varint()?)
                    .map_err(|_| TxCheckError::Malformed("length"))?;
                let end = self
                    .pos
                    .checked_add(len)
                    .filter(|e| *e <= self.buf.len())
                    .ok_or(TxCheckError::Malformed("truncated field"))?;
                let v = &self.buf[self.pos..end];
                self.pos = end;
                Ok(Some((field, Value::Bytes(v))))
            }
            _ => Err(TxCheckError::Malformed("unsupported wire type")),
        }
    }
}

/// A flat message whose fields are all singular: rejects unknown and repeated fields.
struct Fields<'a> {
    values: Vec<(u32, Value<'a>)>,
}

impl<'a> Fields<'a> {
    fn decode(buf: &'a [u8], message: &'static str, allowed: &[u32]) -> Result<Self, TxCheckError> {
        let mut r = Reader::new(buf);
        let mut values: Vec<(u32, Value<'a>)> = Vec::new();
        while let Some((field, v)) = r.next()? {
            if !allowed.contains(&field) {
                return Err(TxCheckError::UnexpectedField { message, field });
            }
            if values.iter().any(|(f, _)| *f == field) {
                return Err(TxCheckError::Duplicate { message, field });
            }
            values.push((field, v));
        }
        Ok(Self { values })
    }

    fn get(&self, field: u32) -> Option<Value<'a>> {
        self.values
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| *v)
    }

    /// A length-delimited field; a varint in its place counts as a mismatch (None).
    fn bytes(&self, field: u32) -> Option<&'a [u8]> {
        match self.get(field) {
            Some(Value::Bytes(b)) => Some(b),
            _ => None,
        }
    }

    /// An int64/int32/enum/bool field. A length-delimited value in its place is reported
    /// as `i64::MIN`, which never equals anything we expect.
    fn int(&self, field: u32) -> Option<i64> {
        match self.get(field) {
            Some(Value::Varint(n)) => Some(n as i64),
            Some(Value::Bytes(_)) => Some(i64::MIN),
            None => None,
        }
    }
}

/// `Transaction.raw`.
struct RawTx<'a> {
    expiration: Option<i64>,
    timestamp: Option<i64>,
    fee_limit: Option<i64>,
    contracts: Vec<&'a [u8]>,
    auths: Vec<&'a [u8]>,
    data: Option<&'a [u8]>,
    scripts: Option<&'a [u8]>,
}

impl<'a> RawTx<'a> {
    fn decode(buf: &'a [u8]) -> Result<Self, TxCheckError> {
        const MSG: &str = "Transaction.raw";
        let mut tx = RawTx {
            expiration: None,
            timestamp: None,
            fee_limit: None,
            contracts: Vec::new(),
            auths: Vec::new(),
            data: None,
            scripts: None,
        };
        let mut seen: Vec<u32> = Vec::new();
        let mut r = Reader::new(buf);
        while let Some((field, v)) = r.next()? {
            // contract (11) and auths (9) are repeated; everything else is singular.
            if field != 11 && field != 9 {
                if seen.contains(&field) {
                    return Err(TxCheckError::Duplicate {
                        message: MSG,
                        field,
                    });
                }
                seen.push(field);
            }
            match (field, v) {
                // ref_block_bytes, ref_block_hash: TaPoS, not ours to judge.
                (1 | 4, Value::Bytes(_)) => {}
                // ref_block_num (deprecated)
                (3, Value::Varint(_)) => {}
                (8, Value::Varint(n)) => tx.expiration = Some(n as i64),
                (9, Value::Bytes(b)) => tx.auths.push(b),
                (10, Value::Bytes(b)) => tx.data = Some(b),
                (11, Value::Bytes(b)) => tx.contracts.push(b),
                (12, Value::Bytes(b)) => tx.scripts = Some(b),
                (14, Value::Varint(n)) => tx.timestamp = Some(n as i64),
                (18, Value::Varint(n)) => tx.fee_limit = Some(n as i64),
                _ => {
                    return Err(TxCheckError::UnexpectedField {
                        message: MSG,
                        field,
                    })
                }
            }
        }
        Ok(tx)
    }
}

/// `Transaction.Contract` with its `google.protobuf.Any` parameter unwrapped.
struct Contract<'a> {
    kind: i64,
    type_url: &'a [u8],
    value: &'a [u8],
    permission_id: i64,
}

impl<'a> Contract<'a> {
    fn decode(buf: &'a [u8]) -> Result<Self, TxCheckError> {
        // type (1), parameter (2), Permission_id (5). provider (3) and ContractName (4) are
        // never set by the node for the contracts we build.
        let c = Fields::decode(buf, "Contract", &[1, 2, 5])?;
        let param = c
            .bytes(2)
            .ok_or(TxCheckError::Malformed("contract without parameter"))?;
        let any = Fields::decode(param, "Any", &[1, 2])?;
        Ok(Self {
            kind: c.int(1).unwrap_or(0),
            type_url: any.bytes(1).unwrap_or_default(),
            value: any.bytes(2).unwrap_or_default(),
            permission_id: c.int(5).unwrap_or(0),
        })
    }

    fn expect_type(&self, kind: u64, name: &str) -> Result<(), TxCheckError> {
        let url = format!("type.googleapis.com/protocol.{name}");
        if self.kind == kind as i64 && self.type_url == url.as_bytes() {
            Ok(())
        } else {
            Err(TxCheckError::Mismatch {
                field: "contract type",
            })
        }
    }
}

/// Protobuf encoding helpers for fixtures (here and, with `test-util`, in the signer's tests).
#[cfg(any(test, feature = "test-util"))]
pub mod enc {
    use gtron_tron_address::TronAddress;

    pub fn varint(mut n: u64, out: &mut Vec<u8>) {
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    pub fn int_field(field: u32, n: i64, out: &mut Vec<u8>) {
        varint(u64::from(field) << 3, out);
        varint(n as u64, out);
    }

    pub fn bytes_field(field: u32, b: &[u8], out: &mut Vec<u8>) {
        varint((u64::from(field) << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }

    /// `Transaction.raw` with one `TransferContract`, laid out as TronGrid builds it.
    pub fn trx_transfer_raw(
        owner: &TronAddress,
        to: &TronAddress,
        amount: i64,
        expiration: i64,
        timestamp: i64,
    ) -> Vec<u8> {
        let mut value = Vec::new();
        bytes_field(1, owner.raw(), &mut value);
        bytes_field(2, to.raw(), &mut value);
        int_field(3, amount, &mut value);
        let mut any = Vec::new();
        bytes_field(
            1,
            b"type.googleapis.com/protocol.TransferContract",
            &mut any,
        );
        bytes_field(2, &value, &mut any);
        let mut c = Vec::new();
        int_field(1, 1, &mut c);
        bytes_field(2, &any, &mut c);
        let mut raw = Vec::new();
        bytes_field(1, &[0x07, 0x90], &mut raw);
        bytes_field(
            4,
            &[0xe1, 0xb9, 0xde, 0x55, 0x96, 0x65, 0xc6, 0x71],
            &mut raw,
        );
        int_field(8, expiration, &mut raw);
        bytes_field(11, &c, &mut raw);
        int_field(14, timestamp, &mut raw);
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::enc::*;
    use super::*;

    struct Fixture {
        kind: i64,
        name: &'static str,
        value: Vec<u8>,
        permission_id: i64,
        expiration: i64,
        timestamp: i64,
        fee_limit: i64,
        extra_raw: Vec<u8>,
        extra_contract: bool,
    }

    impl Fixture {
        fn new(kind: i64, name: &'static str, value: Vec<u8>) -> Self {
            Self {
                kind,
                name,
                value,
                permission_id: 0,
                expiration: NOW + 60_000,
                timestamp: NOW,
                fee_limit: 0,
                extra_raw: Vec::new(),
                extra_contract: false,
            }
        }

        fn contract(&self) -> Vec<u8> {
            let mut any = Vec::new();
            bytes_field(
                1,
                format!("type.googleapis.com/protocol.{}", self.name).as_bytes(),
                &mut any,
            );
            bytes_field(2, &self.value, &mut any);
            let mut c = Vec::new();
            int_field(1, self.kind, &mut c);
            bytes_field(2, &any, &mut c);
            if self.permission_id != 0 {
                int_field(5, self.permission_id, &mut c);
            }
            c
        }

        fn encode(&self) -> Vec<u8> {
            let mut raw = Vec::new();
            bytes_field(1, &[0x07, 0x90], &mut raw);
            bytes_field(
                4,
                &[0xe1, 0xb9, 0xde, 0x55, 0x96, 0x65, 0xc6, 0x71],
                &mut raw,
            );
            int_field(8, self.expiration, &mut raw);
            let c = self.contract();
            bytes_field(11, &c, &mut raw);
            if self.extra_contract {
                bytes_field(11, &c, &mut raw);
            }
            int_field(14, self.timestamp, &mut raw);
            if self.fee_limit != 0 {
                int_field(18, self.fee_limit, &mut raw);
            }
            raw.extend_from_slice(&self.extra_raw);
            raw
        }
    }

    const NOW: i64 = 1_700_000_000_000;

    fn addr(s: &str) -> TronAddress {
        s.parse().unwrap()
    }

    fn owner() -> TronAddress {
        addr("TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC")
    }
    fn to() -> TronAddress {
        addr("TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7")
    }
    fn usdt() -> TronAddress {
        addr("TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t")
    }

    fn transfer_value(owner: &TronAddress, to: &TronAddress, amount: i64) -> Vec<u8> {
        let mut v = Vec::new();
        bytes_field(1, owner.raw(), &mut v);
        bytes_field(2, to.raw(), &mut v);
        int_field(3, amount, &mut v);
        v
    }

    fn trc20_value(owner: &TronAddress, contract: &TronAddress, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        bytes_field(1, owner.raw(), &mut v);
        bytes_field(2, contract.raw(), &mut v);
        bytes_field(4, data, &mut v);
        v
    }

    fn trc20_data(to: &TronAddress, amount: u128) -> Vec<u8> {
        let mut d = TRC20_TRANSFER_SELECTOR.to_vec();
        d.extend(hex::decode(trc20_transfer_param(to, amount)).unwrap());
        d
    }

    fn delegate_value(lock: Option<i64>, balance: i64) -> Vec<u8> {
        let mut v = Vec::new();
        bytes_field(1, owner().raw(), &mut v);
        int_field(2, 1, &mut v);
        int_field(3, balance, &mut v);
        bytes_field(4, to().raw(), &mut v);
        if let Some(b) = lock {
            int_field(5, 1, &mut v);
            int_field(6, b, &mut v);
        }
        v
    }

    fn trx_expected<'a>(o: &'a TronAddress, t: &'a TronAddress) -> Expected<'a> {
        Expected::Trx {
            owner: o,
            to: t,
            amount_sun: 1_000_000,
        }
    }

    fn bounds() -> Bounds {
        Bounds::default()
    }

    /// Same layout TronGrid's `wallet/createtransaction` produces (ref block bytes and
    /// hash, expiration, one contract, timestamp); addresses from the TRON API docs.
    #[test]
    fn accepts_node_shaped_transfer() {
        let o = TronAddress::from_raw(
            &hex::decode("41608f8da72479edc7dd921e4c30bb7e7cddbe722e").unwrap(),
        )
        .unwrap();
        let t = TronAddress::from_raw(
            &hex::decode("41e9d79cc47518930bc322d9bf7cddd260a0260a8d").unwrap(),
        )
        .unwrap();
        let raw = trx_transfer_raw(&o, &t, 1000, NOW + 57_000, NOW);
        let exp = Expected::Trx {
            owner: &o,
            to: &t,
            amount_sun: 1000,
        };
        let c = check(&raw, &exp, &bounds(), NOW).unwrap();
        assert_eq!(c.expiration_ms, NOW + 57_000);
        let other = Expected::Trx {
            owner: &o,
            to: &t,
            amount_sun: 1001,
        };
        assert_eq!(
            check(&raw, &other, &bounds(), NOW),
            Err(TxCheckError::Mismatch { field: "amount" })
        );
    }

    #[test]
    fn trx_transfer_tampering() {
        let (o, t) = (owner(), to());
        let exp = trx_expected(&o, &t);
        let ok = Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000));
        assert!(check(&ok.encode(), &exp, &bounds(), NOW).is_ok());

        let cases: Vec<(Fixture, TxCheckError)> = vec![
            (
                Fixture::new(
                    1,
                    "TransferContract",
                    transfer_value(&o, &usdt(), 1_000_000),
                ),
                TxCheckError::Mismatch {
                    field: "to_address",
                },
            ),
            (
                Fixture::new(1, "TransferContract", transfer_value(&t, &t, 1_000_000)),
                TxCheckError::Mismatch {
                    field: "owner_address",
                },
            ),
            (
                Fixture::new(1, "TransferContract", transfer_value(&o, &t, 9_000_000_000)),
                TxCheckError::Mismatch { field: "amount" },
            ),
            (
                Fixture::new(
                    2,
                    "TransferAssetContract",
                    transfer_value(&o, &t, 1_000_000),
                ),
                TxCheckError::Mismatch {
                    field: "contract type",
                },
            ),
            (
                Fixture::new(1, "TriggerSmartContract", transfer_value(&o, &t, 1_000_000)),
                TxCheckError::Mismatch {
                    field: "contract type",
                },
            ),
            (
                Fixture {
                    extra_contract: true,
                    ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
                },
                TxCheckError::ContractCount(2),
            ),
            (
                Fixture {
                    permission_id: 2,
                    ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
                },
                TxCheckError::Mismatch {
                    field: "Permission_id",
                },
            ),
            (
                Fixture {
                    fee_limit: 1,
                    ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
                },
                TxCheckError::FeeLimit { got: 1, cap: 0 },
            ),
            (
                Fixture {
                    expiration: NOW + 24 * 3600 * 1000,
                    ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
                },
                TxCheckError::Expiration,
            ),
            (
                Fixture {
                    expiration: NOW - 3_600_000,
                    timestamp: NOW - 3_700_000,
                    ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
                },
                TxCheckError::Expiration,
            ),
        ];
        for (f, want) in cases {
            assert_eq!(check(&f.encode(), &exp, &bounds(), NOW), Err(want));
        }

        // Memo, unknown raw field, unknown contract field, duplicate amount.
        let mut memo = Vec::new();
        bytes_field(10, b"hi", &mut memo);
        let f = Fixture {
            extra_raw: memo,
            ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
        };
        assert_eq!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Forbidden("a memo (data)"))
        );
        let mut unknown = Vec::new();
        int_field(30, 1, &mut unknown);
        let f = Fixture {
            extra_raw: unknown,
            ..Fixture::new(1, "TransferContract", transfer_value(&o, &t, 1_000_000))
        };
        assert!(matches!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::UnexpectedField { .. })
        ));
        let mut v = transfer_value(&o, &t, 1_000_000);
        int_field(7, 5, &mut v);
        let f = Fixture::new(1, "TransferContract", v);
        assert!(matches!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::UnexpectedField { .. })
        ));
        let mut v = transfer_value(&o, &t, 1_000_000);
        int_field(3, 9_000_000_000, &mut v);
        let f = Fixture::new(1, "TransferContract", v);
        assert!(matches!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Duplicate { .. })
        ));
        // Garbage.
        assert!(matches!(
            check(&[0x0a, 0xff], &exp, &bounds(), NOW),
            Err(TxCheckError::Malformed(_))
        ));
    }

    #[test]
    fn trc20_tampering() {
        let (o, t, u) = (owner(), to(), usdt());
        let exp = Expected::Trc20 {
            owner: &o,
            contract: &u,
            to: &t,
            amount_raw: 5_000_000,
        };
        let good = |data: Vec<u8>| Fixture {
            fee_limit: 30_000_000,
            ..Fixture::new(31, "TriggerSmartContract", trc20_value(&o, &u, &data))
        };
        assert!(check(
            &good(trc20_data(&t, 5_000_000)).encode(),
            &exp,
            &bounds(),
            NOW
        )
        .is_ok());
        assert_eq!(
            check(
                &good(trc20_data(&o, 5_000_000)).encode(),
                &exp,
                &bounds(),
                NOW
            ),
            Err(TxCheckError::Mismatch { field: "data" })
        );
        assert_eq!(
            check(
                &good(trc20_data(&t, 6_000_000)).encode(),
                &exp,
                &bounds(),
                NOW
            ),
            Err(TxCheckError::Mismatch { field: "data" })
        );
        let mut approve = vec![0x09, 0x5e, 0xa7, 0xb3];
        approve.extend(hex::decode(trc20_transfer_param(&t, 5_000_000)).unwrap());
        assert_eq!(
            check(&good(approve).encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Mismatch { field: "data" })
        );
        let mut long = trc20_data(&t, 5_000_000);
        long.push(0);
        assert_eq!(
            check(&good(long).encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Mismatch { field: "data" })
        );
        // Another token contract.
        let f = Fixture {
            fee_limit: 30_000_000,
            ..Fixture::new(
                31,
                "TriggerSmartContract",
                trc20_value(&o, &t, &trc20_data(&t, 5_000_000)),
            )
        };
        assert_eq!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Mismatch {
                field: "contract_address"
            })
        );
        // call_value smuggles TRX along.
        let mut v = trc20_value(&o, &u, &trc20_data(&t, 5_000_000));
        int_field(3, 1_000_000_000, &mut v);
        let f = Fixture {
            fee_limit: 30_000_000,
            ..Fixture::new(31, "TriggerSmartContract", v)
        };
        assert_eq!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::Mismatch {
                field: "call_value"
            })
        );
        // Fee limit above the cap.
        let f = Fixture {
            fee_limit: 1_000_000_000,
            ..good(trc20_data(&t, 5_000_000))
        };
        assert_eq!(
            check(&f.encode(), &exp, &bounds(), NOW),
            Err(TxCheckError::FeeLimit {
                got: 1_000_000_000,
                cap: 30_000_000
            })
        );
    }

    #[test]
    fn delegation_tampering() {
        let (o, r) = (owner(), to());
        let b = Bounds {
            permission_id: 3,
            ..bounds()
        };
        let exp = Expected::Delegate {
            owner: &o,
            receiver: &r,
            balance_sun: 10_000_000,
            lock_blocks: Some(1200),
        };
        let f = |v: Vec<u8>, perm: i64| Fixture {
            permission_id: perm,
            ..Fixture::new(57, "DelegateResourceContract", v)
        };
        assert!(check(
            &f(delegate_value(Some(1200), 10_000_000), 3).encode(),
            &exp,
            &b,
            NOW
        )
        .is_ok());
        assert_eq!(
            check(
                &f(delegate_value(None, 10_000_000), 3).encode(),
                &exp,
                &b,
                NOW
            ),
            Err(TxCheckError::Mismatch { field: "lock" })
        );
        assert_eq!(
            check(
                &f(delegate_value(Some(10), 10_000_000), 3).encode(),
                &exp,
                &b,
                NOW
            ),
            Err(TxCheckError::Mismatch {
                field: "lock_period"
            })
        );
        assert_eq!(
            check(
                &f(delegate_value(Some(1200), 99_000_000), 3).encode(),
                &exp,
                &b,
                NOW
            ),
            Err(TxCheckError::Mismatch { field: "balance" })
        );
        assert_eq!(
            check(
                &f(delegate_value(Some(1200), 10_000_000), 2).encode(),
                &exp,
                &b,
                NOW
            ),
            Err(TxCheckError::Mismatch {
                field: "Permission_id"
            })
        );
        // A plain TRX transfer under the same permission is not a delegation.
        let t = Fixture {
            permission_id: 3,
            ..Fixture::new(1, "TransferContract", transfer_value(&o, &r, 10_000_000))
        };
        assert_eq!(
            check(&t.encode(), &exp, &b, NOW),
            Err(TxCheckError::Mismatch {
                field: "contract type"
            })
        );

        let un = Expected::Undelegate {
            owner: &o,
            receiver: &r,
            balance_sun: 10_000_000,
        };
        let mut v = Vec::new();
        bytes_field(1, o.raw(), &mut v);
        int_field(2, 1, &mut v);
        int_field(3, 10_000_000, &mut v);
        bytes_field(4, r.raw(), &mut v);
        let ok = Fixture {
            permission_id: 3,
            ..Fixture::new(58, "UnDelegateResourceContract", v.clone())
        };
        assert!(check(&ok.encode(), &un, &b, NOW).is_ok());
        // Bandwidth instead of energy.
        let mut bw = Vec::new();
        bytes_field(1, o.raw(), &mut bw);
        int_field(3, 10_000_000, &mut bw);
        bytes_field(4, r.raw(), &mut bw);
        let f = Fixture {
            permission_id: 3,
            ..Fixture::new(58, "UnDelegateResourceContract", bw)
        };
        assert_eq!(
            check(&f.encode(), &un, &b, NOW),
            Err(TxCheckError::Mismatch { field: "resource" })
        );
    }
}
