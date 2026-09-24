//! A real Rise login, driven headlessly: the CLI's PKCE authorization-code flow
//! against the test Dex, with the harness filling in Dex's login form instead
//! of a browser.
//!
//! This is the same path `rise login` takes — Rise builds the authorize URL,
//! Dex authenticates the user, and Rise exchanges the code on the back channel
//! — so the session it yields names the user's `User` resource exactly as an
//! interactive login's does. There is deliberately no shortcut that trades a
//! presented ID token for a session.

use anyhow::{Context, Result};
use base64::Engine as _;
use reqwest::Url;

use crate::dex::DexEndpoint;

/// One of the CLI's loopback callbacks, registered on the test Dex client. The
/// harness never listens on it: it reads the code off the final redirect.
const REDIRECT_URI: &str = "http://localhost:8765/callback";

/// Upper bound on redirects and form posts before giving up.
const MAX_STEPS: usize = 12;

/// Log `username` in through Rise's PKCE code flow and return the session.
pub fn login(api_base: &str, dex: &DexEndpoint, username: &str, password: &str) -> Result<String> {
    let verifier = rise_backend_auth::generate_bootstrap_credential();
    let challenge = pkce_challenge(&verifier);

    let authorize = crate::http::post_json(
        &format!("{api_base}/api/v1/auth/authorize"),
        None,
        &serde_json::json!({
            "flow": "code",
            "redirect_uri": REDIRECT_URI,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
        }),
    )?;
    anyhow::ensure!(
        authorize.status == 200,
        "authorize returned {}:\n{}",
        authorize.status,
        authorize.body
    );
    let authorize: serde_json::Value =
        serde_json::from_str(&authorize.body).context("parse authorize response")?;
    let authorization_url = authorize["authorization_url"]
        .as_str()
        .context("authorize response has no authorization_url")?;
    // The CLI flow is bound by PKCE and carries no `state` of its own; when
    // one is present, the callback must echo it.
    let expected_state = query_param(authorization_url, "state")?;

    let code = run_dex_login(
        dex,
        authorization_url,
        expected_state.as_deref(),
        username,
        password,
    )?;

    let exchanged = crate::http::post_json(
        &format!("{api_base}/api/v1/auth/code/exchange"),
        None,
        &serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "redirect_uri": REDIRECT_URI,
        }),
    )?;
    anyhow::ensure!(
        exchanged.status == 200,
        "code exchange returned {}:\n{}",
        exchanged.status,
        exchanged.body
    );
    let exchanged: serde_json::Value =
        serde_json::from_str(&exchanged.body).context("parse code exchange response")?;
    exchanged["token"]
        .as_str()
        .map(str::to_string)
        .context("code exchange response has no token")
}

/// Walk Dex from the authorize URL to the redirect carrying the code.
fn run_dex_login(
    dex: &DexEndpoint,
    authorization_url: &str,
    expected_state: Option<&str>,
    username: &str,
    password: &str,
) -> Result<String> {
    // Redirects are followed by hand: the last one targets the CLI's loopback
    // callback, which nothing serves, and every URL Dex hands out is spelled
    // with its issuer host, which the harness may not be able to resolve.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build Dex login client")?;
    let reachable = reachable_base(dex)?;
    let issuer = Url::parse(&dex.issuer).context("parse Dex issuer")?;

    let mut url = reroute(&Url::parse(authorization_url)?, &issuer, &reachable);
    let mut response = client
        .get(url.clone())
        .send()
        .with_context(|| format!("GET {url}"))?;
    let mut submitted_login = false;
    for _ in 0..MAX_STEPS {
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("redirect without a Location")?;
            let next = url.join(location).context("resolve redirect")?;
            if next.as_str().starts_with(REDIRECT_URI) {
                return code_from_callback(next.as_str(), expected_state);
            }
            url = reroute(&next, &issuer, &reachable);
            response = client
                .get(url.clone())
                .send()
                .with_context(|| format!("GET {url}"))?;
            continue;
        }
        anyhow::ensure!(status.is_success(), "Dex answered {status} at {url}");
        let body = response.text().unwrap_or_default();
        let form =
            parse_form(&body).with_context(|| format!("Dex page at {url} has no form:\n{body}"))?;
        let target = match &form.action {
            Some(action) => reroute(&url.join(action)?, &issuer, &reachable),
            None => url.clone(),
        };
        let fields: Vec<(String, String)> = match form.kind {
            FormKind::Login => {
                anyhow::ensure!(
                    !submitted_login,
                    "Dex showed its login form again — are {username}'s credentials right?"
                );
                submitted_login = true;
                vec![
                    ("login".to_string(), username.to_string()),
                    ("password".to_string(), password.to_string()),
                ]
            }
            FormKind::Approval => {
                let mut fields = form.hidden;
                fields.push(("approval".to_string(), "approve".to_string()));
                fields
            }
        };
        url = target;
        response = client
            .post(url.clone())
            .form(&fields)
            .send()
            .with_context(|| format!("POST {url}"))?;
    }
    anyhow::bail!("Dex login did not reach the callback within {MAX_STEPS} steps")
}

