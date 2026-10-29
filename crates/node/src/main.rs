//! `gtron-node`: reference agent for agent-mode marketplace shops.
//!
//! Polls `GET /p1/jobs?wait=25` with a `pk_` key, delegates energy from this wallet
//! with its own private key through a TRON node, and reports the transaction back.
//! The key never leaves this process. Runs anywhere with outbound HTTPS.
//!
//! Env: NODE_API_URL (https://api.example.com), NODE_API_KEY (pk_live_…),
//! NODE_PRIVATE_KEY (hex) or NODE_PRIVATE_KEY_FILE (path, mode 0400; preferred),
//! TRONGRID_URL (https; TRONGRID_ALLOW_HTTP=1 for a private plain-http node),
//! TRONGRID_API_KEY, NODE_STATE_DIR (.gtron-node), NODE_LOCK_DELEGATIONS (true),
//! NODE_MAX_BALANCE_SUN, NODE_MAX_DAILY_BALANCE_SUN, NODE_MAX_LOCKED_BALANCE_SUN,
//! NODE_MAX_LOCK_SECONDS, NODE_ALLOWED_RECEIVERS or NODE_ALLOW_ANY_RECEIVER=1.
//! A `.env` in the working directory is read only if it is owned by this user and not
//! accessible to group/others.
//!
//! The private key is deliberately not a command-line flag: arguments end up in shell
//! history and `ps`. The agent refuses jobs above its own local limits (per delegation,
//! per 24 hours, and in total still locked) and checks every node-built transaction
//! before signing it. Each signed transaction is written to NODE_STATE_DIR before it is
//! broadcast, so a retry of a job re-sends that same transaction instead of signing a
//! second one; within those limits neither a compromised platform nor a lying TRON node
//! can make it sign more than one delegation per job.
//!
//! Failed polls and jobs that are signed but not finished back off exponentially (up to a
//! minute), so an outage of the platform or of the TRON node does not turn into a request
//! storm. SIGTERM / Ctrl+C stop the agent after the job in hand; a second Ctrl+C exits at
//! once, which is safe too: whatever was signed is on disk and resumes on the next start.

mod agent;
mod backoff;
mod check;
mod config;
mod job;
mod limits;
mod setup;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use gtron_tron_address::TronAddress;
use gtron_tron_signing::keys::address_of;
use gtron_tron_signing::store::{Store, StoreError};
use gtron_tron_signing::tron::{
    allow_plain_http_from_env, check_node_url, check_node_url_with, Node,
};
use k256::ecdsa::SigningKey;
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::agent::{Agent, EXPIRY_MARGIN_MS, JOB_RETRY};
use crate::backoff::RetryQueue;
use crate::config::Cli;
use crate::limits::LocalLimits;

fn main() -> anyhow::Result<ExitCode> {
    if std::env::args_os().nth(1).is_some_and(|a| a == "setup") {
        return setup::main();
    }
    let config = config::load_config();
    let dotenv_warning = config::load_dotenv(Path::new(".env"));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Some(w) = dotenv_warning {
        tracing::warn!("{w}");
    }
    config.map_err(anyhow::Error::msg)?;
    let cli = Cli::parse();
    // Parsed into `cli`; keep them out of the environment of anything we might spawn.
    std::env::remove_var("NODE_API_KEY");
    std::env::remove_var("TRONGRID_API_KEY");
    let hex_key = config::private_key()?;
    let raw = Zeroizing::new(hex::decode(hex_key.trim().trim_start_matches("0x"))?);
    drop(hex_key);
    let key = SigningKey::from_slice(&raw)?;
    let address = address_of(&key);
    if cli.address {
        println!("{address}");
        return Ok(ExitCode::SUCCESS);
    }
    // The pk_ key and the job stream must not travel in clear text.
    check_node_url(&cli.api_url)
        .map_err(|_| anyhow::anyhow!("NODE_API_URL must use https (http only for localhost)"))?;
    check_node_url_with(&cli.trongrid_url, allow_plain_http_from_env())?;
    let limits = cli.limits()?;
    // `--check` next to a running agent: its directory lock is expected, not an error.
    let (store, state_note) = match Store::open(&cli.state_dir) {
        Ok(s) => (s, None),
        Err(StoreError::Locked(d)) if cli.check => (
            Store::memory(),
            Some(format!("{d} is in use by a running agent")),
        ),
        Err(e) => return Err(e.into()),
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, key, address, limits, store, state_note))
}

async fn run(
    cli: Cli,
    key: SigningKey,
    address: TronAddress,
    limits: LocalLimits,
    store: Store,
    state_note: Option<String>,
) -> anyhow::Result<ExitCode> {
    let agent = Agent {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(40))
            // Lets the platform see which agent versions are still out there.
            .user_agent(concat!("gtron-node/", env!("CARGO_PKG_VERSION")))
            .build()?,
        api_url: cli.api_url.trim_end_matches('/').to_owned(),
        api_key: cli.api_key,
        key,
        address,
        node: Node::new(
            &cli.trongrid_url,
            cli.trongrid_api_key,
            Duration::from_secs(15),
        ),
        lock: cli.lock,
        limits,
        store,
        expiry_margin_ms: EXPIRY_MARGIN_MS,
        retries: Mutex::new(RetryQueue::new(JOB_RETRY.0, JOB_RETRY.1)),
    };
    if cli.check {
        let ready = check::run(&agent, state_note.as_deref()).await;
        return Ok(if ready {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("stop requested; finishing the job in hand (Ctrl+C again to exit now)");
        let _ = stop_tx.send(true);
        shutdown_signal().await;
        tracing::warn!("exiting now; signed jobs resume on the next start");
        std::process::exit(130);
    });
    agent.run(stop_rx).await;
    Ok(ExitCode::SUCCESS)
}

/// Ctrl+C, or SIGTERM from systemd / `docker stop`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!(error = %e, "no SIGTERM handler; only Ctrl+C stops cleanly"),
        }
    }
    if tokio::signal::ctrl_c().await.is_err() {
        // No signal handling at all: never report a stop.
        std::future::pending::<()>().await;
    }
}
