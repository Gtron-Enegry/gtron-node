//! The job loop: poll, check, sign, broadcast, report.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use gtron_tron_address::TronAddress;
use gtron_tron_signing::keys::{address_of, sign_raw_data};
use gtron_tron_signing::store::{Record, Store};
use gtron_tron_signing::tron::{read_body, Node, MAX_BODY};
use gtron_tron_signing::txcheck::{self, Bounds, Expected};
use k256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::backoff::{Backoff, RetryQueue};
use crate::job::{Job, JobKind, JobList, JobStatus};
use crate::limits::{self, CapEvent, LocalLimits};

/// After expiration, how long a broadcast transaction may still show up before "the node
/// does not know it" is taken to mean it never landed.
pub const EXPIRY_MARGIN_MS: i64 = 90_000;
const CAPS_BLOB: &str = "caps.json";
/// Long-poll wait asked of the platform; it answers at once when jobs are open.
const POLL_WAIT_SECS: u64 = 25;
/// A round that did no work lasts at least this long: the platform answers at once while
/// a job of ours is open (even one we are backing off from) or when it has no free
/// long-poll slot, and the loop must not spin on that.
const MIN_IDLE_ROUND: Duration = Duration::from_secs(2);
const POLL_RETRY: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(60));
/// Capped at a minute: a delegation that landed must still be reported before the job
/// expires, or the platform counts it as failed while the client has the energy.
pub const JOB_RETRY: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(60));
/// How long a finished or declined job is left alone if the platform still lists it.
const DONE_HOLD: Duration = Duration::from_secs(30);

/// Why a job did not finish, and whether it may go back to the platform.
#[derive(Debug)]
pub enum Fail {
    /// Nothing was signed for it (or what was signed provably never landed): decline.
    Decline(anyhow::Error),
    /// A transaction was signed for it and may land: keep the job, try again later.
    Retry(anyhow::Error),
}

fn decline(e: impl Into<anyhow::Error>) -> Fail {
    Fail::Decline(e.into())
}

