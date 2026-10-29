//! `gtron-node setup`: a guided install. Asks for the shop key and the wallet key, checks
//! both against the platform and the TRON node, links the wallet to the shop if needed,
//! then writes the settings and starts the agent as a service.

#[cfg(unix)]
mod install;
#[cfg(unix)]
mod platform;
#[cfg(unix)]
mod tty;

use std::process::ExitCode;

#[cfg(not(unix))]
pub fn main() -> anyhow::Result<ExitCode> {
    eprintln!("gtron-node setup supports Linux and macOS. On this system, configure it by hand: see README.md.");
    Ok(ExitCode::FAILURE)
}

#[cfg(unix)]
pub fn main() -> anyhow::Result<ExitCode> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let echo = tty::NoEcho::new();
    let result = wizard::run(&rt);
    drop(echo);
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) if wizard::cancelled(&e) => {
            cliclack::outro_cancel("Setup cancelled. Nothing was installed.")?;
            Ok(ExitCode::FAILURE)
        }
        Err(e) => {
            cliclack::outro_cancel(format!("Setup stopped: {e:#}"))?;
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(unix)]
mod wizard {
    use console::style;
    use gtron_tron_address::TronAddress;
    use gtron_tron_signing::keys::{address_of, sign_message};
    use gtron_tron_signing::tron::{check_node_url, check_node_url_with, Node};
    use k256::ecdsa::SigningKey;
    use tokio::runtime::Runtime;
    use zeroize::Zeroizing;

    use super::install::{self, Host, Receivers, Settings, Target};
    use super::platform::{is_verification_message, Platform, Shop};
    use super::tty::discard_typeahead;
    use crate::printable::printable;

    const DEFAULT_API: &str = "https://api.gtron.network";
    const TRONGRID: &str = "https://api.trongrid.io";
    const RECOMMENDED: (u64, u64, u64, u64) = (50_000, 200_000, 500_000, 30);

