//! Command line / environment, the wallet key and the private `.env`.

use std::path::{Path, PathBuf};

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Parser};
use gtron_tron_address::TronAddress;
use gtron_tron_signing::secretfile::check_private_file;
use zeroize::Zeroizing;

use crate::limits::LocalLimits;

#[derive(Parser)]
#[command(
    name = "gtron-node",
    version,
    about = "gtron.network marketplace agent",
    after_help = "First time here? `gtron-node setup` asks for everything and installs the service."
)]
pub struct Cli {
    #[arg(long, env = "NODE_API_URL")]
    pub api_url: String,
    #[arg(long, env = "NODE_API_KEY", hide_env_values = true)]
    pub api_key: String,
    #[arg(long, env = "TRONGRID_URL", default_value = "https://api.trongrid.io")]
    pub trongrid_url: String,
    #[arg(long, env = "TRONGRID_API_KEY", hide_env_values = true)]
    pub trongrid_api_key: Option<String>,
    /// Where signed transactions and the spend log are kept. One agent per directory.
    #[arg(long, env = "NODE_STATE_DIR", default_value = ".gtron-node")]
    pub state_dir: PathBuf,
    /// Lock delegations for the rental period so the wallet cannot reclaim early by mistake.
    /// `true`/`false`, also `1`/`0`, `yes`/`no`, `on`/`off`.
    #[arg(
        long,
        env = "NODE_LOCK_DELEGATIONS",
        default_value_t = true,
        action = ArgAction::Set,
        value_parser = BoolishValueParser::new()
    )]
    pub lock: bool,
    /// Largest delegation this agent signs, in SUN of stake (default 50 000 TRX).
    #[arg(long, env = "NODE_MAX_BALANCE_SUN", default_value_t = 50_000_000_000)]
    pub max_balance_sun: i64,
    /// Most stake delegated in any 24 hours, in SUN (default 200 000 TRX).
    #[arg(
        long,
        env = "NODE_MAX_DAILY_BALANCE_SUN",
        default_value_t = 200_000_000_000
    )]
    pub max_daily_balance_sun: i64,
    /// Most stake in delegations whose lock has not run out yet, in SUN (default 500 000 TRX).
    /// Unlocked delegations do not count: the wallet can reclaim them at any time, and
    /// only the per-delegation and 24h caps bound them.
    #[arg(
        long,
        env = "NODE_MAX_LOCKED_BALANCE_SUN",
        default_value_t = 500_000_000_000
    )]
    pub max_locked_balance_sun: i64,
    /// Longest lock this agent accepts, in seconds (default 30 days).
    #[arg(long, env = "NODE_MAX_LOCK_SECONDS", default_value_t = 30 * 24 * 3600)]
    pub max_lock_seconds: i64,
    /// Only delegate to these addresses (comma list).
    #[arg(long, env = "NODE_ALLOWED_RECEIVERS", value_delimiter = ',')]
    pub allowed_receivers: Vec<String>,
    /// Delegate to any receiver the platform names. Required when no allowlist is set.
    /// The docs (and the error without an allowlist) say `NODE_ALLOW_ANY_RECEIVER=1`, so
    /// `1`/`yes`/`on` count as well as `true`.
    #[arg(
        long,
        env = "NODE_ALLOW_ANY_RECEIVER",
        default_value_t = false,
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = BoolishValueParser::new()
    )]
    pub allow_any_receiver: bool,
    /// Print the wallet address and exit.
    #[arg(long)]
    pub address: bool,
    /// Check the configuration, the platform key and the TRON node, then exit (0 = ready).
    #[arg(long)]
    pub check: bool,
    /// Settings file (`KEY=value` lines, owned by you, mode 0600). `gtron-node setup` writes
    /// one. Read before everything else; values already in the environment win.
    #[arg(long, env = "NODE_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,
}