fn pkce_challenge(verifier: &str) -> String {
    let hex = rise_backend_auth::sha256_hex(verifier.as_bytes());
    let digest: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("sha256_hex emits hex"))
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Where the harness reaches Dex: the origin of its (reachable) token endpoint.
fn reachable_base(dex: &DexEndpoint) -> Result<Url> {
    let mut base = Url::parse(&dex.token_url).context("parse Dex token URL")?;
    base.set_path("/");
    base.set_query(None);
    Ok(base)
}

/// Swap the issuer's origin for the reachable one, leaving other hosts alone.
fn reroute(url: &Url, issuer: &Url, reachable: &Url) -> Url {
    if url.origin() != issuer.origin() {
        return url.clone();
    }
    let mut rerouted = url.clone();
    // Both are http(s) URLs with a host, so these cannot fail.
    let _ = rerouted.set_scheme(reachable.scheme());
    let _ = rerouted.set_host(reachable.host_str());
    let _ = rerouted.set_port(reachable.port());
    rerouted
}

fn query_param(url: &str, name: &str) -> Result<Option<String>> {
    let url = Url::parse(url).with_context(|| format!("parse {url}"))?;
    Ok(url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned()))
}

/// The code from the final redirect, after checking it answers this login.
fn code_from_callback(callback: &str, expected_state: Option<&str>) -> Result<String> {
    if let Some(error) = query_param(callback, "error")? {
        anyhow::bail!("Dex refused the login: {error}");
    }
    let state = query_param(callback, "state")?;
    anyhow::ensure!(
        state.as_deref() == expected_state,
        "callback state {state:?} does not match the authorize request ({expected_state:?})"
    );
    query_param(callback, "code")?.context("callback carries no code")
}

#[derive(Debug, PartialEq)]
enum FormKind {
    Login,
    Approval,
}

#[derive(Debug)]
struct Form {
    kind: FormKind,
    action: Option<String>,
    hidden: Vec<(String, String)>,
}

/// Recognize Dex's password or approval form. A deliberately small scanner:
/// the harness only needs the form's action and hidden fields.
fn parse_form(html: &str) -> Option<Form> {
    let start = html.find("<form")?;
    let end = html[start..]
        .find("</form>")
        .map_or(html.len(), |e| start + e);
    let form = &html[start..end];
    let open_end = form.find('>')?;
    let action = attribute(&form[..open_end], "action").filter(|a| !a.is_empty());
    let kind = if form.contains("name=\"password\"") {
        FormKind::Login
    } else if form.contains("name=\"approval\"") {
        FormKind::Approval
    } else {
        return None;
    };
    let hidden = form
        .match_indices("<input")
        .filter_map(|(at, _)| {
            let tag = &form[at..at + form[at..].find('>')?];
            (attribute(tag, "type").as_deref() == Some("hidden"))
                .then(|| Some((attribute(tag, "name")?, attribute(tag, "value")?)))
                .flatten()
        })
        .collect();
    Some(Form {
        kind,
        action,
        hidden,
    })
}