    pub fn cancelled(e: &anyhow::Error) -> bool {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted)
    }

    fn trx(n: u64) -> String {
        // 50000 -> "50 000": TRX amounts read better grouped.
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push('\u{202f}');
            }
            out.push(c);
        }
        out
    }

    fn short(a: &str) -> String {
        if a.len() > 14 {
            format!("{}…{}", &a[..6], &a[a.len() - 6..])
        } else {
            a.to_owned()
        }
    }

    pub fn run(rt: &Runtime) -> anyhow::Result<()> {
        let host = Host::detect();
        println!();
        cliclack::intro(format!(
            "{} {}",
            style(" gtron-node setup ").black().on_cyan().bold(),
            style(concat!("v", env!("CARGO_PKG_VERSION"))).dim()
        ))?;
        cliclack::log::remark(
            "Connects your TRON wallet to your gtron.network shop.\n\
             Your private key is stored on this machine only and never sent anywhere.",
        )?;
        if cfg!(target_os = "linux") && host.systemd && !host.root {
            cliclack::log::warning(format!(
                "Not running as root: the system service option needs {}.",
                style("sudo gtron-node setup").bold()
            ))?;
        }

        // ---- 1. Platform --------------------------------------------------------------
        let api_url: String = cliclack::input("Platform API")
            .default_input(DEFAULT_API)
            .validate(|s: &String| {
                check_node_url(s.trim())
                    .map_err(|_| "Use an https:// address (http only for localhost)")
            })
            .interact()?;
        let api_url = api_url.trim().trim_end_matches('/').to_owned();
        let (api_key, platform, shop) = loop {
            discard_typeahead();
            let key = Zeroizing::new(
                cliclack::password("Shop API key")
                    .mask('•')
                    .validate(|s: &String| {
                        if s.trim().starts_with("pk_") {
                            Ok(())
                        } else {
                            Err("Provider keys start with pk_live_ or pk_test_ (cabinet → Shop → API keys)")
                        }
                    })
                    .interact()?
                    .trim()
                    .to_owned(),
            );
            let platform = Platform::new(&api_url, &key)?;
            let sp = cliclack::spinner();
            sp.start("Checking the key with the platform…");
            match rt.block_on(platform.shop()) {
                Ok(shop) => {
                    sp.stop(format!(
                        "Shop {} · {} mode · {}",
                        style(&shop.name).bold(),
                        shop.mode,
                        shop.status
                    ));
                    break (key, platform, shop);
                }
                Err(e) if matches!(e.status, Some(401 | 403)) => {
                    sp.error(format!("The platform rejected this key: {e}"));
                }
                Err(e) => {
                    sp.error(format!("{e}"));
                    if !cliclack::confirm("Try again?")
                        .initial_value(true)
                        .interact()?
                    {
                        anyhow::bail!("the platform is unreachable");
                    }
                }
            }
        };
        shop_notes(&shop)?;

        // ---- 2. Wallet ----------------------------------------------------------------
        let limited = cliclack::select("Signing key")
            .item(
                true,
                "A delegation-only key",
                "recommended · if this machine is compromised, the key still cannot move TRX",
            )
            .item(
                false,
                "The wallet's own key",
                "simplest · whoever gets this machine controls the wallet",
            )
            .interact()?;
        let (hex_key, signing_key, address) = if limited {
            let wallet: String = cliclack::input("Wallet address (the one holding the staked TRX)")
                .placeholder("T…")
                .validate(|s: &String| {
                    s.trim()
                        .parse::<TronAddress>()
                        .map(|_| ())
                        .map_err(|_| "Not a TRON address")
                })
                .interact()?;
            let wallet: TronAddress = wallet.trim().parse()?;
            let (hex_key, key) = if cliclack::select("The key itself")
                .item(
                    true,
                    "Create a new key here",
                    "recommended · it never exists anywhere else",
                )
                .item(false, "Paste a key I already added to the permission", "")
                .interact()?
            {
                new_key()?
            } else {
                ask_key()?
            };
            let signer = address_of(&key);
            cliclack::note(
                "Give this key the permission",
                format!(
                    "Key address  {}\n\nOn tronscan.org/#/wallet/permissions, signed in with wallet\n{}:\n  1. Owner Permission → Edit Permission → Add Active Permission\n  2. Operations → +Add: only “Delegate Resources” and “Reclaim Resources”\n  3. Threshold 1 · Authorized To: the key address above · Weight 1 → Add\n  4. Save, and confirm in TronLink (TRON charges a fee for it)\n\nSetup checks it on chain before it installs anything.",
                    style(signer.as_str()).bold(),
                    wallet
                ),
            )?;
            (hex_key, key, wallet)
        } else {
            loop {
                let (hex_key, key) = ask_key()?;
                let address = address_of(&key);
                if cliclack::confirm(format!(
                    "Is {} the wallet your shop delegates from?",
                    style(address.as_str()).bold()
                ))
                .initial_value(true)
                .interact()?
                {
                    break (hex_key, key, address);
                }
            }
        };
        link_wallet(rt, &platform, (!limited).then_some(&signing_key), &address)?;

        // ---- 3. TRON node -------------------------------------------------------------
        let (trongrid_url, trongrid_api_key, allow_plain_http_node, permission_id) = loop {
            let own = cliclack::select("TRON node")
                .item(false, "TronGrid", "recommended · api.trongrid.io")
                .item(true, "My own node", "full node HTTP API")
                .interact()?;
            let (url, plain) = if own {
                let url: String = cliclack::input("Node URL")
                    .placeholder("https://tron.example.com")
                    .validate(|s: &String| {
                        check_node_url_with(s.trim(), true).map_err(|_| "Not a valid URL")
                    })
                    .interact()?;
                let url = url.trim().trim_end_matches('/').to_owned();
                let plain = check_node_url(&url).is_err();
                if plain
                    && !cliclack::confirm(
                        "This node is plain http. Only use that on a private network. Continue?",
                    )
                    .initial_value(false)
                    .interact()?
                {
                    continue;
                }
                (url, plain)
            } else {
                (TRONGRID.to_owned(), false)
            };
            discard_typeahead();
            let key = Zeroizing::new(
                cliclack::password(if own {
                    "API key for the node (Enter to skip)"
                } else {
                    "TronGrid API key (Enter to skip)"
                })
                .mask('•')
                .allow_empty()
                .interact()?
                .trim()
                .to_owned(),
            );
            let key = (!key.is_empty()).then_some(key);
            if key.is_none() && !own {
                cliclack::log::warning(
                    "Without an API key TronGrid throttles hard. A free key: trongrid.io → Dashboard.",
                )?;
            }
            let node = Node::new(
                &url,
                key.as_ref().map(|k| k.to_string()),
                std::time::Duration::from_secs(15),
            );
            let sp = cliclack::spinner();
            sp.start("Checking the TRON node…");
            match rt.block_on(node.balances(&address)) {
                Ok((sun, _)) => {
                    sp.stop(format!(
                        "TRON node reachable · wallet holds {} TRX",
                        trx((sun / 1_000_000).max(0) as u64)
                    ));
                    let permission_id = if limited {
                        Some(find_permission(
                            rt,
                            &node,
                            &address,
                            &address_of(&signing_key),
                        )?)
                    } else {
                        None
                    };
                    break (url, key, plain, permission_id);
                }
                Err(e) => {
                    sp.error(format!("{e}"));
                    if !cliclack::confirm("Try another node or key?")
                        .initial_value(true)
                        .interact()?
                    {
                        anyhow::bail!("the TRON node is unreachable");
                    }
                }
            }
        };

        // ---- 4. Policy ----------------------------------------------------------------
        let receivers = if cliclack::select("Who may receive delegations?")
            .item(
                true,
                "Any client",
                "recommended · the platform names the buyer's address",
            )
            .item(
                false,
                "Only addresses I list",
                "for a shop that serves known clients",
            )
            .interact()?
        {
            Receivers::Any
        } else {
            let list: String = cliclack::input("Allowed addresses")
                .placeholder("TXXX…, TYYY…")
                .validate(|s: &String| {
                    let parsed: Vec<_> = s
                        .split([',', ' ', '\n'])
                        .filter(|a| !a.is_empty())
                        .map(str::parse::<TronAddress>)
                        .collect();
                    if parsed.is_empty() {
                        Err("At least one address".to_owned())
                    } else if let Some(Err(e)) = parsed.iter().find(|p| p.is_err()) {
                        Err(format!("{e}"))
                    } else {
                        Ok(())
                    }
                })
                .interact()?;
            Receivers::Only(
                list.split([',', ' ', '\n'])
                    .filter(|a| !a.is_empty())
                    .map(str::parse)
                    .collect::<Result<_, _>>()?,
            )
        };
        let (r_one, r_day, r_locked, r_days) = RECOMMENDED;
        let custom = cliclack::select("Safety limits")
            .item(
                false,
                "Recommended",
                format!(
                    "≤ {} TRX per delegation · ≤ {} per 24 h · ≤ {} locked",
                    trx(r_one),
                    trx(r_day),
                    trx(r_locked)
                ),
            )
            .item(true, "Custom", "set each limit yourself")
            .interact()?;
        let (max_trx, max_daily_trx, max_locked_trx, max_lock_days) = if custom {
            (
                amount("Largest single delegation, TRX", r_one)?,
                amount("Most delegated in 24 hours, TRX", r_day)?,
                amount("Most locked at once, TRX", r_locked)?,
                amount("Longest rental, days", r_days)?,
            )
        } else {
            RECOMMENDED
        };

        // ---- 5. How it runs -----------------------------------------------------------
        let mut pick = cliclack::select("How should gtron-node run?");
        let mut first = None;
        if host.systemd && host.root {
            pick = pick.item(
                Target::Systemd,
                "System service",
                "systemd · starts on boot · recommended",
            );
            first.get_or_insert(Target::Systemd);
        }
        if host.docker {
            pick = pick.item(
                Target::Docker,
                "Docker container",
                format!("docker compose · {}", install::DOCKER_IMAGE),
            );
            first.get_or_insert(Target::Docker);
        }
        if host.launchd_ok() {
            pick = pick.item(
                Target::Launchd,
                "Background service",
                "launchd · starts when you log in",
            );
            first.get_or_insert(Target::Launchd);
        }
        pick = pick.item(
            Target::Manual,
            "I'll start it myself",
            "writes the settings, prints the command",
        );
        let target = pick
            .initial_value(first.unwrap_or(Target::Manual))
            .interact()?;

        let settings = Settings {
            api_url,
            api_key,
            trongrid_url,
            trongrid_api_key,
            allow_plain_http_node,
            receivers,
            max_trx,
            max_daily_trx,
            max_locked_trx,
            max_lock_days,
            permission: permission_id.map(|id| (address.clone(), id)),
        };
        cliclack::note(
            "Summary",
            format!(
                "Shop      {}\nWallet    {}\nKey       {}\nTRON      {}\nReceivers {}\nLimits    {} / {} per 24 h / {} locked TRX, rentals ≤ {} days",
                shop.name,
                address,
                match permission_id {
                    Some(id) => format!("delegation-only, permission {id}"),
                    None => "the wallet's own key".to_owned(),
                },
                settings.trongrid_url,
                match &settings.receivers {
                    Receivers::Any => "any client".to_owned(),
                    Receivers::Only(l) => format!("{} listed", l.len()),
                },
                trx(max_trx),
                trx(max_daily_trx),
                trx(max_locked_trx),
                max_lock_days
            ),
        )?;
        if !cliclack::confirm("Install and start?")
            .initial_value(true)
            .interact()?
        {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
        }

        // ---- 6. Install ---------------------------------------------------------------
        let sp = cliclack::spinner();
        sp.start("Installing…");
        let result = install::apply(target, &host, &settings, &hex_key, &mut |s| {
            sp.set_message(s);
        });
        let done = match result {
            Ok(d) => {
                sp.stop("Installed");
                d
            }
            Err(e) => {
                sp.error(format!("{e:#}"));
                return Err(e);
            }
        };
        let home = host.home.as_deref();
        let mut lines = vec![
            format!("Settings  {}", install::tilde(&done.config, home)),
            format!("State     {}", install::tilde(&done.state, home)),
            format!(
                "          {}",
                style("keep it across upgrades: it holds what was signed").dim()
            ),
            format!("Logs      {}", done.logs),
        ];
        for (i, m) in done.manage.iter().enumerate() {
            let label = if i == 0 { "Commands" } else { "" };
            lines.push(format!("{label:<10}{m}"));
        }
        cliclack::note("Where things are", lines.join("\n"))?;
        cliclack::outro(match target {
            Target::Manual => format!(
                "Ready. Start it with:\n   {}",
                style(&done.manage[0]).cyan()
            ),
            _ => format!(
                "{} gtron-node is running and taking jobs for {}.",
                style("●").green(),
                style(&shop.name).bold()
            ),
        })?;
        Ok(())
    }

    fn shop_notes(shop: &Shop) -> anyhow::Result<()> {
        if shop.mode != "agent" {
            anyhow::bail!(
                "shop \"{}\" is in {} mode: the platform delegates for it and sends no jobs to an agent. \
                 Switch it to agent mode in the cabinet (Shop → Settings), then run setup again",
                shop.name,
                shop.mode
            );
        }
        if let Some(issue) = &shop.eligibility_issue {
            cliclack::log::warning(format!("The shop does not sell yet: {issue}"))?;
        } else if shop.status != "active" {
            cliclack::log::warning(format!(
                "The shop is {}: it gets jobs once it is active.",
                shop.status
            ))?;
        }
        Ok(())
    }

    /// Make sure the platform knows this wallet belongs to the shop; offer to prove it.
    fn ask_key() -> anyhow::Result<(Zeroizing<String>, SigningKey)> {
        loop {
            discard_typeahead();
            let raw = Zeroizing::new(
                cliclack::password("Private key")
                    .mask('•')
                    .validate(|s: &String| {
                        let h = s.trim().trim_start_matches("0x");
                        if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                            Ok(())
                        } else {
                            Err("64 hex characters, as TronLink exports it (Settings → Export private key)")
                        }
                    })
                    .interact()?,
            );
            let hex_key = Zeroizing::new(raw.trim().trim_start_matches("0x").to_lowercase());
            let bytes = Zeroizing::new(hex::decode(hex_key.as_str())?);
            match SigningKey::from_slice(&bytes) {
                Ok(key) => return Ok((hex_key, key)),
                Err(_) => cliclack::log::error("That is not a valid secp256k1 private key.")?,
            }
        }
    }

    /// A fresh key from the OS random source.
    fn new_key() -> anyhow::Result<(Zeroizing<String>, SigningKey)> {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")?;
        loop {
            let mut bytes = Zeroizing::new([0u8; 32]);
            f.read_exact(bytes.as_mut())?;
            if let Ok(key) = SigningKey::from_slice(bytes.as_ref()) {
                return Ok((Zeroizing::new(hex::encode(bytes.as_ref())), key));
            }
        }
    }

    /// Wait until one of the wallet's active permissions lets `signer` delegate and nothing
    /// more; its id.
    fn find_permission(
        rt: &Runtime,
        node: &Node,
        wallet: &TronAddress,
        signer: &TronAddress,
    ) -> anyhow::Result<i32> {
        loop {
            let sp = cliclack::spinner();
            sp.start("Looking for the key's permission on chain…");
            let problem = match rt.block_on(node.active_permissions(wallet)) {
                Ok(list) => {
                    let mine: Vec<_> = list
                        .iter()
                        .filter(|(_, p)| p.keys.iter().any(|(a, _)| a == signer.as_str()))
                        .collect();
                    match mine
                        .iter()
                        .find(|(_, p)| p.check_delegation_only(signer).is_ok())
                    {
                        Some((id, _)) => {
                            sp.stop(format!(
                                "Permission {id}: this key may only delegate and reclaim resources"
                            ));
                            return Ok(*id);
                        }
                        None => match mine.first() {
                            Some((id, p)) => format!(
                                "permission {id}: {}",
                                p.check_delegation_only(signer).unwrap_err()
                            ),
                            None => {
                                format!("no active permission of {wallet} has key {signer} yet")
                            }
                        },
                    }
                }
                Err(e) => format!("the TRON node did not answer: {e}"),
            };
            sp.error(problem);
            if !cliclack::confirm("Check again? (add or fix the permission first)")
                .initial_value(true)
                .interact()?
            {
                anyhow::bail!("the delegation-only permission is not set up");
            }
        }
    }

    fn link_wallet(
        rt: &Runtime,
        platform: &Platform,
        key: Option<&SigningKey>,
        address: &TronAddress,
    ) -> anyhow::Result<()> {
        let sp = cliclack::spinner();
        sp.start("Looking up the shop's wallets…");
        let wallets = match rt.block_on(platform.wallets()) {
            Ok(w) => w,
            Err(e) => {
                sp.error(format!("Could not list the shop's wallets: {e}"));
                return Ok(());
            }
        };
        let existing = wallets.into_iter().find(|w| w.address == address.as_str());
        if existing.as_ref().is_some_and(|w| w.verified) {
            sp.stop(format!(
                "Wallet {} is verified for this shop",
                short(address.as_str())
            ));
            return Ok(());
        }
        sp.stop("This wallet is not verified for the shop yet");
        let Some(key) = key else {
            // Only the wallet's own key proves ownership; a permission key cannot.
            if existing.is_none() {
                if let Err(e) = rt.block_on(platform.add_wallet(address.as_str())) {
                    return scope_hint(&e.to_string(), e.status);
                }
            }
            cliclack::log::warning(
                "Verify the wallet in the cabinet (Shop → Wallets) by signing with TronLink. No jobs reach it until then.",
            )?;
            return Ok(());
        };
        if !cliclack::confirm(
            "Verify it now? gtron-node signs a verification message with it. No funds move.",
        )
        .initial_value(true)
        .interact()?
        {
            cliclack::log::warning(
                "No jobs reach this wallet until it is verified (cabinet → Shop → Wallets).",
            )?;
            return Ok(());
        }
        let wallet = match existing {
            Some(w) => w,
            None => match rt.block_on(platform.add_wallet(address.as_str())) {
                Ok(w) => w,
                Err(e) => return scope_hint(&e.to_string(), e.status),
            },
        };
        let Some(message) = wallet.message_to_sign.clone() else {
            cliclack::log::warning("The platform sent nothing to sign; verify it in the cabinet.")?;
            return Ok(());
        };
        // Sign only what verification looks like, for this very address: never whatever
        // text the server (or someone in between) sends.
        if !is_verification_message(&message, address.as_str()) {
            anyhow::bail!(
                "the platform asked to sign an unexpected message; not signing it:\n{}",
                printable(&message)
            );
        }
        cliclack::note("Signing", &message)?;
        let sp = cliclack::spinner();
        sp.start("Verifying the wallet…");
        match rt.block_on(platform.verify_wallet(&wallet.id, &sign_message(key, &message))) {
            Ok(w) if w.verified => sp.stop("Wallet verified for this shop"),
            Ok(_) => sp.error("The platform did not accept the signature"),
            Err(e) => {
                sp.error(format!("{e}"));
                return scope_hint(&e.to_string(), e.status);
            }
        }
        Ok(())
    }

    fn scope_hint(err: &str, status: Option<u16>) -> anyhow::Result<()> {
        if status == Some(403) {
            cliclack::log::warning(
                "This API key may not manage wallets (it needs the `shop` scope). \
                 Verify the wallet in the cabinet (Shop → Wallets) instead.",
            )?;
        } else {
            cliclack::log::warning(format!(
                "Could not verify the wallet ({err}). Verify it in the cabinet (Shop → Wallets)."
            ))?;
        }
        Ok(())
    }

    fn amount(prompt: &str, default: u64) -> anyhow::Result<u64> {
        let v: String = cliclack::input(prompt)
            .default_input(&default.to_string())
            .validate(
                |s: &String| match s.trim().replace([' ', '_', ','], "").parse::<u64>() {
                    Ok(n) if n > 0 && n <= 1_000_000_000 => Ok(()),
                    _ => Err("A whole number above 0"),
                },
            )
            .interact()?;
        Ok(v.trim().replace([' ', '_', ','], "").parse()?)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn formatting() {
            assert_eq!(trx(50_000), "50\u{202f}000");
            assert_eq!(trx(500), "500");
            assert_eq!(trx(1_234_567), "1\u{202f}234\u{202f}567");
            assert_eq!(short("TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC"), "TMVQGm…uHK2HC");
        }
    }
}