fn retry(e: impl Into<anyhow::Error>) -> Fail {
    Fail::Retry(e.into())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What a stolen signing key could do with the wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScope {
    /// The wallet's own key: everything, including moving all its TRX.
    Owner,
    /// A key of this active permission, which only delegates and reclaims resources.
    DelegationOnly(i32),
}

pub struct Agent {
    pub http: reqwest::Client,
    pub api_url: String,
    pub api_key: String,
    pub key: SigningKey,
    /// The wallet whose stake is delegated.
    pub address: TronAddress,
    /// The wallet's active permission `key` signs under; None = `key` is the wallet's own.
    pub permission_id: Option<i32>,
    pub node: Node,
    pub lock: bool,
    pub limits: LocalLimits,
    pub store: Store,
    pub expiry_margin_ms: i64,
    pub retries: Mutex<RetryQueue>,
}

impl Agent {
    pub async fn api<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<T> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.api_url))
            .bearer_auth(&self.api_key);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = read_body(res, MAX_BODY)
            .await
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        if !status.is_success() {
            anyhow::bail!(
                "{path}: HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// What the signing key can do, as the chain says. `Err(Ok(..))`: the permission is
    /// not usable or allows more than delegating, `Err(Err(..))`: the node did not answer.
    pub async fn key_scope(&self) -> Result<KeyScope, Result<String, String>> {
        let Some(id) = self.permission_id else {
            return Ok(KeyScope::Owner);
        };
        let signer = address_of(&self.key);
        match self.node.active_permission(&self.address, id).await {
            Ok(Some(p)) => p
                .check_delegation_only(&signer)
                .map(|()| KeyScope::DelegationOnly(id))
                .map_err(|e| Ok(format!("permission {id} of {}: {e}", self.address))),
            Ok(None) => Err(Ok(format!(
                "{} has no active permission {id}",
                self.address
            ))),
            Err(e) => Err(Err(e.to_string())),
        }
    }

    pub fn events(&self) -> anyhow::Result<Vec<CapEvent>> {
        Ok(self.store.get_blob(CAPS_BLOB)?.unwrap_or_default())
    }

    /// Re-check the caps and record `ev`, dropping events that no longer count.
    fn add_event(&self, ev: CapEvent, now: i64) -> anyhow::Result<()> {
        let mut events = self.events()?;
        self.limits
            .check_caps(&events, &ev, now)
            .map_err(|r| anyhow::anyhow!(r))?;
        limits::record(&mut events, ev, now);
        self.store.put_blob(CAPS_BLOB, &events)?;
        Ok(())
    }

    fn remove_event(&self, job: &str) -> anyhow::Result<()> {
        let mut events = self.events()?;
        events.retain(|e| e.job != job);
        self.store.put_blob(CAPS_BLOB, &events)?;
        Ok(())
    }

    async fn report(&self, job: &Job, tx_id: &str) -> anyhow::Result<()> {
        self.api::<serde_json::Value>(
            reqwest::Method::POST,
            &format!("/p1/jobs/{}/delegated", job.id),
            Some(serde_json::json!({ "tx_id": tx_id })),
        )
        .await?;
        tracing::info!(job = %job.id, kind = %job.kind, energy = job.energy, tx = %tx_id, "job done");
        Ok(())
    }

    async fn handle(&self, job: &Job) -> Result<(), Fail> {
        let key_id = format!("job:{}", job.id);
        let receiver: TronAddress = job.target_address.parse().map_err(decline)?;
        let Some(balance_sun) = job.balance_sun else {
            return Err(decline(anyhow::anyhow!("job without balance_sun")));
        };
        let lock_blocks = match (job.kind, self.lock) {
            (JobKind::Delegate, true) => job.duration_seconds.map(|d| (i64::from(d) / 3).max(1)),
            _ => None,
        };
        let fp = hex::encode(Sha256::digest(
            serde_json::to_vec(&(
                job.kind.as_str(),
                self.address.as_str(),
                receiver.as_str(),
                balance_sun,
                lock_blocks,
            ))
            .expect("job serializes"),
        ));
        // Signed before: finish that transaction, never sign another for this job.
        if let Some(rec) = self.store.get_record(&key_id).map_err(retry)? {
            if rec.fingerprint != fp {
                return Err(retry(anyhow::anyhow!(
                    "job changed after transaction {} was signed for it; not signing again",
                    rec.tx.tx_id
                )));
            }
            return self.resume(job, &key_id, rec).await;
        }
        if job.kind == JobKind::Unknown {
            return Err(decline(anyhow::anyhow!(
                "job kind not supported by gtron-node {}; update the agent",
                env!("CARGO_PKG_VERSION")
            )));
        }
        if let Some(from) = &job.from_address {
            if from != self.address.as_str() {
                return Err(decline(anyhow::anyhow!(
                    "this agent controls {}, job expects {from}",
                    self.address
                )));
            }
        }
        // Before `take`: a job we would refuse anyway goes back to the platform untouched.
        self.limits
            .check(job.kind, &receiver, balance_sun, lock_blocks)
            .map_err(|reason| decline(anyhow::anyhow!(reason)))?;
        let now = now_ms();
        let event = (job.kind == JobKind::Delegate).then(|| CapEvent {
            job: job.id.clone(),
            at_ms: now,
            balance_sun,
            locked_until_ms: lock_blocks.map_or(now, |b| now.saturating_add(b * 3000)),
            receiver: Some(receiver.to_string()),
        });
        if let Some(ev) = &event {
            let events = self.events().map_err(decline)?;
            self.limits
                .check_caps(&events, ev, now)
                .map_err(|r| decline(anyhow::anyhow!(r)))?;
        }
        if job.status == JobStatus::Pending {
            self.api::<serde_json::Value>(
                reqwest::Method::POST,
                &format!("/p1/jobs/{}/take", job.id),
                None,
            )
            .await
            .map_err(decline)?;
        }
        let (tx, expected) = match job.kind {
            JobKind::Delegate => (
                self.node
                    .delegate_resource(
                        &self.address,
                        &receiver,
                        balance_sun,
                        lock_blocks,
                        self.permission_id,
                    )
                    .await
                    .map_err(decline)?,
                Expected::Delegate {
                    owner: &self.address,
                    receiver: &receiver,
                    balance_sun,
                    lock_blocks,
                },
            ),
            JobKind::Undelegate => (
                self.node
                    .undelegate_resource(&self.address, &receiver, balance_sun, self.permission_id)
                    .await
                    .map_err(decline)?,
                Expected::Undelegate {
                    owner: &self.address,
                    receiver: &receiver,
                    balance_sun,
                },
            ),
            JobKind::Unknown => unreachable!("declined above"),
        };
        // The node built the transaction: sign only if it is exactly this delegation.
        tx.verify_id().map_err(decline)?;
        let raw = hex::decode(&tx.raw_data_hex).map_err(decline)?;
        let checked = txcheck::check(
            &raw,
            &expected,
            &Bounds {
                max_fee_limit_sun: 0,
                permission_id: self.permission_id.unwrap_or(0),
                ..Bounds::default()
            },
            now_ms(),
        )
        .map_err(decline)?;
        let signature =
            sign_raw_data(&self.key, &tx.raw_data_hex).map_err(|e| decline(anyhow::anyhow!(e)))?;
        if let Some(ev) = event {
            self.add_event(ev, now_ms()).map_err(decline)?;
        }
        let mut rec = Record {
            fingerprint: fp,
            tx,
            signature,
            expiration_ms: checked.expiration_ms,
            result: serde_json::json!({ "job": job.id, "kind": job.kind.as_str() }),
            broadcast: false,
        };
        // Durable before it leaves the process; if this fails nothing was sent.
        if let Err(e) = self.store.put_record(&key_id, &rec) {
            // A failure after the rename leaves the record on disk, and a retry would
            // broadcast it: the stake goes back to the caps only if it is really gone.
            if matches!(self.store.get_record(&key_id), Ok(None)) {
                if let Err(e2) = self.remove_event(&job.id) {
                    tracing::error!(error = %e2, job = %job.id, "could not return the job's stake to the caps");
                }
            }
            return Err(decline(e));
        }
        let tx_id = rec.tx.tx_id.clone();
        self.node
            .broadcast(&rec.tx, &rec.signature)
            .await
            .map_err(|e| {
                retry(anyhow::anyhow!(
                    "transaction {tx_id} signed, broadcast failed: {e}"
                ))
            })?;
        rec.broadcast = true;
        if let Err(e) = self.store.put_record(&key_id, &rec) {
            tracing::error!(error = %e, tx = %tx_id, "broadcast not recorded; a retry re-sends the same transaction");
        }
        self.report(job, &tx_id).await.map_err(retry)
    }

    /// A job we already signed for: re-send that transaction and report it.
    async fn resume(&self, job: &Job, key_id: &str, mut rec: Record) -> Result<(), Fail> {
        let tx_id = rec.tx.tx_id.clone();
        if !rec.broadcast {
            let landed = match self.node.broadcast(&rec.tx, &rec.signature).await {
                Ok(_) => true,
                Err(e) if now_ms() <= rec.expiration_ms + self.expiry_margin_ms => {
                    return Err(retry(anyhow::anyhow!(
                        "re-broadcast of {tx_id} failed: {e}"
                    )));
                }
                Err(_) => self.node.transaction_exists(&tx_id).await.map_err(retry)?,
            };
            if !landed {
                // Expired well past the margin and unknown to the node, so the job goes
                // back. That answer comes from the node, which may be lying: the record
                // stays, so this job is never signed again, and its stake stays counted
                // in the caps until it ages out.
                return Err(decline(anyhow::anyhow!(
                    "signed transaction {tx_id} expired without landing"
                )));
            }
            rec.broadcast = true;
            if let Err(e) = self.store.put_record(key_id, &rec) {
                tracing::error!(error = %e, tx = %tx_id, "broadcast not recorded");
            }
        }
        self.report(job, &tx_id).await.map_err(retry)
    }

    /// Whether the platform took the decline back.
    async fn decline_job(&self, job: &Job, e: &anyhow::Error) -> bool {
        let reason = e.to_string().chars().take(200).collect::<String>();
        match self
            .api::<serde_json::Value>(
                reqwest::Method::POST,
                &format!("/p1/jobs/{}/decline", job.id),
                Some(serde_json::json!({ "reason": reason })),
            )
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(job = %job.id, error = %e, "decline failed");
                false
            }
        }
    }

    pub async fn process(&self, job: &Job) {
        match self.handle(job).await {
            Ok(()) => self
                .retries
                .lock()
                .unwrap()
                .hold(&job.id, Instant::now() + DONE_HOLD),
            Err(Fail::Decline(e)) => {
                tracing::error!(job = %job.id, error = %e, "job declined");
                let declined = self.decline_job(job, &e).await;
                let mut q = self.retries.lock().unwrap();
                if declined {
                    q.hold(&job.id, Instant::now() + DONE_HOLD);
                } else {
                    // Still listed as ours: without a backoff the next poll repeats this.
                    q.failed(&job.id, Instant::now());
                }
            }
            Err(Fail::Retry(e)) => {
                // Signed and possibly on-chain: declining would let the platform hand the
                // job to someone else while our delegation lands. Keep it and retry.
                let (delay, attempt) = self.retries.lock().unwrap().failed(&job.id, Instant::now());
                tracing::error!(job = %job.id, error = %e, attempt, retry_in = ?delay, "job signed but not finished");
            }
        }
    }

    async fn poll(&self) -> anyhow::Result<Vec<Job>> {
        let list: JobList = self
            .api(
                reqwest::Method::GET,
                &format!("/p1/jobs?wait={POLL_WAIT_SECS}"),
                None,
            )
            .await?;
        Ok(list
            .data
            .into_iter()
            .filter_map(|v| {
                serde_json::from_value::<Job>(v)
                    .inspect_err(
                        |e| tracing::warn!(error = %e, "skipping a job this agent cannot read"),
                    )
                    .ok()
            })
            .filter(|j| j.status.is_open())
            .collect())
    }

    /// One pass over the open jobs that are not backing off. Returns whether any was
    /// worked on. A job in progress is always finished before a stop is honoured.
    async fn round(&self, jobs: &[Job], stop: &watch::Receiver<bool>) -> bool {
        let due: Vec<&Job> = {
            let mut q = self.retries.lock().unwrap();
            q.retain_open(&jobs.iter().map(|j| j.id.as_str()).collect::<HashSet<_>>());
            let now = Instant::now();
            jobs.iter().filter(|j| q.is_due(&j.id, now)).collect()
        };
        for job in &due {
            if *stop.borrow() {
                break;
            }
            self.process(job).await;
        }
        !due.is_empty()
    }

    /// Runs until `stop` turns true; a stop interrupts waits and polls, never a job.
    pub async fn run(&self, mut stop: watch::Receiver<bool>) {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            address = %self.address,
            api = %self.api_url,
            "agent started"
        );
        let mut poll_backoff = Backoff::new(POLL_RETRY.0, POLL_RETRY.1);
        while !*stop.borrow() {
            let started = Instant::now();
            let polled = tokio::select! {
                r = self.poll() => r,
                _ = stop.wait_for(|s| *s) => break,
            };
            let jobs = match polled {
                Ok(jobs) => {
                    poll_backoff.reset();
                    jobs
                }
                Err(e) => {
                    let d = poll_backoff.next_delay();
                    tracing::warn!(error = %e, retry_in = ?d, "poll failed");
                    if pause(d, &mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            if !self.round(&jobs, &stop).await
                && pause(MIN_IDLE_ROUND.saturating_sub(started.elapsed()), &mut stop).await
            {
                break;
            }
        }
        let pending = self.retries.lock().unwrap().len();
        tracing::info!(
            unfinished_jobs = pending,
            "agent stopped; signed jobs resume on the next start"
        );
    }
}

/// Sleep for `d` unless a stop comes first; true if it did.
async fn pause(d: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stop.wait_for(|s| *s) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use gtron_tron_signing::keys::address_of;

    use super::*;
    use crate::limits::tests::{limits, CLIENT};

    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    fn varint(mut n: u64, out: &mut Vec<u8>) {
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

    fn int_field(field: u32, n: i64, out: &mut Vec<u8>) {
        varint(u64::from(field) << 3, out);
        varint(n as u64, out);
    }

    fn bytes_field(field: u32, b: &[u8], out: &mut Vec<u8>) {
        varint((u64::from(field) << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }

    fn delegate_raw(
        owner: &TronAddress,
        receiver: &TronAddress,
        balance: i64,
        lock_period: Option<i64>,
        permission_id: Option<i64>,
        expiration: i64,
    ) -> Vec<u8> {
        let mut value = Vec::new();
        bytes_field(1, owner.raw(), &mut value);
        int_field(2, 1, &mut value);
        int_field(3, balance, &mut value);
        bytes_field(4, receiver.raw(), &mut value);
        if let Some(p) = lock_period {
            int_field(5, 1, &mut value);
            int_field(6, p, &mut value);
        }
        let mut any = Vec::new();
        bytes_field(
            1,
            b"type.googleapis.com/protocol.DelegateResourceContract",
            &mut any,
        );
        bytes_field(2, &value, &mut any);
        let mut c = Vec::new();
        int_field(1, 57, &mut c);
        bytes_field(2, &any, &mut c);
        if let Some(p) = permission_id {
            int_field(5, p, &mut c);
        }
        let mut raw = Vec::new();
        bytes_field(1, &[0x07, 0x90], &mut raw);
        bytes_field(
            4,
            &[0xe1, 0xb9, 0xde, 0x55, 0x96, 0x65, 0xc6, 0x71],
            &mut raw,
        );
        int_field(8, expiration, &mut raw);
        bytes_field(11, &c, &mut raw);
        int_field(14, expiration - 1000, &mut raw);
        raw
    }

    /// The platform and a TRON node in one server.
    #[derive(Default)]
    struct Mock {
        builds: AtomicUsize,
        broadcasts: AtomicUsize,
        polls: AtomicUsize,
        /// What `GET /p1/jobs` lists; it answers at once, like the platform with open jobs.
        open: Mutex<Vec<serde_json::Value>>,
        /// `GET /p1/jobs` holds the request this long when nothing is open.
        poll_hold_ms: AtomicI64,
        fail_polls: AtomicBool,
        fail_declines: AtomicBool,
        expiration_ms: AtomicI64,
        lose_replies: AtomicUsize,
        refuse: AtomicBool,
        fail_report: AtomicUsize,
        accepted: Mutex<Vec<String>>,
        reported: Mutex<Vec<String>>,
        declined: Mutex<Vec<String>>,
    }

    async fn build(
        State(m): State<Arc<Mock>>,
        Json(b): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        m.builds.fetch_add(1, Ordering::SeqCst);
        let owner: TronAddress = b["owner_address"].as_str().unwrap().parse().unwrap();
        let receiver: TronAddress = b["receiver_address"].as_str().unwrap().parse().unwrap();
        let raw = delegate_raw(
            &owner,
            &receiver,
            b["balance"].as_i64().unwrap(),
            b["lock_period"].as_i64(),
            b["Permission_id"].as_i64(),
            now_ms() + m.expiration_ms.load(Ordering::SeqCst),
        );
        Json(serde_json::json!({
            "txID": hex::encode(Sha256::digest(&raw)),
            "raw_data_hex": hex::encode(&raw),
            "raw_data": {},
            "visible": true,
        }))
    }

    async fn broadcast(
        State(m): State<Arc<Mock>>,
        Json(b): Json<serde_json::Value>,
    ) -> axum::response::Response {
        m.broadcasts.fetch_add(1, Ordering::SeqCst);
        let signed = hex::decode(b["transaction"].as_str().unwrap()).unwrap();
        // Field 1 (`raw_data`): tag, varint length, bytes.
        assert_eq!(signed[0], 0x0a);
        let (mut len, mut shift, mut i) = (0usize, 0, 1);
        loop {
            let b = signed[i];
            len |= usize::from(b & 0x7f) << shift;
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let id = hex::encode(Sha256::digest(&signed[i..i + len]));
        if m.refuse.load(Ordering::SeqCst) {
            return Json(
                serde_json::json!({ "result": false, "code": "TRANSACTION_EXPIRATION_ERROR" }),
            )
            .into_response();
        }
        let mut accepted = m.accepted.lock().unwrap();
        if accepted.contains(&id) {
            return Json(serde_json::json!({ "result": false, "code": "DUP_TRANSACTION_ERROR" }))
                .into_response();
        }
        accepted.push(id);
        if m.lose_replies.load(Ordering::SeqCst) > 0 {
            m.lose_replies.fetch_sub(1, Ordering::SeqCst);
            return (StatusCode::BAD_GATEWAY, "upstream timeout").into_response();
        }
        Json(serde_json::json!({ "result": true })).into_response()
    }

    async fn by_id(
        State(m): State<Arc<Mock>>,
        Json(b): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let id = b["value"].as_str().unwrap();
        if m.accepted.lock().unwrap().iter().any(|a| a == id) {
            Json(serde_json::json!({ "txID": id }))
        } else {
            Json(serde_json::json!({}))
        }
    }

    async fn list_jobs(State(m): State<Arc<Mock>>) -> axum::response::Response {
        m.polls.fetch_add(1, Ordering::SeqCst);
        if m.fail_polls.load(Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE, "maintenance").into_response();
        }
        let open = m.open.lock().unwrap().clone();
        if open.is_empty() {
            let hold = m.poll_hold_ms.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(hold.max(0) as u64)).await;
        }
        Json(serde_json::json!({ "object": "list", "data": open, "has_more": false }))
            .into_response()
    }

    async fn job_action(
        State(m): State<Arc<Mock>>,
        UrlPath((id, action)): UrlPath<(String, String)>,
        body: Option<Json<serde_json::Value>>,
    ) -> axum::response::Response {
        match action.as_str() {
            "take" => {}
            "delegated" => {
                if m.fail_report.load(Ordering::SeqCst) > 0 {
                    m.fail_report.fetch_sub(1, Ordering::SeqCst);
                    return (StatusCode::SERVICE_UNAVAILABLE, "{}").into_response();
                }
                let tx = body.unwrap()["tx_id"].as_str().unwrap().to_owned();
                m.reported.lock().unwrap().push(tx);
            }
            "decline" => {
                if m.fail_declines.load(Ordering::SeqCst) {
                    return (StatusCode::SERVICE_UNAVAILABLE, "{}").into_response();
                }
                m.declined.lock().unwrap().push(id)
            }
            _ => return StatusCode::NOT_FOUND.into_response(),
        }
        Json(serde_json::json!({})).into_response()
    }

    async fn agent(margin_ms: i64) -> (Arc<Mock>, Arc<Agent>) {
        let m = Arc::new(Mock::default());
        m.expiration_ms.store(60_000, Ordering::SeqCst);
        let app = Router::new()
            .route("/wallet/delegateresource", post(build))
            .route("/wallet/broadcasthex", post(broadcast))
            .route("/wallet/gettransactionbyid", post(by_id))
            .route("/walletsolidity/gettransactionbyid", post(by_id))
            .route("/p1/jobs", get(list_jobs))
            .route("/p1/jobs/{id}/{action}", post(job_action))
            .with_state(m.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let key = SigningKey::from_slice(&hex::decode(KEY).unwrap()).unwrap();
        let a = Agent {
            http: reqwest::Client::new(),
            api_url: url.clone(),
            api_key: "pk_test".into(),
            address: address_of(&key),
            key,
            permission_id: None,
            node: Node::new(&url, None, Duration::from_secs(5)),
            lock: true,
            limits: limits(),
            store: Store::memory(),
            expiry_margin_ms: margin_ms,
            retries: Mutex::new(RetryQueue::new(JOB_RETRY.0, JOB_RETRY.1)),
        };
        (m, Arc::new(a))
    }

    fn job_json(id: &str, kind: &str, status: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": kind,
            "target_address": CLIENT,
            "energy": 65_000,
            "balance_sun": 500_000_000,
            "duration_seconds": 3600,
            "status": status,
        })
    }

    #[tokio::test]
    async fn a_permission_key_signs_for_the_wallet_under_its_permission() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        let mut a = Arc::into_inner(a).unwrap();
        let wallet: TronAddress = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".parse().unwrap();
        a.address = wallet.clone();
        a.permission_id = Some(2);
        let mut j = job_json("jp", "delegate", "pending");
        j["from_address"] = serde_json::json!(wallet.as_str());
        a.process(&serde_json::from_value(j).unwrap()).await;
        assert_eq!(m.accepted.lock().unwrap().len(), 1);
        assert!(m.declined.lock().unwrap().is_empty());
        // The same job for the key's own address is not this agent's.
        let mut j = job_json("jq", "delegate", "pending");
        j["from_address"] = serde_json::json!(address_of(&a.key).as_str());
        a.process(&serde_json::from_value(j).unwrap()).await;
        assert_eq!(m.accepted.lock().unwrap().len(), 1);
        assert_eq!(*m.declined.lock().unwrap(), vec!["jq".to_owned()]);
    }

    fn job(id: &str, status: &str) -> Job {
        serde_json::from_value(job_json(id, "delegate", status)).unwrap()
    }

    #[tokio::test]
    async fn lost_reply_and_failed_report_never_sign_twice_or_decline() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        m.lose_replies.store(1, Ordering::SeqCst);
        a.process(&job("j1", "pending")).await;
        assert_eq!(m.accepted.lock().unwrap().len(), 1, "the node did take it");
        assert!(m.reported.lock().unwrap().is_empty());

        m.fail_report.store(1, Ordering::SeqCst);
        a.process(&job("j1", "taken")).await;
        assert!(m.reported.lock().unwrap().is_empty());

        a.process(&job("j1", "taken")).await;
        let tx = m.accepted.lock().unwrap()[0].clone();
        assert_eq!(*m.reported.lock().unwrap(), vec![tx]);
        assert_eq!(
            m.builds.load(Ordering::SeqCst),
            1,
            "never built a second transaction"
        );
        assert!(
            m.declined.lock().unwrap().is_empty(),
            "a signed job is never declined"
        );
        assert_eq!(a.events().unwrap().len(), 1, "counted once");
        assert!(
            !a.retries.lock().unwrap().is_due("j1", Instant::now()),
            "a finished job is not picked up again at once"
        );
    }

    #[tokio::test]
    async fn expired_unlanded_transaction_declines_without_resigning() {
        let (m, a) = agent(200).await;
        m.refuse.store(true, Ordering::SeqCst);
        m.expiration_ms.store(500, Ordering::SeqCst);
        a.process(&job("j2", "pending")).await;
        assert!(m.declined.lock().unwrap().is_empty(), "may still land");
        a.process(&job("j2", "taken")).await;
        assert!(
            m.declined.lock().unwrap().is_empty(),
            "inside expiration + margin"
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        a.process(&job("j2", "taken")).await;
        assert_eq!(*m.declined.lock().unwrap(), vec!["j2".to_owned()]);
        // "Never landed" is the node's word: the record and the caps keep the job.
        assert!(a.store.get_record("job:j2").unwrap().is_some());
        assert_eq!(a.events().unwrap().len(), 1, "stake stays counted");
        // Offered again, the job is declined again, never signed a second time.
        a.process(&job("j2", "pending")).await;
        assert_eq!(m.declined.lock().unwrap().len(), 2);
        assert_eq!(m.builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn caps_refuse_before_taking() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        for i in 0..4 {
            a.process(&job(&format!("c{i}"), "pending")).await;
        }
        // 4 × 500 TRX locked hits the 2 000 TRX locked cap on the fifth.
        assert_eq!(m.reported.lock().unwrap().len(), 4);
        a.process(&job("c4", "pending")).await;
        assert_eq!(*m.declined.lock().unwrap(), vec!["c4".to_owned()]);
        assert_eq!(m.builds.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn unknown_kind_is_declined_not_skipped() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        let j: Job = serde_json::from_value(job_json("u1", "freeze", "pending")).unwrap();
        a.process(&j).await;
        assert_eq!(*m.declined.lock().unwrap(), vec!["u1".to_owned()]);
        assert_eq!(m.builds.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stuck_job_backs_off_instead_of_spinning() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        // Signed, the broadcast reply is lost, and every re-broadcast is a duplicate: the
        // job stays `taken` and the platform lists it at once on every poll.
        m.lose_replies.store(1, Ordering::SeqCst);
        *m.open.lock().unwrap() = vec![job_json("s1", "delegate", "taken")];
        run_for(&a, 3.0).await;
        let polls = m.polls.load(Ordering::SeqCst);
        let broadcasts = m.broadcasts.load(Ordering::SeqCst);
        // Without pacing this is thousands of each in three seconds. With it: a poll that
        // works the job, one that finds it backing off, and the same pair again at ~2 s.
        assert!(polls <= 4, "polls: {polls}");
        assert!((1..=2).contains(&broadcasts), "broadcasts: {broadcasts}");
        assert_eq!(m.builds.load(Ordering::SeqCst), 1, "never signed twice");
        assert!(m.declined.lock().unwrap().is_empty());
        // The re-broadcast at ~2 s is a duplicate, i.e. it landed: reported once, and the
        // list that still shows the job does not pull it back in.
        assert!(m.reported.lock().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn stop_interrupts_a_long_poll() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        m.poll_hold_ms.store(25_000, Ordering::SeqCst);
        // `run_for` fails unless the loop stops within a second of the stop.
        run_for(&a, 0.2).await;
        assert_eq!(m.polls.load(Ordering::SeqCst), 1);
    }

    /// Run the loop for `secs`, then stop it; panics if it does not stop within a second.
    async fn run_for(a: &Arc<Agent>, secs: f64) {
        let (tx, rx) = watch::channel(false);
        let runner = tokio::spawn({
            let a = a.clone();
            async move { a.run(rx).await }
        });
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), runner)
            .await
            .expect("stops promptly")
            .unwrap();
    }

    #[tokio::test]
    async fn failed_polls_back_off() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        m.fail_polls.store(true, Ordering::SeqCst);
        run_for(&a, 3.0).await;
        // 0 s, then ~2 s, then ~6 s.
        let polls = m.polls.load(Ordering::SeqCst);
        assert!((1..=2).contains(&polls), "polls: {polls}");
    }

    #[tokio::test]
    async fn failed_decline_backs_off() {
        let (m, a) = agent(EXPIRY_MARGIN_MS).await;
        m.fail_declines.store(true, Ordering::SeqCst);
        // Over this agent's per-delegation limit: declined before anything is signed.
        let mut big = job_json("d1", "delegate", "pending");
        big["balance_sun"] = serde_json::json!(5_000_000_000_i64);
        *m.open.lock().unwrap() = vec![big];
        run_for(&a, 3.0).await;
        let polls = m.polls.load(Ordering::SeqCst);
        assert!(polls <= 4, "polls: {polls}");
        assert_eq!(m.builds.load(Ordering::SeqCst), 0);
    }
}
