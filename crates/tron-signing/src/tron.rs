//! Transaction building and broadcasting through a TRON node HTTP API (TronGrid).

use std::time::Duration;

use gtron_tron_address::TronAddress;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("node request failed: {0}")]
    Http(String),
    #[error("node refused: {0}")]
    Refused(String),
    #[error("node returned a transaction that does not match its id (tampering or bug)")]
    TxIdMismatch,
    #[error("broadcast failed: {0}")]
    Broadcast(String),
    #[error("node URL {0} must use https (plain http is allowed for localhost only)")]
    InsecureUrl(String),
}

pub struct Node {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

/// Unsigned transaction as the node returns it. We keep the full JSON so that
/// broadcasting sends exactly what the node produced, plus our signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedTx {
    #[serde(rename = "txID")]
    pub tx_id: String,
    pub raw_data_hex: String,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl UnsignedTx {
    /// txID must equal sha256(raw_data). This only binds the id to the bytes we sign;
    /// it says nothing about what those bytes do, because whoever built the transaction
    /// computed both. The contents are checked separately by [`crate::txcheck::check`].
    pub fn verify_id(&self) -> Result<(), NodeError> {
        let raw = hex::decode(&self.raw_data_hex).map_err(|_| NodeError::TxIdMismatch)?;
        let digest = hex::encode(Sha256::digest(&raw));
        if digest.eq_ignore_ascii_case(&self.tx_id) {
            Ok(())
        } else {
            Err(NodeError::TxIdMismatch)
        }
    }
}

/// `https://` anywhere; plain `http://` only for a node on this machine.
pub fn check_node_url(url: &str) -> Result<(), NodeError> {
    check_node_url_with(url, false)
}

/// Like [`check_node_url`]; `allow_http` (the operator's `TRONGRID_ALLOW_HTTP=1`) also
/// accepts plain `http://` to a node on a private link. The URL is parsed, so userinfo
/// tricks such as `http://localhost:x@evil.example/` are judged by their real host.
pub fn check_node_url_with(url: &str, allow_http: bool) -> Result<(), NodeError> {
    let insecure = || NodeError::InsecureUrl(url.to_owned());
    let parsed = url::Url::parse(url.trim()).map_err(|_| insecure())?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if allow_http => Ok(()),
        "http" => {
            let loopback = match parsed.host() {
                Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if loopback {
                Ok(())
            } else {
                Err(insecure())
            }
        }
        _ => Err(insecure()),
    }
}

/// `TRONGRID_ALLOW_HTTP=1|true`: the same opt-in the API honours for its node.
pub fn allow_plain_http_from_env() -> bool {
    std::env::var("TRONGRID_ALLOW_HTTP").is_ok_and(|v| v == "1" || v == "true")
}

/// Serialized `Transaction { raw_data = 1; signature = 2 }`, built from the raw bytes we
/// checked and signed, so nothing is re-encoded from the node's JSON.
pub fn signed_transaction_bytes(raw: &[u8], signature: &[u8]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut n: u64) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }
    let mut out = Vec::with_capacity(raw.len() + signature.len() + 8);
    out.push(0x0a);
    varint(&mut out, raw.len() as u64);
    out.extend_from_slice(raw);
    out.push(0x12);
    varint(&mut out, signature.len() as u64);
    out.extend_from_slice(signature);
    out
}

/// `DelegateResourceContract` and `UnDelegateResourceContract` in Tron.proto's ContractType.
pub const DELEGATE_RESOURCE_CONTRACT: usize = 57;
pub const UNDELEGATE_RESOURCE_CONTRACT: usize = 58;

/// An active permission of an account: who may sign which contract types for it.
#[derive(Debug, Clone)]
pub struct Permission {
    pub threshold: i64,
    /// (base58 address, weight)
    pub keys: Vec<(String, i64)>,
    /// Bit `n` (byte n/8, bit n%8) set = contract type `n` allowed.
    pub operations: Vec<u8>,
}

impl Permission {
    pub fn allowed_types(&self) -> Vec<usize> {
        (0..self.operations.len() * 8)
            .filter(|n| self.operations[n / 8] & (1 << (n % 8)) != 0)
            .collect()
    }

