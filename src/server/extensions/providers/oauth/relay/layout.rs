//! Path layout mapping `/oidc/{project}/{extension}/*` URLs to oauthrelay resource keys.

use oauthrelay_core::{Endpoint, PathLayout, ResourceKey};

/// Serves every endpoint of an OAuth extension below `/oidc/{project}/{extension}/`:
///
/// | Endpoint | Path below `/oidc/` |
/// |---|---|
/// | authorize | `{project}/{extension}/authorize` |
/// | callback | `{project}/{extension}/callback` |
/// | token | `{project}/{extension}/token` |
/// | jwks | `{project}/{extension}/jwks` |
/// | discovery | `{project}/{extension}/.well-known/openid-configuration` |
///
/// The relay and its upstream share the key `{project}/{extension}`, so the
/// callback, which is keyed by the upstream, lives next to the relay's endpoints.
pub struct RiseLayout;

impl PathLayout for RiseLayout {
    fn parse(&self, path: &str) -> Option<(Endpoint, ResourceKey)> {
        let segments: Vec<&str> = path.split('/').collect();
        let (project, extension, endpoint) = match segments.as_slice() {
            [project, extension, endpoint] => {
                let endpoint = match *endpoint {
                    "authorize" => Endpoint::Authorize,
                    "callback" => Endpoint::Callback,
                    "token" => Endpoint::Token,
                    "jwks" => Endpoint::Jwks,
                    _ => return None,
                };
                (*project, *extension, endpoint)
            }
            [project, extension, ".well-known", "openid-configuration"] => {
                (*project, *extension, Endpoint::Discovery)
            }
            _ => return None,
        };
        // `ResourceKey::new` rejects empty, `.`, `..` and percent-encoded segments.
        let key = ResourceKey::new(format!("{project}/{extension}")).ok()?;
        Some((endpoint, key))
    }

    fn path(&self, endpoint: Endpoint, key: &ResourceKey) -> String {
        let suffix = match endpoint {
            Endpoint::Authorize => "authorize",
            Endpoint::Callback => "callback",
            Endpoint::Token => "token",
            Endpoint::Jwks => "jwks",
            Endpoint::Discovery => ".well-known/openid-configuration",
        };
        format!("{key}/{suffix}")
    }
}

/// Splits a relay key back into its project and extension names.
pub fn split_key(key: &ResourceKey) -> Option<(&str, &str)> {
    let (project, extension) = key.as_str().split_once('/')?;
    (!extension.contains('/')).then_some((project, extension))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ResourceKey {
        ResourceKey::new(value).unwrap()
    }

    #[test]
    fn renders_the_documented_paths() {
        let key = key("demo/oauth-google");
        let paths: Vec<_> = Endpoint::ALL
            .iter()
            .map(|endpoint| RiseLayout.path(*endpoint, &key))
            .collect();
        assert_eq!(
            paths,
            [
                "demo/oauth-google/authorize",
                "demo/oauth-google/callback",
                "demo/oauth-google/token",
                "demo/oauth-google/.well-known/openid-configuration",
                "demo/oauth-google/jwks",
            ]
        );
    }

    #[test]
    fn round_trips_every_endpoint() {
        let key = key("demo/oauth-google");
        for endpoint in Endpoint::ALL {
            let path = RiseLayout.path(endpoint, &key);
            assert_eq!(
                RiseLayout.parse(&path),
                Some((endpoint, key.clone())),
                "{path}"
            );
        }
    }

    #[test]
    fn rejects_paths_outside_the_layout() {
        for path in [
            "",
            "demo",
            "demo/ext",
            "demo/ext/",
            "demo/ext/token/",
            "demo/ext/unknown",
            "demo/ext/extra/token",
            "demo//token",
            "/demo/ext/token",
            "relay/demo/ext/token",
            "upstream/demo/ext/callback",
            "demo/a%2Fb/token",
            "%2E%2E/ext/token",
            "demo/%2E%2E/token",
            "demo/../token",
            "./ext/token",
            "demo/ext/.well-known",
            "demo/ext/.well-known/openid-configuration/",
            "demo/ext/.well-known/oauth-authorization-server",
        ] {
            assert_eq!(RiseLayout.parse(path), None, "{path}");
        }
    }

    #[test]
    fn splits_keys_into_project_and_extension() {
        assert_eq!(split_key(&key("demo/ext")), Some(("demo", "ext")));
        assert_eq!(split_key(&key("demo")), None);
        assert_eq!(split_key(&key("a/b/c")), None);
    }
}
