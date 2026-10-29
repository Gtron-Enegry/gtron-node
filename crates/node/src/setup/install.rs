//! Where setup puts things, and the commands that start the agent.
//!
//! Every target gets the same three things: a settings file (mode 0600), the wallet key
//! in its own file (mode 0400, owned by the user the agent runs as) and a state
//! directory (0700) that is never touched if it already exists.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use gtron_tron_address::TronAddress;
use zeroize::Zeroizing;

pub const SYSTEMD_UNIT: &str = include_str!("../../dist/gtron-node.service");
pub const DOCKER_IMAGE: &str = concat!(
    "ghcr.io/gtron-enegry/gtron-node:",
    env!("CARGO_PKG_VERSION")
);
const SYSTEM_BIN: &str = "/usr/local/bin/gtron-node";
const SYSTEM_ETC: &str = "/etc/gtron-node";
const SERVICE_USER: &str = "gtron-node";
/// The image's non-root user (crates/node/dist/Dockerfile).
const IMAGE_UID: u32 = 10001;
const LAUNCHD_LABEL: &str = "network.gtron.node";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Systemd,
    Docker,
    Launchd,
    Manual,
}

/// What this machine offers.
pub struct Host {
    pub root: bool,
    pub systemd: bool,
    pub docker: bool,
    pub macos: bool,
    pub home: Option<PathBuf>,
}

impl Host {
    pub fn detect() -> Self {
        let docker = Command::new("docker")
            .args(["compose", "version"])
            .output()
            .is_ok_and(|o| o.status.success())
            && Command::new("docker")
                .arg("info")
                .output()
                .is_ok_and(|o| o.status.success());
        Self {
            root: gtron_tron_signing::secretfile::effective_uid() == 0,
            systemd: cfg!(target_os = "linux") && Path::new("/run/systemd/system").is_dir(),
            docker,
            macos: cfg!(target_os = "macos"),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// `~/.gtron-node`: settings, key and state for launchd and manual runs.
    pub fn user_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self
            .home
            .clone()
            .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?
            .join(".gtron-node"))
    }

    pub fn docker_dir(&self) -> anyhow::Result<PathBuf> {
        if self.root {
            Ok(PathBuf::from("/opt/gtron-node"))
        } else {
            Ok(self
                .home
                .clone()
                .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?
                .join("gtron-node"))
        }
    }
}

pub enum Receivers {
    Any,
    Only(Vec<TronAddress>),
}

pub struct Settings {
    pub api_url: String,
    pub api_key: Zeroizing<String>,
    pub trongrid_url: String,
    pub trongrid_api_key: Option<Zeroizing<String>>,
    pub allow_plain_http_node: bool,
    pub receivers: Receivers,
    pub max_trx: u64,
    pub max_daily_trx: u64,
    pub max_locked_trx: u64,
    pub max_lock_days: u64,
}

const SUN_PER_TRX: u64 = 1_000_000;