    /// `Ok` when `signer` alone can use this permission and it allows delegating and
    /// reclaiming resources and nothing else: a stolen key then cannot move TRX.
    pub fn check_delegation_only(&self, signer: &TronAddress) -> Result<(), String> {
        let weight = self
            .keys
            .iter()
            .find(|(a, _)| a == signer.as_str())
            .map(|(_, w)| *w)
            .ok_or_else(|| format!("{signer} is not a key of this permission"))?;
        if weight < self.threshold {
            return Err(format!(
                "{signer} has weight {weight}, the permission needs {}",
                self.threshold
            ));
        }
        let types = self.allowed_types();
        let wanted = [DELEGATE_RESOURCE_CONTRACT, UNDELEGATE_RESOURCE_CONTRACT];
        if !wanted.iter().all(|t| types.contains(t)) {
            return Err("the permission must allow DelegateResource and UnDelegateResource".into());
        }
        let extra: Vec<String> = types
            .iter()
            .filter(|t| !wanted.contains(t))
            .map(|&t| contract_name(t).map_or_else(|| format!("type {t}"), str::to_owned))
            .collect();
        if !extra.is_empty() {
            return Err(format!(
                "the permission also allows contract types {}: a stolen key could do that too; allow only DelegateResource and UnDelegateResource",
                extra.join(", ")
            ));
        }
        Ok(())
    }
}

/// Tron.proto names of the contract types a permission most often allows by mistake.
fn contract_name(t: usize) -> Option<&'static str> {
    Some(match t {
        1 => "TransferContract (sends TRX)",
        2 => "TransferAssetContract",
        4 => "VoteWitnessContract",
        30 => "CreateSmartContract",
        31 => "TriggerSmartContract (sends USDT and other tokens)",
        46 => "AccountPermissionUpdateContract (changes the wallet's keys)",
        54 => "FreezeBalanceV2Contract",
        55 => "UnfreezeBalanceV2Contract",
        56 => "WithdrawExpireUnfreezeContract",
        59 => "CancelAllUnfreezeV2Contract",
        _ => return None,
    })
}

/// Largest response body read from a TRON node or the platform. Real answers are a few KB;
/// a peer streaming more would otherwise grow memory until the process dies.
pub const MAX_BODY: usize = 1 << 20;

