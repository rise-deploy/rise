//! Small blocking HTTP helpers + a generic poll loop for eventual consistency.

use anyhow::{Context, Result};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Process-wide blocking client, lazily initialized on first use and reused across
/// every `get` call. Building a `reqwest::blocking::Client` spins up a tokio runtime
/// and a connection pool, so the poll loops (which call `get` dozens of times) share
/// a single instance rather than churning one per request.
fn shared_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            // Fail fast on a stalled connection so the poll loop can retry within its
            // budget (some budgets are only 60s); the overall request timeout stays
            // proportionate to the poll cadence.
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest client")
    })
}

/// The shared blocking client. Callers other than `get` (e.g. token minting) use this
/// for ad-hoc requests; it points at the same reused, lazily-initialized instance.
pub fn client() -> &'static reqwest::blocking::Client {
    shared_client()
}

/// GET `url`, optionally overriding the `Host` header (Docker/Traefik routes by host).
pub fn get(url: &str, host: Option<&str>) -> Result<HttpResponse> {
    let mut req = shared_client().get(url);
    if let Some(h) = host {
        req = req.header(reqwest::header::HOST, h);
    }
    let resp = req.send().with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    Ok(HttpResponse { status, body })
}

/// GET `url` with a `Bearer` token — for authenticated Rise API calls.
pub fn get_auth(url: &str, bearer: &str) -> Result<HttpResponse> {
    let resp = shared_client()
        .get(url)
        .bearer_auth(bearer)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    Ok(HttpResponse { status, body })
}

/// A client that does NOT follow redirects — for asserting 3xx + `Location`.
fn no_redirect_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build no-redirect client")
    })
}

/// GET without following redirects, returning the status and `Location` header.
pub fn get_no_redirect(url: &str, host: Option<&str>) -> Result<(u16, Option<String>)> {
    let mut req = no_redirect_client().get(url);
    if let Some(h) = host {
        req = req.header(reqwest::header::HOST, h);
    }
    let resp = req.send().with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Ok((status, location))
}

/// GET with a `Host` override and a `Cookie` header (Traefik forwardAuth auth check).
pub fn get_with_cookie(url: &str, host: &str, cookie: &str) -> Result<HttpResponse> {
    let resp = shared_client()
        .get(url)
        .header(reqwest::header::HOST, host)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    Ok(HttpResponse { status, body })
}

/// GET `url` with a single custom header (e.g. Vault's `X-Vault-Token`).
pub fn get_auth_header(url: &str, header: &str, value: &str) -> Result<HttpResponse> {
    let resp = shared_client()
        .get(url)
        .header(header, value)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    Ok(HttpResponse { status, body })
}

/// Poll `f` until it returns `Ok(true)`, every `interval`, up to `timeout`.
/// `f` returning `Ok(false)` or `Err` keeps polling (transient failures are
/// expected while the system converges). Times out with the last error/message.
pub fn poll<F>(timeout: Duration, interval: Duration, what: &str, mut f: F) -> Result<()>
where
    F: FnMut() -> Result<bool>,
{
    let start = Instant::now();
    loop {
        let outcome = f();
        if matches!(outcome, Ok(true)) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            let detail = match outcome {
                Ok(true) => unreachable!(),
                Ok(false) => "condition not met".to_string(),
                Err(e) => format!("{e:#}"),
            };
            anyhow::bail!("timed out after {timeout:?} waiting for {what} — last: {detail}");
        }
        std::thread::sleep(interval);
    }
}
