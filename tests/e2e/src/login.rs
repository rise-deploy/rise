//! Real Rise logins, driven headlessly against the test Dex, with the harness
//! filling in Dex's login form instead of a person:
//!
//! - [`login`] is the CLI's PKCE authorization-code flow, the path `rise login`
//!   takes, and yields a CLI session.
//! - [`browser_login`] is the web UI's sign-in (`/auth/signin/start` →
//!   `/auth/callback`), and yields the browser session the UI keeps in its
//!   `rise_jwt` cookie.
//!
//! Either way Rise builds the authorize URL, Dex authenticates the user, and
//! Rise exchanges the code on the back channel, so the session names the
//! user's `User` resource exactly as an interactive login's does. There is
//! deliberately no shortcut that trades a presented ID token for a session.

use anyhow::{Context, Result};
use base64::Engine as _;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::Url;

use crate::dex::DexEndpoint;

/// One of the CLI's loopback callbacks, registered on the test Dex client. The
/// harness never listens on it: it reads the code off the final redirect.
const REDIRECT_URI: &str = "http://127.0.0.1:8765/callback";

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
    let expected_state = query_param(authorization_url, "state")?.filter(|state| !state.is_empty());

    let callback = run_dex_login(dex, authorization_url, REDIRECT_URI, username, password)?;
    let code = code_from_callback(&callback, expected_state.as_deref())?;

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

/// Log `username` in through the web UI's sign-in and return the browser
/// session Rise sets as its `rise_jwt` cookie.
pub fn browser_login(
    api_base: &str,
    dex: &DexEndpoint,
    username: &str,
    password: &str,
) -> Result<String> {
    let (_, response) = sign_in(api_base, dex, None, username, password)?;
    anyhow::ensure!(
        response.status().is_success(),
        "sign-in callback answered {}",
        response.status()
    );
    session_cookie(response.headers()).context("sign-in callback set no rise_jwt cookie")
}

/// Sign `username` in to `project`'s app, as a visitor's browser does, and
/// return the app token Rise sets as the app's `rise_jwt` cookie. The app's
/// `/.rise/auth/complete` step uses the app's host and proof cookie while the
/// harness reaches Rise through `api_base`.
pub fn app_login(
    api_base: &str,
    dex: &DexEndpoint,
    project: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    let (mut browser, response) = sign_in(api_base, dex, Some(project), username, password)?;
    // The callback hands the browser to the app's completion step, by
    // redirect or from a page that links to it.
    let complete = if response.status().is_redirection() {
        location(&response)?
    } else {
        let body = response.text().unwrap_or_default();
        let start = body
            .find("/.rise/auth/complete?token=")
            .with_context(|| format!("sign-in callback names no completion step:\n{body}"))?;
        let end = body[start..]
            .find(|c: char| c == '"' || c == '\'' || c.is_whitespace())
            .map_or(body.len(), |len| start + len);
        body[start..end].replace("&amp;", "&")
    };
    let complete = browser.public_url.join(&complete)?;
    query_param(complete.as_str(), "token")?.context("completion step carries no token")?;
    let response = browser.get(&complete)?;
    anyhow::ensure!(
        response.status().is_success(),
        "completion step answered {}",
        response.status()
    );
    session_cookie(response.headers()).context("completion step set no rise_jwt cookie")
}

/// Keep Rise's logical hosts and their proof cookies while connecting through
/// the local API port or Kubernetes port-forward.
struct BrowserLogin {
    client: reqwest::blocking::Client,
    api: Url,
    public_url: Url,
    cookies: Jar,
}

impl BrowserLogin {
    fn new(api_base: &str) -> Result<Self> {
        let client = no_redirect_client()?;
        let api = Url::parse(api_base).context("parse API base")?;
        let metadata: serde_json::Value = client
            .get(api.join("/.well-known/openid-configuration")?)
            .send()
            .context("discover Rise's public origin")?
            .error_for_status()?
            .json()?;
        let public_url = Url::parse(
            metadata["issuer"]
                .as_str()
                .context("discovery has no issuer")?,
        )?;
        Ok(Self {
            client,
            api,
            public_url,
            cookies: Jar::default(),
        })
    }

