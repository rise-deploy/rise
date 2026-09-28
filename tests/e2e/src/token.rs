//! Token helpers: mint the HS256 CI bearer — a legacy Rise session, signed
//! offline over the shared secret.

use anyhow::Result;
use base64::Engine as _;

/// Extract the `jti` claim from a JWT *without* verifying its signature. The
/// harness only needs to tell tokens apart (e.g. to observe the controller
/// re-minting a workload-identity token), so it decodes the payload — the second
/// `.`-separated segment, base64url-no-pad — and reads `jti`. Returns `None` if
/// the input isn't a well-formed JWT or has no `jti`.
pub fn jwt_unverified_jti(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("jti")?.as_str().map(str::to_string)
}

/// Mint the admin CI bearer: a legacy Rise session — HS256 over the shared
/// secret, `iss = aud = public_url`, `email = admin@example.com` (an admin
/// user, so it bypasses ownership checks on the typed APIs).
///
/// It names no `User` resource (no `rise-session+jwt` typ, no `rise_uid`),
/// which is what lets the harness mint it offline before the stack is up and
/// keep using it across an upgrade from an older release. The typed APIs
/// accept it for its lifetime; the generic resource API does not, so a
/// scenario authoring resources logs in through Dex instead (see
/// `ResourceTokenExchange::operator_session`).
pub fn mint_ci_token(secret_b64: &str, public_url: &str) -> Result<String> {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_b64)
        .map_err(|e| anyhow::anyhow!("decode CI token secret: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let claims = serde_json::json!({
        "sub": "rise-ci",
        "email": "admin@example.com",
        "name": "Rise CI",
        "iat": now,
        // 6h TTL: a full minikube + jfrog-vault run (cluster bring-up, two 10m
        // kubectl waits, all scenarios) can take well over an hour, and the
        // bearer is minted once at construction — keep it valid for the whole
        // run.
        "exp": now + 21_600,
        "iss": public_url,
        "aud": public_url,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(&secret),
    )
    .map_err(|e| anyhow::anyhow!("sign CI token: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 32 zero-bytes, base64 — the local/e2e Docker signing secret.
    const SECRET_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    #[test]
    fn mints_admin_bearer_with_expected_claims() {
        let url = "http://rise.localhost:3000";
        let tok = mint_ci_token(SECRET_B64, url).expect("mint");

        // Verify the token via the signer's own HS256/aud-checked path, so the
        // assertions cover a signature-valid token (not just a decoded payload).
        let signer = rise_backend_auth::RiseTokenSigner::new(
            SECRET_B64,
            url.to_string(),
            3600,
            vec!["sub".to_string(), "email".to_string(), "name".to_string()],
            None,
            None,
        )
        .expect("signer");
        let claims = signer.verify_user_jwt(&tok, url).expect("verify");
        assert_eq!(claims.email, "admin@example.com");
        assert_eq!(claims.iss, url);
        assert_eq!(claims.rise_uid, None, "a legacy session names no User");

        // HS256 header.
        let header = tok.split('.').next().unwrap();
        let hdr: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(header)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(hdr["alg"], "HS256");
    }

    #[test]
    fn extracts_jti_without_verifying() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"jti":"abc-123","exp":42}"#);
        let jwt = format!("header.{payload}.sig");
        assert_eq!(jwt_unverified_jti(&jwt).as_deref(), Some("abc-123"));
        // Not a JWT / no jti → None, never a panic.
        assert_eq!(jwt_unverified_jti("not-a-jwt"), None);
        let no_jti = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        assert_eq!(jwt_unverified_jti(&format!("h.{no_jti}.s")), None);
    }
}