/// A double-quoted attribute's value, HTML entities decoded.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let at = tag.find(&needle)? + needle.len();
    let value = &tag[at..at + tag[at..].find('"')?];
    Some(
        value
            .replace("&amp;", "&")
            .replace("&#43;", "+")
            .replace("&#34;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_base64url_sha256_of_the_verifier() {
        // BASE64URL(SHA256(verifier)) without padding, computed independently
        // (Python's hashlib + urlsafe_b64encode).
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mJ92K9iqpE9WVuoGjgkS9UGbsOAdmE"),
            "vYRvzBlzDGRQEEpoWPl0WBLRuQ5h-wRiW6DX-QMzJP8"
        );
    }

    #[test]
    fn recognizes_dex_login_form() {
        let html = r#"<html><body>
            <form method="post" action="/dex/auth/local/login?back=&amp;state=abc">
              <input tabindex="1" required id="login" name="login" type="text">
              <input tabindex="2" required id="password" name="password" type="password">
              <button tabindex="3" id="submit-login" type="submit">Login</button>
            </form></body></html>"#;
        let form = parse_form(html).expect("a login form");
        assert_eq!(form.kind, FormKind::Login);
        assert_eq!(
            form.action.as_deref(),
            Some("/dex/auth/local/login?back=&state=abc")
        );
    }

    #[test]
    fn recognizes_dex_approval_form() {
        let html = r#"<form method="post">
              <input type="hidden" name="req" value="req-123"/>
              <input type="hidden" name="approval" value="approve">
              <button type="submit">Grant Access</button>
            </form>"#;
        let form = parse_form(html).expect("an approval form");
        assert_eq!(form.kind, FormKind::Approval);
        assert_eq!(form.action, None);
        assert!(form
            .hidden
            .contains(&("req".to_string(), "req-123".to_string())));
    }

    #[test]
    fn a_page_without_a_known_form_is_not_a_form() {
        assert!(parse_form("<html><body>Internal Server Error</body></html>").is_none());
        assert!(parse_form(r#"<form action="/x"><input name="q"></form>"#).is_none());
    }

    #[test]
    fn reroutes_only_the_issuer_origin() {
        let issuer = Url::parse("http://rise-dex:5556/dex").unwrap();
        let reachable = Url::parse("http://127.0.0.1:5556/").unwrap();
        let at_issuer = Url::parse("http://rise-dex:5556/dex/auth?x=1").unwrap();
        assert_eq!(
            reroute(&at_issuer, &issuer, &reachable).as_str(),
            "http://127.0.0.1:5556/dex/auth?x=1"
        );
        let elsewhere = Url::parse("http://localhost:8765/callback?code=c").unwrap();
        assert_eq!(reroute(&elsewhere, &issuer, &reachable), elsewhere);
    }

    #[test]
    fn the_callback_must_answer_this_login() {
        let ok = "http://localhost:8765/callback?code=the-code&state=s1";
        assert_eq!(code_from_callback(ok, Some("s1")).unwrap(), "the-code");
        assert!(code_from_callback(ok, Some("s2")).is_err());
        assert!(
            code_from_callback(ok, None).is_err(),
            "an unrequested state"
        );
        let refused = "http://localhost:8765/callback?error=access_denied&state=s1";
        assert!(code_from_callback(refused, Some("s1")).is_err());

        // Rise's CLI flow is bound by PKCE and sends no state.
        let stateless = "http://localhost:8765/callback?code=the-code";
        assert_eq!(code_from_callback(stateless, None).unwrap(), "the-code");
        assert!(code_from_callback(stateless, Some("s1")).is_err());
    }
}
