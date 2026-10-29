//! The few platform calls setup makes: who the key belongs to, and the shop's wallets.

use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use gtron_tron_signing::tron::{read_body, MAX_BODY};

use crate::printable::printable;

#[derive(Debug, Deserialize)]
pub struct Shop {
    pub name: String,
    pub mode: String,
    pub status: String,
    pub eligibility_issue: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Wallet {
    pub id: String,
    pub address: String,
    pub verified: bool,
    pub message_to_sign: Option<String>,
}

#[derive(Deserialize)]
struct List<T> {
    data: Vec<T>,
}

#[derive(Debug)]
pub struct Failure {
    /// HTTP status, or `None` when the platform could not be reached at all.
    pub status: Option<u16>,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Platform {
    http: reqwest::Client,
    url: String,
    key: String,
}

impl Platform {
    pub fn new(url: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("gtron-node/", env!("CARGO_PKG_VERSION"), " setup"))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: url.trim_end_matches('/').to_owned(),
            key: key.to_owned(),
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, Failure> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.key);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| Failure {
            status: None,
            message: format!("cannot reach {}: {}", self.url, root_cause(&e)),
        })?;
        let status = res.status();
        let text = read_body(res, MAX_BODY).await.map_err(|e| Failure {
            status: Some(status.as_u16()),
            message: format!("unexpected answer from {}: {e}", self.url),
        })?;
        if !status.is_success() {
            // The API's error envelope: {"error": {"message": "..."}}.
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(printable))
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(Failure {
                status: Some(status.as_u16()),
                message,
            });
        }
        serde_json::from_str(&text).map_err(|e| Failure {
            status: Some(status.as_u16()),
            message: format!("unexpected answer from {}: {e}", self.url),
        })
    }

    /// Its text fields are printed, so they come back without terminal escapes.
    pub async fn shop(&self) -> Result<Shop, Failure> {
        let s: Shop = self.call(reqwest::Method::GET, "/p1/shop", None).await?;
        Ok(Shop {
            name: printable(&s.name),
            mode: printable(&s.mode),
            status: printable(&s.status),
            eligibility_issue: s.eligibility_issue.as_deref().map(printable),
        })
    }

    pub async fn wallets(&self) -> Result<Vec<Wallet>, Failure> {
        let l: List<Wallet> = self
            .call(reqwest::Method::GET, "/p1/addresses", None)
            .await?;
        Ok(l.data)
    }

    pub async fn add_wallet(&self, address: &str) -> Result<Wallet, Failure> {
        self.call(
            reqwest::Method::POST,
            "/p1/addresses",
            Some(serde_json::json!({ "address": address })),
        )
        .await
    }

    pub async fn verify_wallet(&self, id: &str, signature: &str) -> Result<Wallet, Failure> {
        self.call(
            reqwest::Method::POST,
            &format!("/p1/addresses/{id}/verify"),
            Some(serde_json::json!({ "signature": signature })),
        )
        .await
    }
}

/// reqwest's own message is "error sending request for url (…)"; the cause says why.
fn root_cause(e: &reqwest::Error) -> String {
    let mut err: &dyn std::error::Error = e;
    while let Some(next) = err.source() {
        err = next;
    }
    err.to_string()
}

/// Whether `message` is exactly the platform's ownership proof for `address`
/// (gtron_app::marketplace::verification_message), and nothing else to sign.
pub fn is_verification_message(message: &str, address: &str) -> bool {
    let lines: Vec<&str> = message.split('\n').collect();
    let [title, shop, addr, code, outro] = lines[..] else {
        return false;
    };
    title == "gtron.network address verification"
        && shop.strip_prefix("Shop: ").is_some_and(|s| !s.is_empty())
        && addr == format!("Address: {address}")
        && code.strip_prefix("Code: ").is_some_and(|c| {
            !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        && outro == "Signing this message proves control of the wallet. It moves no funds."
        && !message.chars().any(|c| c.is_control() && c != '\n')
}

#[cfg(test)]
mod verification_tests {
    use super::is_verification_message;

    const A: &str = "TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7";

    fn msg(shop: &str, addr: &str, code: &str) -> String {
        format!(
            "gtron.network address verification\nShop: {shop}\nAddress: {addr}\nCode: {code}\nSigning this message proves control of the wallet. It moves no funds."
        )
    }

    #[test]
    fn only_the_exact_template_is_signed() {
        assert!(is_verification_message(
            &msg("Energy (prv_1)", A, "Ab12cd"),
            A
        ));
        assert!(!is_verification_message(
            &msg("Energy", "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t", "x1"),
            A
        ));
        assert!(!is_verification_message(
            &msg("Energy", A, "x1; transfer"),
            A
        ));
        assert!(!is_verification_message(&msg("E\x1b[2K", A, "x1"), A));
        assert!(!is_verification_message(
            &format!("{}\nmore", msg("E", A, "x1")),
            A
        ));
        assert!(!is_verification_message(&msg("E\nAddress: x", A, "x1"), A));
    }
}