    fn get(&mut self, url: &Url) -> Result<reqwest::blocking::Response> {
        let host = url.host_str().context("browser URL has no host")?;
        let mut reachable = self.api.join(url.path())?;
        reachable.set_query(url.query());
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let mut request = self
            .client
            .get(reachable)
            .header(reqwest::header::HOST, authority);
        if let Some(cookies) = self.cookies.cookies(url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request
            .send()
            .with_context(|| format!("GET {} on {host}", url.path()))?;
        // Cookie scope belongs to the browser URL, not the local transport URL.
        for cookie in response.headers().get_all(reqwest::header::SET_COOKIE) {
            if let Ok(cookie) = cookie.to_str() {
                self.cookies.add_cookie_str(cookie, url);
            }
        }
        Ok(response)
    }
}

/// Walk Rise's browser sign-in through Dex and return Rise's answer to the
/// callback: for the web UI when `project` is `None`, for that app otherwise.
fn sign_in(
    api_base: &str,
    dex: &DexEndpoint,
    project: Option<&str>,
    username: &str,
    password: &str,
) -> Result<(BrowserLogin, reqwest::blocking::Response)> {
    let mut browser = BrowserLogin::new(api_base)?;
    let mut start = browser.public_url.join("/api/v1/auth/signin/start")?;
    if let Some(project) = project {
        start.query_pairs_mut().append_pair("project", project);
    }
    let mut response = browser.get(&start)?;
    let mut authorization_url = None;
    for _ in 0..MAX_STEPS {
        anyhow::ensure!(
            response.status().is_redirection(),
            "sign-in start answered {} instead of redirecting to Dex",
            response.status()
        );
        let next = start.join(&location(&response)?)?;
        if query_param(next.as_str(), "redirect_uri")?.is_some() {
            authorization_url = Some(next);
            break;
        }
        anyhow::ensure!(
            matches!(
                next.path(),
                "/.rise/auth/signin/start" | "/api/v1/auth/signin/begin"
            ),
            "unexpected sign-in hop {}",
            next.path()
        );
        start = next;
        response = browser.get(&start)?;
    }
    let authorization_url = authorization_url.context("sign-in did not reach Dex")?;
    // Rise's own callback, spelled with its public URL, which the harness may
    // not resolve: it is reached through `api_base` instead.
    let redirect_uri = query_param(authorization_url.as_str(), "redirect_uri")?
        .context("authorize URL carries no redirect_uri")?;

    let expected_state = query_param(authorization_url.as_str(), "state")?;
    let callback = run_dex_login(
        dex,
        authorization_url.as_str(),
        &redirect_uri,
        username,
        password,
    )?;
    code_from_callback(&callback, expected_state.as_deref())?;
    let response = browser.get(&Url::parse(&callback)?)?;
    Ok((browser, response))
}

/// The non-empty `rise_jwt` among a response's cookies. A cleared legacy
/// cookie of the same name may accompany it.
fn session_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|cookie| cookie.split(';').next()?.trim().strip_prefix("rise_jwt="))
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn no_redirect_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build login client")
}

fn location(response: &reqwest::blocking::Response) -> Result<String> {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .context("redirect without a Location")
}

