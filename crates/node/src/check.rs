//! `gtron-node --check`: is this agent ready to take jobs? Prints one line per check.

use serde::Deserialize;

use crate::agent::Agent;

#[derive(Deserialize)]
struct Shop {
    name: String,
    mode: String,
    status: String,
    eligibility_issue: Option<String>,
}

#[derive(Default)]
struct Report {
    failed: bool,
}

impl Report {
    fn ok(&mut self, what: &str, detail: impl std::fmt::Display) {
        println!("ok    {what:<10} {detail}");
    }

    fn warn(&mut self, what: &str, detail: impl std::fmt::Display) {
        println!("warn  {what:<10} {detail}");
    }

    fn fail(&mut self, what: &str, detail: impl std::fmt::Display) {
        println!("FAIL  {what:<10} {detail}");
        self.failed = true;
    }
}

/// Read-only: nothing is taken, signed or broadcast. True when every check passed.
pub async fn run(agent: &Agent, state: Option<&str>) -> bool {
    let mut r = Report::default();
    r.ok("version", env!("CARGO_PKG_VERSION"));
    r.ok("wallet", &agent.address);
    match state {
        None => r.ok("state", "writable, locked by this check"),
        Some(note) => r.warn("state", note),
    }
    let l = &agent.limits;
    r.ok(
        "limits",
        format!(
            "≤ {} TRX per delegation, ≤ {} TRX per 24 h, ≤ {} TRX locked, receivers: {}",
            l.max_balance_sun / 1_000_000,
            l.max_daily_sun / 1_000_000,
            l.max_locked_sun / 1_000_000,
            if l.allowed_receivers.is_empty() {
                "any".to_owned()
            } else {
                format!("{} allowed", l.allowed_receivers.len())
            }
        ),
    );

    match agent
        .api::<Shop>(reqwest::Method::GET, "/p1/shop", None)
        .await
    {
        Ok(s) if s.mode != "agent" => r.fail(
            "platform",
            format!(
                "shop \"{}\" is in {} mode: the platform signs for it and sends this agent no jobs",
                s.name, s.mode
            ),
        ),
        Ok(s) => {
            let line = format!("shop \"{}\", {}", s.name, s.status);
            match (s.status.as_str(), s.eligibility_issue) {
                ("active", None) => r.ok("platform", line),
                (_, Some(issue)) => r.warn("platform", format!("{line}: {issue}")),
                _ => r.warn("platform", format!("{line}: no jobs until it is active")),
            }
        }
        Err(e) => r.fail(
            "platform",
            format!("{e} (check NODE_API_URL and NODE_API_KEY)"),
        ),
    }

    match agent.node.balances(&agent.address).await {
        Ok((0, _)) => r.warn(
            "tron",
            "node reachable, wallet has 0 TRX (not activated or empty)",
        ),
        Ok((sun, _)) => r.ok(
            "tron",
            format!("node reachable, wallet has {} TRX", sun / 1_000_000),
        ),
        Err(e) => r.fail(
            "tron",
            format!("{e} (check TRONGRID_URL and TRONGRID_API_KEY)"),
        ),
    }

    println!(
        "{}",
        if r.failed {
            "not ready"
        } else {
            "ready to take jobs"
        }
    );
    !r.failed
}
