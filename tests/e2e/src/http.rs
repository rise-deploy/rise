//! Small blocking HTTP helpers + a generic poll loop for eventual consistency.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// A blocking client with a sane request timeout.
pub fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client")
}

/// GET `url`, optionally overriding the `Host` header (Docker/Traefik routes by host).
pub fn get(url: &str, host: Option<&str>) -> Result<HttpResponse> {
    let mut req = client().get(url);
    if let Some(h) = host {
        req = req.header(reqwest::header::HOST, h);
    }
    let resp = req.send().with_context(|| format!("GET {url}"))?;
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
