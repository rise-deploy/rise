/// Validates that custom domains do not overlap with the project default domain patterns.
///
/// This module provides utilities to check if a custom domain would conflict with
/// the automatically generated domain pattern for Rise projects.
///
/// Convert an ingress URL template to a regex pattern.
///
/// The template may contain placeholders like `{project_name}` and `{deployment_group}`.
/// This function converts the template into a regex pattern that can be used to check
/// if a domain would conflict with the project's default domain pattern.
///
/// Templates are bare hostnames (settings validation rejects a path), e.g.
/// `"{project_name}.apps.example.com"` → regex matching `*.apps.example.com`.
///
/// # Arguments
///
/// * `template` - The ingress URL template from the configuration
///
/// # Returns
///
/// An optional regex::Regex object that matches domains conflicting with the template
pub fn template_to_regex(template: &str) -> Option<regex::Regex> {
    let hostname = template;

    // Build regex pattern by replacing placeholders and escaping the rest
    let mut regex_pattern = String::from("^");

    let mut current_pos = 0;
    while current_pos < hostname.len() {
        if let Some(start) = hostname[current_pos..].find('{') {
            let start_pos = current_pos + start;

            // Escape and append everything before the placeholder
            if start_pos > current_pos {
                regex_pattern.push_str(&regex::escape(&hostname[current_pos..start_pos]));
            }

            // Find the closing brace
            if let Some(end) = hostname[start_pos..].find('}') {
                let end_pos = start_pos + end;

                // Replace placeholder with regex pattern
                // Match one or more non-dot characters for single-level subdomain
                regex_pattern.push_str(r"[a-z0-9]([a-z0-9-]*[a-z0-9])?");

                current_pos = end_pos + 1;
            } else {
                // No closing brace found, escape the rest
                regex_pattern.push_str(&regex::escape(&hostname[current_pos..]));
                break;
            }
        } else {
            // No more placeholders, escape the rest
            regex_pattern.push_str(&regex::escape(&hostname[current_pos..]));
            break;
        }
    }

    regex_pattern.push('$');

    // Compile the regex
    regex::Regex::new(&regex_pattern).ok()
}

/// Extract the hostname from a URL.
///
/// # Examples
///
/// - `"https://example.com"` → `"example.com"`
/// - `"http://example.com:8080"` → `"example.com"`
/// - `"example.com"` → `"example.com"`
///
/// # Arguments
///
/// * `url` - The URL to extract hostname from
///
/// # Returns
///
/// The hostname if successfully extracted, None otherwise
fn extract_hostname_from_url(url: &str) -> Option<String> {
    let parsed = if url.contains("://") {
        url::Url::parse(url)
    } else {
        url::Url::parse(&format!("https://{url}"))
    }
    .ok()?;
    Some(parsed.host_str()?.to_string())
}

/// The bare, lowercase IDNA hostname of a control-plane URL or `host[:port]`, for
/// [`reject_control_plane_host`]. `None` for an empty value.
pub fn control_plane_host(url_or_host: &str) -> Option<String> {
    extract_hostname_from_url(url_or_host.trim())
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        .filter(|host| !host.is_empty())
}

/// Refuse a custom domain that is one of Rise's own control-plane hosts — the
/// UI/API, the identity provider, the registry, the sign-in host — or a
/// subdomain of one.
///
/// A project's custom domain is routed to the project's containers, so the
/// host itself would hand control-plane traffic to the project. A subdomain
/// would let the project set cookies scoped to the control-plane host.
/// `control_plane_hosts` are bare lowercase hostnames, as
/// [`control_plane_host`] produces them.
pub fn reject_control_plane_host(
    domain: &str,
    control_plane_hosts: &[String],
) -> Result<(), String> {
    let normalized =
        control_plane_host(domain).ok_or_else(|| format!("Invalid custom domain '{domain}'"))?;
    for host in control_plane_hosts {
        let host = control_plane_host(host)
            .ok_or_else(|| format!("Invalid control-plane host '{host}'"))?;
        if normalized == host || normalized.ends_with(&format!(".{host}")) {
            return Err(format!(
                "Custom domain '{domain}' conflicts with Rise's control-plane host '{host}'"
            ));
        }
    }
    Ok(())
}