/// The body of `res` as text, refusing anything over `limit` bytes.
pub async fn read_body(mut res: reqwest::Response, limit: usize) -> Result<String, String> {
    if res.content_length().is_some_and(|n| n > limit as u64) {
        return Err(format!("response over {limit} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > limit {
            return Err(format!("response over {limit} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "response is not UTF-8".to_owned())
}

impl Node {
    pub fn new(base_url: &str, api_key: Option<String>, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                // A redirect would carry TRON-PRO-API-KEY to wherever it points.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
        }
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, NodeError> {
        let mut req = self
            .http
            .post(format!("{}/{path}", self.base_url))
            .json(&body);
        if let Some(k) = &self.api_key {
            req = req.header("TRON-PRO-API-KEY", k);
        }
        let res = req
            .send()
            .await
            .map_err(|e| NodeError::Http(e.to_string()))?;
        let status = res.status();
        let text = read_body(res, MAX_BODY).await.map_err(NodeError::Http)?;
        if !status.is_success() {
            return Err(NodeError::Refused(format!(
                "HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        serde_json::from_str(&text).map_err(|e| NodeError::Http(format!("bad JSON: {e}")))
    }

    fn check_error(v: &serde_json::Value) -> Result<(), NodeError> {
        if let Some(e) = v.get("Error").and_then(|e| e.as_str()) {
            return Err(NodeError::Refused(e.to_owned()));
        }
        if let Some(code) = v.get("code").and_then(|c| c.as_str()) {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .map(hex_to_text)
                .unwrap_or_default();
            return Err(NodeError::Refused(format!("{code}: {msg}")));
        }
        Ok(())
    }

    /// Plain TRX transfer. The node builds and we verify the id.
    pub async fn create_trx_transfer(
        &self,
        from: &TronAddress,
        to: &TronAddress,
        amount_sun: i64,
    ) -> Result<UnsignedTx, NodeError> {
        let v = self
            .post(
                "wallet/createtransaction",
                serde_json::json!({ "owner_address": from.as_str(), "to_address": to.as_str(), "amount": amount_sun, "visible": true }),
            )
            .await?;
        Self::check_error(&v)?;
        let tx: UnsignedTx = serde_json::from_value(v)
            .map_err(|e| NodeError::Http(format!("unexpected transaction shape: {e}")))?;
        tx.verify_id()?;
        Ok(tx)
    }

    /// `DelegateResourceContract` signed by a permitted key (`Permission_id`) on behalf of `owner`.
    pub async fn delegate_resource(
        &self,
        owner: &TronAddress,
        receiver: &TronAddress,
        balance_sun: i64,
        lock_blocks: Option<i64>,
        permission_id: Option<i32>,
    ) -> Result<UnsignedTx, NodeError> {
        let mut body = serde_json::json!({
            "owner_address": owner.as_str(), "receiver_address": receiver.as_str(),
            "balance": balance_sun, "resource": "ENERGY", "visible": true,
            "lock": lock_blocks.is_some(),
        });
        if let Some(b) = lock_blocks {
            body["lock_period"] = serde_json::json!(b);
        }
        if let Some(p) = permission_id {
            body["Permission_id"] = serde_json::json!(p);
        }
        let v = self.post("wallet/delegateresource", body).await?;
        Self::check_error(&v)?;
        let tx: UnsignedTx = serde_json::from_value(v)
            .map_err(|e| NodeError::Http(format!("unexpected transaction shape: {e}")))?;
        tx.verify_id()?;
        Ok(tx)
    }

    /// `UnDelegateResourceContract` for a delegation made earlier.
    pub async fn undelegate_resource(
        &self,
        owner: &TronAddress,
        receiver: &TronAddress,
        balance_sun: i64,
        permission_id: Option<i32>,
    ) -> Result<UnsignedTx, NodeError> {
        let mut body = serde_json::json!({
            "owner_address": owner.as_str(), "receiver_address": receiver.as_str(),
            "balance": balance_sun, "resource": "ENERGY", "visible": true,
        });
        if let Some(p) = permission_id {
            body["Permission_id"] = serde_json::json!(p);
        }
        let v = self.post("wallet/undelegateresource", body).await?;
        Self::check_error(&v)?;
        let tx: UnsignedTx = serde_json::from_value(v)
            .map_err(|e| NodeError::Http(format!("unexpected transaction shape: {e}")))?;
        tx.verify_id()?;
        Ok(tx)
    }

    /// TRC-20 `transfer(address,uint256)`.
    pub async fn create_trc20_transfer(
        &self,
        from: &TronAddress,
        contract: &TronAddress,
        to: &TronAddress,
        amount_raw: u128,
        fee_limit_sun: i64,
    ) -> Result<UnsignedTx, NodeError> {
        let param = trc20_transfer_param(to, amount_raw);
        let v = self
            .post(
                "wallet/triggersmartcontract",
                serde_json::json!({
                    "owner_address": from.as_str(), "contract_address": contract.as_str(),
                    "function_selector": "transfer(address,uint256)", "parameter": param,
                    "fee_limit": fee_limit_sun, "call_value": 0, "visible": true
                }),
            )
            .await?;
        Self::check_error(&v)?;
        if v.get("result")
            .and_then(|r| r.get("result"))
            .and_then(|b| b.as_bool())
            != Some(true)
        {
            let msg = v
                .get("result")
                .and_then(|r| r.get("message"))
                .and_then(|m| m.as_str())
                .map(hex_to_text)
                .unwrap_or_else(|| "no result".into());
            return Err(NodeError::Refused(msg));
        }
        let tx: UnsignedTx =
            serde_json::from_value(v.get("transaction").cloned().unwrap_or_default())
                .map_err(|e| NodeError::Http(format!("unexpected transaction shape: {e}")))?;
        tx.verify_id()?;
        Ok(tx)
    }

    /// Broadcast the exact bytes we checked and signed (`broadcasthex`), never the node's
    /// JSON rendering of them: java-tron rebuilds a JSON-posted transaction from
    /// `raw_data` and ignores `raw_data_hex`.
    pub async fn broadcast(
        &self,
        tx: &UnsignedTx,
        signature_hex: &str,
    ) -> Result<String, NodeError> {
        tx.verify_id()?;
        let raw = hex::decode(&tx.raw_data_hex).map_err(|_| NodeError::TxIdMismatch)?;
        let sig = hex::decode(signature_hex)
            .map_err(|_| NodeError::Broadcast("signature is not hex".into()))?;
        let body = serde_json::json!({
            "transaction": hex::encode(signed_transaction_bytes(&raw, &sig)),
        });
        let v = self.post("wallet/broadcasthex", body).await?;
        // Always our own id: it is sha256 of the bytes we signed, whatever the node echoes.
        if v.get("result").and_then(|b| b.as_bool()) == Some(true) {
            return Ok(tx.tx_id.clone());
        }
        let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .map(hex_to_text)
            .unwrap_or_default();
        // Re-broadcasting an already accepted transaction is fine.
        if code == "DUP_TRANSACTION_ERROR" {
            return Ok(tx.tx_id.clone());
        }
        Err(NodeError::Broadcast(format!("{code}: {msg}")))
    }

    /// Whether the chain knows the transaction: the full node (pending or included) or the
    /// solidity node (irreversible). `gettransactionbyid` answers `{}` for an unknown id.
    /// "Unknown" needs an answer from at least one of them; two failures are an error.
    pub async fn transaction_exists(&self, tx_id: &str) -> Result<bool, NodeError> {
        let body = serde_json::json!({ "value": tx_id, "visible": true });
        let known = |v: &serde_json::Value| {
            v.get("txID")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.eq_ignore_ascii_case(tx_id))
        };
        let full = self.post("wallet/gettransactionbyid", body.clone()).await;
        if full.as_ref().is_ok_and(known) {
            return Ok(true);
        }
        match self.post("walletsolidity/gettransactionbyid", body).await {
            Ok(v) => Ok(known(&v)),
            Err(e) => full.map(|v| known(&v)).map_err(|_| e),
        }
    }

    /// The account's active permission `id`, if it has one.
    pub async fn active_permission(
        &self,
        owner: &TronAddress,
        id: i32,
    ) -> Result<Option<Permission>, NodeError> {
        Ok(self
            .active_permissions(owner)
            .await?
            .into_iter()
            .find(|(i, _)| *i == id)
            .map(|(_, p)| p))
    }

    /// All active permissions of the account, with their ids.
    pub async fn active_permissions(
        &self,
        owner: &TronAddress,
    ) -> Result<Vec<(i32, Permission)>, NodeError> {
        let v = self
            .post(
                "wallet/getaccount",
                serde_json::json!({ "address": owner.as_str(), "visible": true }),
            )
            .await?;
        let Some(list) = v.get("active_permission").and_then(|a| a.as_array()) else {
            return Ok(Vec::new());
        };
        Ok(list
            .iter()
            .filter_map(|p| {
                let id = i32::try_from(p.get("id")?.as_i64()?).ok()?;
                let operations = p
                    .get("operations")
                    .and_then(|o| o.as_str())
                    .and_then(|o| hex::decode(o).ok())
                    .unwrap_or_default();
                let keys = p
                    .get("keys")
                    .and_then(|k| k.as_array())
                    .map(|keys| {
                        keys.iter()
                            .filter_map(|k| {
                                Some((
                                    k.get("address")?.as_str()?.to_owned(),
                                    k.get("weight")?.as_i64()?,
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some((
                    id,
                    Permission {
                        threshold: p.get("threshold").and_then(|t| t.as_i64()).unwrap_or(1),
                        keys,
                        operations,
                    },
                ))
            })
            .collect())
    }

    /// TRX balance in SUN and TRC-20 balances (raw units) as `getaccount` reports them.
    pub async fn balances(
        &self,
        address: &TronAddress,
    ) -> Result<(i64, Vec<(String, u128)>), NodeError> {
        let v = self
            .post(
                "wallet/getaccount",
                serde_json::json!({ "address": address.as_str(), "visible": true }),
            )
            .await?;
        let trx = v.get("balance").and_then(|b| b.as_i64()).unwrap_or(0);
        let mut trc20 = Vec::new();
        if let Some(list) = v.get("trc20").and_then(|t| t.as_array()) {
            for entry in list {
                if let Some(obj) = entry.as_object() {
                    for (contract, amount) in obj {
                        let n = amount
                            .as_str()
                            .and_then(|s| s.parse::<u128>().ok())
                            .unwrap_or(0);
                        trc20.push((contract.clone(), n));
                    }
                }
            }
        }
        Ok((trx, trc20))
    }
}

/// ABI encoding of `(address,uint256)`: 20-byte EVM address left-padded, then the amount.
pub fn trc20_transfer_param(to: &TronAddress, amount_raw: u128) -> String {
    let mut out = String::with_capacity(128);
    out.push_str(&"0".repeat(24));
    out.push_str(&hex::encode(&to.raw()[1..]));
    out.push_str(&format!("{amount_raw:064x}"));
    out
}

fn hex_to_text(h: &str) -> String {
    hex::decode(h)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| h.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trc20_param_layout() {
        let to: TronAddress = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".parse().unwrap();
        let p = trc20_transfer_param(&to, 1_000_000);
        assert_eq!(p.len(), 128);
        assert!(p.starts_with(&"0".repeat(24)));
        assert!(p.ends_with("f4240"));
    }

    #[test]
    fn node_url_must_be_https_unless_local() {
        assert!(check_node_url("https://api.trongrid.io").is_ok());
        assert!(check_node_url("http://localhost:8090").is_ok());
        assert!(check_node_url("http://127.0.0.1:8090/").is_ok());
        assert!(check_node_url("http://[::1]:8090").is_ok());
        assert!(check_node_url("http://api.trongrid.io").is_err());
        assert!(check_node_url("http://localhost.evil.example").is_err());
        assert!(check_node_url("http://localhost.evil.example").is_err());
        assert!(check_node_url("ftp://x").is_err());
        assert!(check_node_url("http://localhost:x@evil.example/").is_err());
        assert!(check_node_url("HTTP://LOCALHOST:8090").is_ok());
        assert!(check_node_url("http://127.0.0.2:8090").is_ok());
        assert!(check_node_url("not a url").is_err());
        assert!(check_node_url_with("http://10.0.0.5:8090", true).is_ok());
        assert!(check_node_url_with("ftp://10.0.0.5", true).is_err());
    }

    #[test]
    fn signed_transaction_encoding() {
        let raw = vec![0xab; 200];
        let sig = vec![0x01; 65];
        let b = signed_transaction_bytes(&raw, &sig);
        // field 1 with a two-byte length varint (200), then field 2 (65 bytes).
        assert_eq!(&b[..3], &[0x0a, 0xc8, 0x01]);
        assert_eq!(&b[3..203], &raw[..]);
        assert_eq!(&b[203..205], &[0x12, 65]);
        assert_eq!(&b[205..], &sig[..]);
    }

    #[test]
    fn txid_verification() {
        let raw = "0a0212";
        let id = hex::encode(Sha256::digest(hex::decode(raw).unwrap()));
        let ok = UnsignedTx {
            tx_id: id,
            raw_data_hex: raw.into(),
            rest: Default::default(),
        };
        assert!(ok.verify_id().is_ok());
        let bad = UnsignedTx {
            tx_id: "00".repeat(32),
            raw_data_hex: raw.into(),
            rest: Default::default(),
        };
        assert!(matches!(bad.verify_id(), Err(NodeError::TxIdMismatch)));
    }

    #[test]
    fn delegation_only_permission() {
        let signer: TronAddress = "TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7".parse().unwrap();
        let mut ops = vec![0u8; 32];
        ops[7] |= 1 << 1; // 57
        ops[7] |= 1 << 2; // 58
        let p = Permission {
            threshold: 1,
            keys: vec![(signer.to_string(), 1)],
            operations: ops.clone(),
        };
        assert_eq!(p.allowed_types(), vec![57, 58]);
        assert!(p.check_delegation_only(&signer).is_ok());
        let other: TronAddress = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".parse().unwrap();
        assert!(p.check_delegation_only(&other).is_err());
        let mut transfer = ops.clone();
        transfer[0] |= 1 << 1; // 1 = TransferContract
        let p2 = Permission {
            operations: transfer,
            ..p.clone()
        };
        assert!(p2
            .check_delegation_only(&signer)
            .unwrap_err()
            .contains("TransferContract"));
        let p3 = Permission {
            threshold: 2,
            ..p.clone()
        };
        assert!(p3.check_delegation_only(&signer).is_err());
        let p4 = Permission {
            operations: vec![0; 32],
            ..p
        };
        assert!(p4.check_delegation_only(&signer).is_err());
    }
}