/// Walk Dex from the authorize URL to the redirect back to `callback`, and
/// return that redirect's URL (carrying the code, or an error).
fn run_dex_login(
    dex: &DexEndpoint,
    authorization_url: &str,
    callback: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    // Redirects are followed by hand: the last one targets the login's
    // callback, which the caller handles (nothing serves the CLI's loopback
    // one), and every URL Dex hands out is spelled with its issuer host, which
    // the harness may not be able to resolve.
    let client = no_redirect_client()?;
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
            let next = url
                .join(&location(&response)?)
                .context("resolve redirect")?;
            if next.as_str().starts_with(callback) {
                return Ok(next.into());
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
                with_hidden(form.hidden, [("login", username), ("password", password)])
            }
            FormKind::Approval => with_hidden(form.hidden, [("approval", "approve")]),
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

/// A form's hidden fields plus the values the harness fills in. A filled-in
/// value replaces a hidden field of the same name rather than doubling it.
fn with_hidden<const N: usize>(
    hidden: Vec<(String, String)>,
    filled: [(&str, &str); N],
) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = hidden
        .into_iter()
        .filter(|(name, _)| !filled.iter().any(|(filled, _)| filled == name))
        .collect();
    fields.extend(
        filled
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    fields
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
    // Dex echoes an empty `state=` when the request carried none.
    let state = query_param(callback, "state")?.filter(|state| !state.is_empty());
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    struct BrowserHop {
        path: String,
        host: Option<&'static str>,
        cookie: Option<&'static str>,
        response: String,
    }

    fn mock_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!(
            "HTTP/1.1 {status} Mock\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(body);
        response
    }

    fn browser_server(hops: Vec<BrowserHop>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            for hop in hops {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing request {}", hop.path);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept mock browser request: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim(), format!("GET {} HTTP/1.1", hop.path));
                let mut headers = Vec::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
                }
                let header = |name: &str| {
                    headers
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.as_str())
                };
                if let Some(host) = hop.host {
                    assert_eq!(header("host"), Some(host), "{}", hop.path);
                }
                assert_eq!(header("cookie"), hop.cookie, "{}", hop.path);
                stream.write_all(hop.response.as_bytes()).unwrap();
            }
        });
        (api, server)
    }

    #[test]
    fn browser_logins_follow_public_and_app_hosts_with_separate_secure_proofs() {
        const PUBLIC: &str = "https://rise.example:3443";
        const CALLBACK: &str = "https://rise.example:3443/api/v1/auth/callback";
        const APP: &str = "https://app.example:7443";
        const CENTRAL_COOKIE: &str = "__Host-rise_oauth_central=central-proof";
        const APP_COOKIE: &str = "__Host-rise_oauth_app=app-proof";
        for app in [false, true] {
            let mut authorize = Url::parse("http://dex.invalid:5556/dex/auth").unwrap();
            authorize
                .query_pairs_mut()
                .append_pair("redirect_uri", CALLBACK)
                .append_pair("state", "state");
            let mut hops = vec![BrowserHop {
                path: "/.well-known/openid-configuration".into(),
                host: None,
                cookie: None,
                response: mock_response(
                    200,
                    &[("Content-Type", "application/json")],
                    &format!(r#"{{"issuer":"{PUBLIC}"}}"#),
                ),
            }];
            if app {
                hops.extend([
                    BrowserHop {
                        path: "/api/v1/auth/signin/start?project=app".into(),
                        host: Some("rise.example:3443"),
                        cookie: None,
                        response: mock_response(
                            302,
                            &[(
                                "Location",
                                &format!("{APP}/.rise/auth/signin/start?project=app"),
                            )],
                            "",
                        ),
                    },
                    BrowserHop {
                        path: "/.rise/auth/signin/start?project=app".into(),
                        host: Some("app.example:7443"),
                        cookie: None,
                        response: mock_response(
                            302,
                            &[
                                (
                                    "Location",
                                    &format!("{PUBLIC}/api/v1/auth/signin/begin?transaction=tx"),
                                ),
                                (
                                    "Set-Cookie",
                                    &format!(
                                        "{APP_COOKIE}; Secure; HttpOnly; Path=/; SameSite=Lax"
                                    ),
                                ),
                            ],
                            "",
                        ),
                    },
                ]);
            }
            hops.push(BrowserHop {
                path: if app {
                    "/api/v1/auth/signin/begin?transaction=tx"
                } else {
                    "/api/v1/auth/signin/start"
                }
                .into(),
                host: Some("rise.example:3443"),
                cookie: None,
                response: mock_response(
                    302,
                    &[
                        ("Location", authorize.as_str()),
                        (
                            "Set-Cookie",
                            &format!("{CENTRAL_COOKIE}; Secure; HttpOnly; Path=/; SameSite=Lax"),
                        ),
                    ],
                    "",
                ),
            });
            hops.push(BrowserHop {
                path: format!("{}?{}", authorize.path(), authorize.query().unwrap()),
                host: None,
                cookie: None,
                response: mock_response(
                    302,
                    &[("Location", &format!("{CALLBACK}?code=code&state=state"))],
                    "",
                ),
            });
            hops.push(BrowserHop {
                path: "/api/v1/auth/callback?code=code&state=state".into(),
                host: Some("rise.example:3443"),
                cookie: Some(CENTRAL_COOKIE),
                response: if app {
                    mock_response(
                        302,
                        &[(
                            "Location",
                            &format!("{APP}/.rise/auth/complete?token=completion"),
                        )],
                        "",
                    )
                } else {
                    mock_response(
                        200,
                        &[(
                            "Set-Cookie",
                            "rise_jwt=ui-session; Secure; Path=/; HttpOnly",
                        )],
                        "success",
                    )
                },
            });
            if app {
                hops.push(BrowserHop {
                    path: "/.rise/auth/complete?token=completion".into(),
                    host: Some("app.example:7443"),
                    cookie: Some(APP_COOKIE),
                    response: mock_response(
                        200,
                        &[(
                            "Set-Cookie",
                            "rise_jwt=app-session; Secure; Path=/; HttpOnly",
                        )],
                        "success",
                    ),
                });
            }
            let (api, server) = browser_server(hops);
            let dex = DexEndpoint {
                issuer: "http://dex.invalid:5556/dex".into(),
                token_url: format!("{api}/dex/token"),
                client_id: "test".into(),
                client_secret: "test".into(),
            };
            let result = if app {
                app_login(&api, &dex, "app", "user", "password")
            } else {
                browser_login(&api, &dex, "user", "password")
            };
            server.join().unwrap();
            assert_eq!(
                result.unwrap(),
                if app { "app-session" } else { "ui-session" }
            );
        }
    }

    #[test]
    fn logical_cookie_scope_refuses_other_hosts_and_insecure_requests() {
        let mut hops = vec![BrowserHop {
            path: "/.well-known/openid-configuration".into(),
            host: None,
            cookie: None,
            response: mock_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"issuer":"https://rise.example"}"#,
            ),
        }];
        for (path, cookie) in [
            ("/set", None),
            ("/insecure", None),
            ("/other-host", None),
            ("/same-host", Some("__Host-rise_oauth_central=proof")),
        ] {
            hops.push(BrowserHop {
                path: path.into(),
                host: None,
                cookie,
                response: mock_response(
                    200,
                    if path == "/set" {
                        &[(
                            "Set-Cookie",
                            "__Host-rise_oauth_central=proof; Secure; HttpOnly; Path=/",
                        )]
                    } else {
                        &[]
                    },
                    "",
                ),
            });
        }
        let (api, server) = browser_server(hops);
        let mut browser = BrowserLogin::new(&api).unwrap();
        for url in [
            "https://rise.example/set",
            "http://rise.example/insecure",
            "https://app.example/other-host",
            "https://rise.example/same-host",
        ] {
            browser.get(&Url::parse(url).unwrap()).unwrap();
        }
        server.join().unwrap();
    }

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
    fn submissions_carry_the_forms_hidden_fields() {
        let html = r#"<form method="post" action="/dex/auth/local/login?state=abc">
              <input type="hidden" name="req" value="req-123">
              <input name="login" type="text">
              <input name="password" type="password">
            </form>"#;
        let form = parse_form(html).expect("a login form");
        assert_eq!(form.kind, FormKind::Login);
        assert_eq!(
            with_hidden(form.hidden, [("login", "ada"), ("password", "pw")]),
            vec![
                ("req".to_string(), "req-123".to_string()),
                ("login".to_string(), "ada".to_string()),
                ("password".to_string(), "pw".to_string()),
            ]
        );

        // The approval form's own hidden `approval` is replaced, not doubled.
        let approval = vec![
            ("req".to_string(), "r".to_string()),
            ("approval".to_string(), "approve".to_string()),
        ];
        assert_eq!(
            with_hidden(approval, [("approval", "approve")]),
            vec![
                ("req".to_string(), "r".to_string()),
                ("approval".to_string(), "approve".to_string()),
            ]
        );
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
        let elsewhere = Url::parse("http://127.0.0.1:8765/callback?code=c").unwrap();
        assert_eq!(reroute(&elsewhere, &issuer, &reachable), elsewhere);
    }

    #[test]
    fn the_callback_must_answer_this_login() {
        let ok = "http://127.0.0.1:8765/callback?code=the-code&state=s1";
        assert_eq!(code_from_callback(ok, Some("s1")).unwrap(), "the-code");
        assert!(code_from_callback(ok, Some("s2")).is_err());
        assert!(
            code_from_callback(ok, None).is_err(),
            "an unrequested state"
        );
        let refused = "http://127.0.0.1:8765/callback?error=access_denied&state=s1";
        assert!(code_from_callback(refused, Some("s1")).is_err());

        // Rise's CLI flow is bound by PKCE and sends no state.
        let stateless = "http://127.0.0.1:8765/callback?code=the-code";
        assert_eq!(code_from_callback(stateless, None).unwrap(), "the-code");
        // ...which Dex echoes back as an empty parameter.
        let echoed = "http://127.0.0.1:8765/callback?code=the-code&state=";
        assert_eq!(code_from_callback(echoed, None).unwrap(), "the-code");
        assert!(code_from_callback(stateless, Some("s1")).is_err());
    }

    #[test]
    fn the_session_cookie_is_the_non_empty_rise_jwt() {
        let mut headers = reqwest::header::HeaderMap::new();
        for cookie in [
            "other=1; Path=/",
            "rise_jwt=; Domain=example.com; Max-Age=0",
            "rise_jwt=the.session.token; HttpOnly; SameSite=Lax; Path=/",
        ] {
            headers.append(reqwest::header::SET_COOKIE, cookie.parse().unwrap());
        }
        assert_eq!(
            session_cookie(&headers).as_deref(),
            Some("the.session.token")
        );
        assert_eq!(session_cookie(&reqwest::header::HeaderMap::new()), None);
    }
}