impl Cli {
    pub fn limits(&self) -> anyhow::Result<LocalLimits> {
        let allowed_receivers = self
            .allowed_receivers
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<TronAddress>()
                    .map_err(|e| anyhow::anyhow!("NODE_ALLOWED_RECEIVERS: {s}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if allowed_receivers.is_empty() && !self.allow_any_receiver {
            anyhow::bail!(
                "set NODE_ALLOWED_RECEIVERS, or NODE_ALLOW_ANY_RECEIVER=1 to delegate to whoever the platform names"
            );
        }
        for (name, v) in [
            ("NODE_MAX_BALANCE_SUN", self.max_balance_sun),
            ("NODE_MAX_DAILY_BALANCE_SUN", self.max_daily_balance_sun),
            ("NODE_MAX_LOCKED_BALANCE_SUN", self.max_locked_balance_sun),
            ("NODE_MAX_LOCK_SECONDS", self.max_lock_seconds),
        ] {
            if v <= 0 {
                anyhow::bail!("{name} must be positive, got {v}");
            }
        }
        Ok(LocalLimits {
            max_balance_sun: self.max_balance_sun,
            max_lock_blocks: self.max_lock_seconds / 3,
            allowed_receivers,
            max_daily_sun: self.max_daily_balance_sun,
            max_locked_sun: self.max_locked_balance_sun,
        })
    }
}

/// `NODE_PRIVATE_KEY_FILE` (preferred) or `NODE_PRIVATE_KEY`, removed from the environment
/// after reading. Runs before the async runtime starts any thread.
pub fn private_key() -> anyhow::Result<Zeroizing<String>> {
    let key = match std::env::var("NODE_PRIVATE_KEY_FILE")
        .ok()
        .filter(|p| !p.is_empty())
    {
        Some(path) => {
            check_private_file(&path).map_err(|e| anyhow::anyhow!("NODE_PRIVATE_KEY_FILE: {e}"))?;
            let raw = Zeroizing::new(std::fs::read_to_string(&path)?);
            Zeroizing::new(raw.trim().to_owned())
        }
        None => Zeroizing::new(std::env::var("NODE_PRIVATE_KEY").map_err(|_| {
            anyhow::anyhow!("NODE_PRIVATE_KEY_FILE or NODE_PRIVATE_KEY is required")
        })?),
    };
    std::env::remove_var("NODE_PRIVATE_KEY");
    Ok(key)
}

/// `--config FILE` / `--config=FILE` / `NODE_CONFIG`, found before clap runs: the file
/// supplies the values clap reads from the environment.
fn config_path() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(p) = a.to_str().and_then(|s| s.strip_prefix("--config=")) {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("NODE_CONFIG")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Load the `--config` file, if one is named. Unlike `./.env`, a named file that cannot be
/// used is an error: running with half the settings would be worse than not starting.
pub fn load_config() -> Result<(), String> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    check_private_file(&path).map_err(|e| format!("--config: {e}"))?;
    dotenvy::from_path(&path).map_err(|e| format!("--config {}: {e}", path.display()))
}

/// Load `./.env` only if nobody else could have written or read it. Returns a warning to
/// log once logging is up (the filter itself may come from `.env`).
pub fn load_dotenv(path: &Path) -> Option<String> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    match check_private_file(path) {
        Ok(()) => dotenvy::from_path(path)
            .err()
            .map(|e| format!("{}: {e}", path.display())),
        Err(e) => Some(format!("not loading {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_is_not_a_flag() {
        // Only env/file: the flag would put the key into `ps` and shell history.
        assert!(Cli::try_parse_from([
            "gtron-node",
            "--api-url",
            "https://x",
            "--api-key",
            "pk",
            "--private-key",
            "01"
        ])
        .is_err());
    }

    #[test]
    fn boolean_settings_take_what_the_docs_say() {
        let base = ["gtron-node", "--api-url", "https://x", "--api-key", "pk"];
        let parse = |extra: &[&str]| {
            let mut args = base.to_vec();
            args.extend_from_slice(extra);
            Cli::try_parse_from(args).unwrap()
        };
        let c = parse(&[]);
        assert!(c.lock, "delegations are locked unless turned off");
        assert!(!c.allow_any_receiver);
        assert!(parse(&["--allow-any-receiver"]).allow_any_receiver);
        assert!(parse(&["--allow-any-receiver=1"]).allow_any_receiver);
        assert!(!parse(&["--allow-any-receiver=0"]).allow_any_receiver);
        assert!(!parse(&["--lock=false"]).lock);
        assert!(!parse(&["--lock=0"]).lock);
        assert!(parse(&["--lock=yes"]).lock);
    }

    #[test]
    fn limits_must_be_positive() {
        let cli = |extra: &[&str]| {
            let mut args = vec![
                "gtron-node",
                "--api-url",
                "https://x",
                "--api-key",
                "pk",
                "--allow-any-receiver",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args).unwrap()
        };
        assert!(cli(&[]).limits().is_ok());
        assert!(cli(&["--max-daily-balance-sun", "0"]).limits().is_err());
        assert!(cli(&["--max-lock-seconds=-1"]).limits().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dotenv_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gtron-node-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        assert!(load_dotenv(&p).is_none(), "absent is fine");
        std::fs::write(&p, "GTRON_NODE_DOTENV_TEST=1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_dotenv(&p).is_some());
        assert!(std::env::var("GTRON_NODE_DOTENV_TEST").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
