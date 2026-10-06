//! `test-nansen` — x402 smoke driver.
//!
//! `--check`: FREE live leg — unpaid POST to the profiler endpoint prints the
//! rail table (no key, no payment). Default: ONE paid call via the funded
//! x402 wallet (`--address <0x…>`, default = payer), printing cost, tx hash
//! and a Monad explorer link (PENDING-WALLET until the key is real).
//!
//! **P09 status:** implemented (`orchestrator` agent); interfaces frozen.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde_json::Value;

use sentinel::config::Config;
use sentinel::nansen::{ENDPOINT_PERP_POSITIONS, NansenClient, client, x402};

/// Free-leg target ($0.01 rail; the cheapest challenge to parse).
const CHECK_ENDPOINT: &str = ENDPOINT_PERP_POSITIONS;

/// Default Nansen base URL when `NANSEN_BASE_URL` is unset (free leg).
const DEFAULT_BASE_URL: &str = "https://api.nansen.ai";

/// Monad rail network (CAIP-2) the paid flow targets.
const MONAD_NETWORK: &str = "eip155:143";

/// Placeholder payer address sent on the free leg (STUB-04 promo hook).
const ZERO_PAYER: &str = "0x0000000000000000000000000000000000000000";

/// Header carrying the payer address on the initial unpaid request (STUB-04).
const HEADER_PAYER_ADDRESS: &str = "X-Payer-Address";

/// Characters of the paid response printed by the smoke.
const HEAD_LIMIT: usize = 1024;

/// Per-call HTTP timeout (matches the client).
const HTTP_TIMEOUT_SECS: u64 = 30;

const USAGE: &str = "\
usage: test-nansen [--check | --address <0x…>]

  --check          free live leg: unpaid POST to the profiler endpoint, print
                   the x402 rail table (no key, no payment required)
  --address <0x…>  address whose perp positions are fetched (default: the
                   configured x402 payer address)

No arguments: ONE paid perp-positions call through the x402 flow, printing the
response head, cost_usd, tx_hash and a Monad explorer link. PENDING-WALLET
until NANSEN_PAYER_KEY is a real funded key (STUB-16).";

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq)]
enum Invocation {
    /// `--check`: the free unpaid challenge leg.
    Check,
    /// The paid smoke; `address` is the optional `--address` override.
    Paid {
        /// Optional `0x…` address to query positions for.
        address: Option<String>,
    },
    /// `--help` / `-h`.
    Help,
    /// Anything unrecognized.
    Usage,
}