impl Settings {
    /// `KEY=value` lines for systemd's EnvironmentFile, Docker's env_file and `--config`.
    /// `extra` are the paths that differ per target.
    pub fn env_file(&self, extra: &[(&str, String)]) -> Zeroizing<String> {
        let mut s = Zeroizing::new(String::from("# Written by gtron-node setup\n"));
        let mut line = |k: &str, v: &str| {
            let _ = writeln!(s, "{k}={v}");
        };
        line("NODE_API_URL", &self.api_url);
        line("NODE_API_KEY", &self.api_key);
        line("TRONGRID_URL", &self.trongrid_url);
        if let Some(k) = &self.trongrid_api_key {
            line("TRONGRID_API_KEY", k);
        }
        if self.allow_plain_http_node {
            line("TRONGRID_ALLOW_HTTP", "1");
        }
        match &self.receivers {
            Receivers::Any => line("NODE_ALLOW_ANY_RECEIVER", "1"),
            Receivers::Only(list) => line(
                "NODE_ALLOWED_RECEIVERS",
                &list
                    .iter()
                    .map(TronAddress::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
        line(
            "NODE_MAX_BALANCE_SUN",
            &(self.max_trx * SUN_PER_TRX).to_string(),
        );
        line(
            "NODE_MAX_DAILY_BALANCE_SUN",
            &(self.max_daily_trx * SUN_PER_TRX).to_string(),
        );
        line(
            "NODE_MAX_LOCKED_BALANCE_SUN",
            &(self.max_locked_trx * SUN_PER_TRX).to_string(),
        );
        line(
            "NODE_MAX_LOCK_SECONDS",
            &(self.max_lock_days * 24 * 3600).to_string(),
        );
        line("NODE_LOCK_DELEGATIONS", "true");
        line("RUST_LOG", "info");
        for (k, v) in extra {
            line(k, v);
        }
        s
    }
}

/// What was done, for the final summary.
pub struct Installed {
    pub config: PathBuf,
    pub state: PathBuf,
    pub logs: String,
    /// Start / stop / status commands, most useful first.
    pub manage: Vec<String>,
}

/// `/Users/me/.gtron-node` → `~/.gtron-node`, for display.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Write `contents` to `path` with `mode`, owned by `owner`, without a moment where the
/// file exists with wider permissions or half its contents.
fn write_private(
    path: &Path,
    contents: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| anyhow::anyhow!("{}: {e}", tmp.display()))?;
    f.write_all(contents)?;
    f.sync_all()?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid))
            .map_err(|e| anyhow::anyhow!("chown {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(())
}

fn private_dir(path: &Path, mode: u32, owner: Option<(u32, u32)>) -> anyhow::Result<()> {
    fs::create_dir_all(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    Ok(())
}

/// Run a command, failing with its stderr.
fn run(cmd: &str, args: &[&str], dir: Option<&Path>) -> anyhow::Result<String> {
    let mut c = Command::new(cmd);
    c.args(args);
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let out = c.output().map_err(|e| anyhow::anyhow!("{cmd}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{cmd} {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn id_of(user: &str) -> anyhow::Result<(u32, u32)> {
    let uid = run("id", &["-u", user], None)?.parse()?;
    let gid = run("id", &["-g", user], None)?.parse()?;
    Ok((uid, gid))
}

fn current_ids() -> anyhow::Result<(u32, u32)> {
    let uid = run("id", &["-u"], None)?.parse()?;
    let gid = run("id", &["-g"], None)?.parse()?;
    Ok((uid, gid))
}

fn wallet_file(hex_key: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(format!("{hex_key}\n").into_bytes())
}

/// The binary setup is running from, as an absolute path.
fn this_binary() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

/// Each step reports itself through `step` before it runs.
pub fn apply(
    target: Target,
    host: &Host,
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<Installed> {
    match target {
        Target::Systemd => systemd(settings, hex_key, step),
        Target::Docker => docker(host, settings, hex_key, step),
        Target::Launchd => launchd(host, settings, hex_key, step),
        Target::Manual => manual(host, settings, hex_key, step),
    }
}

fn systemd(
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<Installed> {
    step("Creating the gtron-node system user");
    if id_of(SERVICE_USER).is_err() {
        run(
            "useradd",
            &[
                "--system",
                "--home-dir",
                "/var/lib/gtron-node",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                SERVICE_USER,
            ],
            None,
        )?;
    }
    let (uid, gid) = id_of(SERVICE_USER)?;

    step("Writing settings and key");
    let etc = Path::new(SYSTEM_ETC);
    // root:gtron-node 0750: the agent can reach its key, nobody else can list the directory.
    private_dir(etc, 0o750, Some((0, gid)))?;
    // Read by systemd as root; the agent's user never needs it.
    write_private(
        &etc.join("node.env"),
        settings.env_file(&[]).as_bytes(),
        0o600,
        Some((0, 0)),
    )?;
    write_private(
        &etc.join("wallet.key"),
        &wallet_file(hex_key),
        0o400,
        Some((uid, gid)),
    )?;

    let exe = this_binary()?;
    if exe != Path::new(SYSTEM_BIN) {
        step("Installing the binary");
        let tmp = Path::new(SYSTEM_BIN).with_extension("new");
        fs::copy(&exe, &tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, SYSTEM_BIN)?;
    }

    step("Installing the service");
    write_private(
        Path::new("/etc/systemd/system/gtron-node.service"),
        SYSTEMD_UNIT.as_bytes(),
        0o644,
        Some((0, 0)),
    )?;
    run("systemctl", &["daemon-reload"], None)?;
    run("systemctl", &["enable", "gtron-node"], None)?;
    step("Starting gtron-node");
    // `restart` also covers a reinstall over a running agent; the job in hand finishes first.
    run("systemctl", &["restart", "gtron-node"], None)?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let state = run("systemctl", &["is-active", "gtron-node"], None).unwrap_or_default();
    if state != "active" {
        anyhow::bail!("the service is {state:?}; see `journalctl -u gtron-node -n 50`");
    }
    Ok(Installed {
        config: etc.join("node.env"),
        state: PathBuf::from("/var/lib/gtron-node"),
        logs: "journalctl -u gtron-node -f".into(),
        manage: vec![
            "systemctl status gtron-node".into(),
            "systemctl restart gtron-node".into(),
            "systemctl stop gtron-node".into(),
        ],
    })
}

pub fn compose_file(user: (u32, u32)) -> String {
    format!(
        r#"# Written by gtron-node setup. Start: docker compose up -d   Logs: docker compose logs -f
services:
  gtron-node:
    image: {DOCKER_IMAGE}
    user: "{uid}:{gid}"
    env_file: node.env
    environment:
      NODE_STATE_DIR: /var/lib/gtron-node
      NODE_PRIVATE_KEY_FILE: /run/secrets/wallet_key
    secrets:
      - wallet_key
    volumes:
      - ./state:/var/lib/gtron-node
    restart: unless-stopped
    # SIGTERM lets the job in hand finish.
    stop_grace_period: 120s
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]

secrets:
  wallet_key:
    file: ./wallet.key
"#,
        uid = user.0,
        gid = user.1
    )
}

fn docker(
    host: &Host,
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<Installed> {
    let dir = host.docker_dir()?;
    // The container runs as the owner of its key and state: the image's user when we can
    // hand the files to it (root), otherwise you.
    let owner = if host.root {
        (IMAGE_UID, IMAGE_UID)
    } else {
        current_ids()?
    };
    step("Writing settings and key");
    private_dir(&dir, 0o700, None)?;
    let state = dir.join("state");
    if !state.exists() {
        private_dir(&state, 0o700, Some(owner))?;
    }
    write_private(
        &dir.join("node.env"),
        settings.env_file(&[]).as_bytes(),
        0o600,
        None,
    )?;
    write_private(
        &dir.join("wallet.key"),
        &wallet_file(hex_key),
        0o400,
        Some(owner),
    )?;
    write_private(
        &dir.join("compose.yml"),
        compose_file(owner).as_bytes(),
        0o644,
        None,
    )?;
    step("Pulling the image");
    run("docker", &["compose", "pull"], Some(&dir))?;
    step("Starting the container");
    run("docker", &["compose", "up", "-d"], Some(&dir))?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let running = run(
        "docker",
        &["compose", "ps", "--status", "running", "-q"],
        Some(&dir),
    )?;
    if running.is_empty() {
        anyhow::bail!(
            "the container is not running; see `docker compose logs` in {}",
            dir.display()
        );
    }
    let cd = format!("cd {} &&", tilde(&dir, host.home.as_deref()));
    Ok(Installed {
        config: dir.join("node.env"),
        state,
        logs: format!("{cd} docker compose logs -f"),
        manage: vec![
            format!("{cd} docker compose ps"),
            format!("{cd} docker compose restart"),
            format!("{cd} docker compose down"),
        ],
    })
}

/// Settings, key and state under `~/.gtron-node`, for launchd and manual runs.
fn user_files(
    host: &Host,
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let dir = host.user_dir()?;
    step("Writing settings and key");
    private_dir(&dir, 0o700, None)?;
    let state = dir.join("state");
    if !state.exists() {
        private_dir(&state, 0o700, None)?;
    }
    let key = dir.join("wallet.key");
    write_private(&key, &wallet_file(hex_key), 0o400, None)?;
    let config = dir.join("node.env");
    write_private(
        &config,
        settings
            .env_file(&[
                ("NODE_PRIVATE_KEY_FILE", key.display().to_string()),
                ("NODE_STATE_DIR", state.display().to_string()),
            ])
            .as_bytes(),
        0o600,
        None,
    )?;
    Ok((dir, config, state))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn launchd_plist(exe: &Path, config: &Path, dir: &Path) -> String {
    let e = |p: &Path| xml_escape(&p.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>WorkingDirectory</key><string>{dir}</string>
  <key>RunAtLoad</key><true/>
  <!-- Restart after a crash, not after a clean stop. -->
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <!-- SIGTERM lets the job in hand finish. -->
  <key>ExitTimeOut</key><integer>120</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = e(exe),
        config = e(config),
        dir = e(dir),
        log = e(&dir.join("node.log")),
    )
}

fn launchd(
    host: &Host,
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<Installed> {
    let (dir, config, state) = user_files(host, settings, hex_key, step)?;
    let exe = this_binary()?;
    let agents = host
        .home
        .clone()
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?
        .join("Library/LaunchAgents");
    fs::create_dir_all(&agents)?;
    let plist = agents.join(format!("{LAUNCHD_LABEL}.plist"));
    step("Installing the service");
    write_private(
        &plist,
        launchd_plist(&exe, &config, &dir).as_bytes(),
        0o644,
        None,
    )?;
    let (uid, _) = current_ids()?;
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    // A previous install is still loaded: unload it first, or bootstrap fails.
    let _ = run("launchctl", &["bootout", &service], None);
    step("Starting gtron-node");
    run(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
        None,
    )?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let info = run("launchctl", &["print", &service], None)?;
    if !info.contains("state = running") {
        anyhow::bail!(
            "the agent is not running; see {}",
            dir.join("node.log").display()
        );
    }
    Ok(Installed {
        config,
        state,
        logs: format!(
            "tail -f {}",
            tilde(&dir.join("node.log"), host.home.as_deref())
        ),
        manage: vec![
            format!("launchctl kickstart -k {service}"),
            format!("launchctl bootout {service}"),
        ],
    })
}

fn manual(
    host: &Host,
    settings: &Settings,
    hex_key: &str,
    step: &mut dyn FnMut(&str),
) -> anyhow::Result<Installed> {
    let (_, config, state) = user_files(host, settings, hex_key, step)?;
    let exe = this_binary()?;
    let home = host.home.as_deref();
    let run_it = format!("{} --config {}", tilde(&exe, home), tilde(&config, home));
    Ok(Installed {
        config,
        state,
        logs: "in the terminal it runs in".into(),
        manage: vec![run_it],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            api_url: "https://api.gtron.network".into(),
            api_key: Zeroizing::new("pk_live_x".into()),
            trongrid_url: "https://api.trongrid.io".into(),
            trongrid_api_key: None,
            allow_plain_http_node: false,
            receivers: Receivers::Any,
            max_trx: 50_000,
            max_daily_trx: 200_000,
            max_locked_trx: 500_000,
            max_lock_days: 30,
        }
    }

    #[test]
    fn env_file_is_what_the_agent_reads() {
        let env = settings().env_file(&[("NODE_STATE_DIR", "/s".into())]);
        assert!(env.starts_with("# Written by gtron-node setup\n"));
        assert!(env.contains("NODE_API_KEY=pk_live_x\n"));
        assert!(env.contains("NODE_ALLOW_ANY_RECEIVER=1\n"));
        assert!(env.contains("NODE_MAX_BALANCE_SUN=50000000000\n"));
        assert!(env.contains("NODE_MAX_LOCK_SECONDS=2592000\n"));
        assert!(env.contains("NODE_STATE_DIR=/s\n"));
        assert!(!env.contains("TRONGRID_API_KEY"));
        // Every line parses the way the agent will load it.
        let dir = std::env::temp_dir().join(format!("gtron-node-setup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("node.env");
        fs::write(&p, env.as_bytes()).unwrap();
        let vars: Vec<(String, String)> = dotenvy::from_path_iter(&p)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(vars
            .iter()
            .any(|(k, v)| k == "NODE_MAX_DAILY_BALANCE_SUN" && v == "200000000000"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn allowlist_is_comma_separated() {
        let mut s = settings();
        s.receivers = Receivers::Only(vec![
            "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".parse().unwrap(),
            "TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7".parse().unwrap(),
        ]);
        let env = s.env_file(&[]);
        assert!(env.contains(
            "NODE_ALLOWED_RECEIVERS=TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t,TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7\n"
        ));
        assert!(!env.contains("NODE_ALLOW_ANY_RECEIVER"));
    }

    #[test]
    fn private_files_are_private_from_the_start() {
        let dir = std::env::temp_dir().join(format!("gtron-node-priv-{}", std::process::id()));
        private_dir(&dir, 0o700, None).unwrap();
        let p = dir.join("wallet.key");
        write_private(&p, b"01\n", 0o400, None).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o400
        );
        gtron_tron_signing::secretfile::check_private_file(&p).unwrap();
        // Rewriting a read-only key works: the new file replaces it.
        write_private(&p, b"02\n", 0o400, None).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"02\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rendered_service_files() {
        let compose = compose_file((1000, 1000));
        assert!(compose.contains(&format!("image: {DOCKER_IMAGE}")));
        assert!(compose.contains("user: \"1000:1000\""));
        let plist = launchd_plist(
            Path::new("/Applications/a&b/gtron-node"),
            Path::new("/Users/x/.gtron-node/node.env"),
            Path::new("/Users/x/.gtron-node"),
        );
        assert!(plist.contains("<string>/Applications/a&amp;b/gtron-node</string>"));
        assert!(plist.contains("/Users/x/.gtron-node/node.log"));
        assert!(SYSTEMD_UNIT.contains("ExecStart=/usr/local/bin/gtron-node"));
    }
}