/// Check if a custom domain would conflict with project default domain
/// patterns or with a Rise control-plane host (see
/// [`reject_control_plane_host`]).
///
/// # Arguments
///
/// * `domain` - The custom domain to validate
/// * `production_template` - The production ingress URL template
/// * `staging_template` - The optional staging ingress URL template
/// * `environment_template` - The optional environment ingress URL template
/// * `control_plane_hosts` - Rise's own hosts, which no custom domain may claim
///
/// # Returns
///
/// Ok(()) if the domain is valid, Err(reason) if it conflicts with a project pattern or a
/// control-plane host
pub fn validate_custom_domain(
    domain: &str,
    production_template: &str,
    staging_template: Option<&str>,
    environment_template: Option<&str>,
    control_plane_hosts: &[String],
) -> Result<(), String> {
    reject_control_plane_host(domain, control_plane_hosts)?;

    // Check against production template
    if let Some(regex) = template_to_regex(production_template) {
        if regex.is_match(domain) {
            return Err(format!(
                "Custom domain '{}' conflicts with the project default domain pattern (production template)",
                domain
            ));
        }
    }

    // Check against staging template if provided
    if let Some(staging_template) = staging_template {
        if let Some(regex) = template_to_regex(staging_template) {
            if regex.is_match(domain) {
                return Err(format!(
                    "Custom domain '{}' conflicts with the staging deployment domain pattern",
                    domain
                ));
            }
        }
    }

    // Check against environment template if provided
    if let Some(environment_template) = environment_template {
        if let Some(regex) = template_to_regex(environment_template) {
            if regex.is_match(domain) {
                return Err(format!(
                    "Custom domain '{}' conflicts with the environment deployment domain pattern",
                    domain
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_to_regex_subdomain() {
        let regex = template_to_regex("{project_name}.apps.example.com").unwrap();
        assert!(regex.is_match("foo.apps.example.com"));
        assert!(regex.is_match("bar.apps.example.com"));
        assert!(!regex.is_match("apps.example.com")); // Too short
        assert!(!regex.is_match("foo.bar.apps.example.com")); // Too many levels
        assert!(!regex.is_match("other.com"));
    }

    #[test]
    fn test_template_to_regex_staging() {
        let regex =
            template_to_regex("{project_name}-{deployment_group}.preview.example.com").unwrap();
        assert!(regex.is_match("foo-bar.preview.example.com"));
        assert!(regex.is_match("a-b.preview.example.com"));
        // Should not match without the dash
        assert!(!regex.is_match("foobar.preview.example.com"));
        assert!(!regex.is_match("foo.preview.example.com"));
    }

    #[test]
    fn test_extract_hostname_from_url() {
        assert_eq!(
            extract_hostname_from_url("https://example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_hostname_from_url("http://example.com:8080"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_hostname_from_url("example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_hostname_from_url("example.com:8080"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_hostname_from_url("http://example.com/path"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_validate_custom_domain_subdomain_conflict() {
        let result = validate_custom_domain(
            "bar.apps.example.com",
            "{project_name}.apps.example.com",
            None,
            None,
            &[],
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("conflicts with the project default domain pattern"));
    }

    #[test]
    fn test_validate_custom_domain_subdomain_ok() {
        let result = validate_custom_domain(
            "mycustomdomain.com",
            "{project_name}.apps.example.com",
            None,
            None,
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_domain_staging_conflict() {
        let result = validate_custom_domain(
            "foo-bar.preview.example.com",
            "{project_name}.apps.example.com",
            Some("{project_name}-{deployment_group}.preview.example.com"),
            None,
            &[],
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("staging deployment domain pattern"));
    }

    #[test]
    fn test_validate_custom_domain_environment_conflict() {
        let result = validate_custom_domain(
            "staging.myapp.apps.example.com",
            "{project_name}.apps.example.com",
            None,
            Some("{environment}.{project_name}.apps.example.com"),
            &[],
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("environment deployment domain pattern"));
    }

    #[test]
    fn test_validate_custom_domain_multiple_levels() {
        // Should not match domains with too many subdomain levels
        let result = validate_custom_domain(
            "foo.bar.apps.example.com",
            "{project_name}.apps.example.com",
            None,
            None,
            &[],
        );
        assert!(result.is_ok()); // Should be OK since regex doesn't match extra levels
    }

    #[test]
    fn test_validate_custom_domain_rise_public_url_conflict() {
        let result = validate_custom_domain(
            "rise.example.com",
            "{project_name}.apps.example.com",
            None,
            None,
            &["rise.example.com".to_string()],
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("conflicts with Rise's control-plane host"));
    }

    fn control_plane() -> Vec<String> {
        [
            "https://rise.example.com",
            "https://dex.example.com/dex",
            "registry.example.com:5000",
            "https://signin.example.com",
        ]
        .into_iter()
        .filter_map(control_plane_host)
        .collect()
    }

    /// Every control-plane host is refused, not just the public URL's, and
    /// so is any subdomain of one — whatever the case or a trailing dot.
    #[test]
    fn control_plane_hosts_and_their_subdomains_are_refused() {
        let hosts = control_plane();
        assert_eq!(
            hosts,
            [
                "rise.example.com",
                "dex.example.com",
                "registry.example.com",
                "signin.example.com"
            ]
        );
        for domain in [
            "rise.example.com",
            "dex.example.com",
            "DEX.example.com.",
            "registry.example.com",
            "signin.example.com",
            "evil.rise.example.com",
            "a.b.dex.example.com",
        ] {
            let error = validate_custom_domain(
                domain,
                "{project_name}.apps.example.com",
                None,
                None,
                &hosts,
            )
            .expect_err(domain);
            assert!(error.contains("control-plane host"), "{error}");
        }
        for domain in ["example.com", "notdex.example.com", "shop.example.org"] {
            assert!(
                reject_control_plane_host(domain, &hosts).is_ok(),
                "{domain} is not a control-plane host"
            );
        }
    }

    #[test]
    fn test_validate_custom_domain_rise_public_url_ok() {
        let result = validate_custom_domain(
            "mycustomdomain.com",
            "{project_name}.apps.example.com",
            None,
            None,
            &["rise.example.com".to_string()],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn control_plane_checks_match_unicode_and_punycode_hosts() {
        assert_eq!(
            control_plane_host("https://user:password@BÜCHER.example.:8443/path"),
            Some("xn--bcher-kva.example".to_string())
        );
        for hosts in [
            vec!["bücher.example".to_string()],
            vec!["xn--bcher-kva.example".to_string()],
        ] {
            for domain in [
                "bücher.example",
                "xn--bcher-kva.example",
                "shop.bücher.example.",
                "shop.xn--bcher-kva.example",
            ] {
                assert!(
                    reject_control_plane_host(domain, &hosts).is_err(),
                    "{domain}"
                );
            }
        }
    }
}