/// Hand-rolled argument parsing (frozen surface: `--check` | `--address <0x…>`
/// | default).
fn parse_args(args: &[String]) -> Invocation {
    match args {
        [] => Invocation::Paid { address: None },
        [flag] if flag == "--check" => Invocation::Check,
        [flag] if flag == "--help" || flag == "-h" => Invocation::Help,
        [flag, value] if flag == "--address" => Invocation::Paid {
            address: Some(value.clone()),
        },
        _ => Invocation::Usage,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Invocation::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Invocation::Usage => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        Invocation::Check => {
            let _ = dotenvy::dotenv();
            let base_url =
                std::env::var("NANSEN_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
            run_check(&base_url).await
        }
        Invocation::Paid { address } => run_paid(address).await,
    }
}

/// Free live leg: one unpaid POST, then print the rail table (Monad
/// highlighted). No key, no payment, exit 0 on a parsed 402.
async fn run_check(base_url: &str) -> anyhow::Result<()> {
    let url = format!("{}{CHECK_ENDPOINT}", base_url.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("building the HTTP client")?;
    let body = client::perp_positions_body(&[]);
    let response = http
        .post(&url)
        .header(HEADER_PAYER_ADDRESS, ZERO_PAYER)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("unpaid POST {url}"))?;

    let status = response.status();
    let challenge_header = response
        .headers()
        .get(x402::HEADER_PAYMENT_REQUIRED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("reading the {status} body"))?;

    if status.as_u16() != 402 {
        anyhow::bail!("{url} answered HTTP {status} (expected the 402 x402 challenge)");
    }
    let challenge =
        x402::parse_payment_required(status.as_u16(), &bytes, challenge_header.as_deref())
            .with_context(|| format!("parsing the x402 challenge from {url}"))?;

    print_rail_table(&challenge);
    Ok(())
}

/// Paid smoke: ONE perp-positions call through the full x402 flow.
async fn run_paid(address: Option<String>) -> anyhow::Result<()> {
    let cfg = Config::load().context("loading Sentinel configuration (.env + environment)")?;
    if is_placeholder_key(cfg.nansen.payer_key.expose()) {
        anyhow::bail!(
            "PENDING-WALLET: NANSEN_PAYER_KEY is still the all-zero placeholder, so the paid \
             x402 flow cannot run (STUB-16). Fund the payer wallet and set a real 0x key, or run \
             the free leg: test-nansen --check"
        );
    }
    let client = NansenClient::new(&cfg.nansen)
        .context("building the Nansen x402 client (check NANSEN_PAYER_KEY)")?;
    let payer = client.address();
    let target = match address {
        Some(address) => {
            validate_address(&address)?;
            address
        }
        None => payer.clone(),
    };

    let (response, meta) = client
        .perp_positions(std::slice::from_ref(&target), now_ms())
        .await
        .context("paid perp-positions call (402 → sign → retry)")?;

    println!("address queried: {target}");
    print_response_head(&response, HEAD_LIMIT);
    println!("cost_usd: {}", meta.cost_usd);
    match &meta.tx_hash {
        Some(tx) => println!("tx_hash: {tx}\n  https://monadscan.com/tx/{tx}"),
        None if meta.cached => {
            println!("tx_hash: none — served from cache (no payment on this call)");
        }
        None => println!("tx_hash: none — no settlement header on the paid response"),
    }
    Ok(())
}

/// Print the rail table; the Monad rail (our target) is highlighted, and the
/// frozen selector's pick is echoed below.
fn print_rail_table(challenge: &x402::PaymentRequired) {
    println!(
        "x402 challenge: {} (x402Version {}) — {} rail(s)",
        challenge.resource.url,
        challenge.x402_version,
        challenge.accepts.len()
    );
    if let Some(error) = &challenge.error {
        println!("server error field: {error}");
    }
    println!(
        "{:<12} | {:<42} | {:>10} | {:<42} | domain name/version",
        "network", "asset", "amount", "payTo"
    );
    println!("{}", "-".repeat(126));
    for rail in &challenge.accepts {
        let name = rail
            .extra
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let version = rail
            .extra
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let monad = rail.network == MONAD_NETWORK;
        println!(
            "{:<12} | {:<42} | {:>10} | {:<42} | {}/{}{}",
            rail.network,
            rail.asset,
            rail.amount,
            rail.pay_to,
            name,
            version,
            if monad { "   <== MONAD (target)" } else { "" }
        );
    }
    match x402::select_rail(challenge, MONAD_NETWORK) {
        Ok(rail) => {
            let name = rail
                .extra
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let version = rail
                .extra
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("?");
            println!(
                "selected Monad rail ({MONAD_NETWORK}): asset={} amount={} payTo={} \
                 maxTimeoutSeconds={} domain={name}/{version}",
                rail.asset, rail.amount, rail.pay_to, rail.max_timeout_seconds
            );
        }
        Err(err) => {
            println!("WARNING: no usable Monad rail ({MONAD_NETWORK}) in this challenge: {err}")
        }
    }
}

/// Print at most `limit` characters of the pretty-printed response.
fn print_response_head(response: &Value, limit: usize) {
    let text = serde_json::to_string_pretty(response).unwrap_or_else(|_| response.to_string());
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    println!("{head}");
    if chars.next().is_some() {
        println!("… (truncated to the first {limit} characters)");
    }
}

/// True when `key` is the all-zero placeholder (no real wallet yet).
fn is_placeholder_key(key: &str) -> bool {
    let key = key.trim();
    let hex_part = key.strip_prefix("0x").unwrap_or(key);
    hex_part.chars().all(|c| c == '0')
}

/// Validate a `0x…` 20-byte address argument.
fn validate_address(address: &str) -> anyhow::Result<()> {
    let ok = address.len() == 42
        && address.starts_with("0x")
        && address[2..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        anyhow::bail!(
            "--address expects a 0x-prefixed 20-byte address (got {} characters)",
            address.len()
        )
    }
}

/// Wall-clock epoch milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Recorded profiler 402 challenge (same fixture the free leg parses).
    const PROFILER_402_FIXTURE: &str =
        include_str!("../../../../docs/evidence/p01-nansen-402-profiler-perp-positions.json");

    #[test]
    fn parse_args_default_is_paid_without_address() {
        assert_eq!(parse_args(&[]), Invocation::Paid { address: None });
    }

    #[test]
    fn parse_args_check_and_help() {
        assert_eq!(parse_args(&args(&["--check"])), Invocation::Check);
        assert_eq!(parse_args(&args(&["--help"])), Invocation::Help);
        assert_eq!(parse_args(&args(&["-h"])), Invocation::Help);
    }

    #[test]
    fn parse_args_address_flag() {
        let address = format!("0x{}", "11".repeat(20));
        assert_eq!(
            parse_args(&args(&["--address", &address])),
            Invocation::Paid {
                address: Some(address)
            }
        );
    }

    #[test]
    fn parse_args_rejects_unknown_shapes() {
        assert_eq!(parse_args(&args(&["--address"])), Invocation::Usage);
        assert_eq!(
            parse_args(&args(&["--address", "0x1", "extra"])),
            Invocation::Usage
        );
        assert_eq!(parse_args(&args(&["--nope"])), Invocation::Usage);
        assert_eq!(
            parse_args(&args(&["--check", "--address", "0x1"])),
            Invocation::Usage
        );
    }

    #[test]
    fn placeholder_key_detection() {
        assert!(is_placeholder_key(
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        ));
        assert!(is_placeholder_key("0x00"));
        assert!(!is_placeholder_key(&format!("0x{}", "07".repeat(32))));
        assert!(!is_placeholder_key(&format!("0x{}1", "00".repeat(31))));
    }

    #[test]
    fn address_validation() {
        assert_eq!(ZERO_PAYER.len(), 42, "zero payer must be a 20-byte address");
        assert!(validate_address(ZERO_PAYER).is_ok());
        assert!(validate_address(&format!("0x{}", "11".repeat(20))).is_ok());

        assert!(validate_address("0x123").is_err());
        assert!(validate_address(&"1".repeat(42)).is_err());
        assert!(validate_address(&format!("0x{}", "z".repeat(40))).is_err());
    }

    #[tokio::test]
    async fn check_leg_parses_the_challenge_offline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHECK_ENDPOINT))
            .and(header(HEADER_PAYER_ADDRESS, ZERO_PAYER))
            .respond_with(
                ResponseTemplate::new(402)
                    .set_body_json(serde_json::from_str::<Value>(PROFILER_402_FIXTURE).unwrap())
                    .append_header(
                        x402::HEADER_PAYMENT_REQUIRED,
                        base64_standard(PROFILER_402_FIXTURE),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        run_check(&server.uri())
            .await
            .expect("free leg must parse the challenge");

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        let body: Value = requests[0].body_json().expect("json body");
        assert_eq!(body, json!({ "addresses": [] }));
    }

    #[tokio::test]
    async fn check_leg_rejects_a_non_challenge_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;

        assert!(run_check(&server.uri()).await.is_err());
    }

    fn base64_standard(text: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    }
}
