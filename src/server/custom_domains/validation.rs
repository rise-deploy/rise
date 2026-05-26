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
/// Supports templates with:
/// - Subdomain-based routing: `{project_name}.apps.example.com`
/// - Path-based routing: `apps.example.com/{project_name}`
/// - Mixed routing: `{project_name}.apps.example.com/{deployment_group}`
///
/// # Examples
///
/// - `"{project_name}.apps.example.com"` → regex matching `*.apps.example.com`
/// - `"apps.example.com/{project_name}"` → exact match for `apps.example.com`
/// - `"{project_name}.example.com/{deployment_group}"` → regex matching `*.example.com`
///
/// # Arguments
///
/// * `template` - The ingress URL template from the configuration
///
/// # Returns
///
/// An optional regex::Regex object that matches domains conflicting with the template
pub fn template_to_regex(template: &str) -> Option<regex::Regex> {
    // Extract the hostname part (before any slash for path-based routing)
    let hostname = if let Some(slash_pos) = template.find('/') {
        &template[..slash_pos]
    } else {
        template
    };

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
    // Remove scheme if present
    let without_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };

    // Extract hostname (before port or path)
    let hostname = without_scheme
        .split(&[':', '/'][..])
        .next()
        .unwrap_or("")
        .to_string();

    if hostname.is_empty() {
        None
    } else {
        Some(hostname)
    }
}

/// Check if a custom domain would conflict with project default domain patterns or Rise's public URL.
///
/// # Arguments
///
/// * `domain` - The custom domain to validate
/// * `production_template` - The production ingress URL template
/// * `staging_template` - The optional staging ingress URL template
/// * `environment_template` - The optional environment ingress URL template
/// * `rise_public_url` - The optional Rise public URL to prevent conflicts
///
/// # Returns
///
/// Ok(()) if the domain is valid, Err(reason) if it conflicts with a project pattern or Rise's URL
pub fn validate_custom_domain(
    domain: &str,
    production_template: &str,
    staging_template: Option<&str>,
    environment_template: Option<&str>,
    rise_public_url: Option<&str>,
) -> Result<(), String> {
    // Check against Rise's own public URL
    if let Some(public_url) = rise_public_url {
        if let Some(rise_hostname) = extract_hostname_from_url(public_url) {
            if domain == rise_hostname {
                return Err(format!(
                    "Custom domain '{}' conflicts with Rise's public URL hostname",
                    domain
                ));
            }
        }
    }

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
    fn test_template_to_regex_path_based() {
        let regex = template_to_regex("example.com/{project_name}").unwrap();
        assert!(regex.is_match("example.com"));
        assert!(!regex.is_match("other.com"));
        assert!(!regex.is_match("foo.example.com"));
    }

    #[test]
    fn test_template_to_regex_mixed() {
        // Template with both hostname placeholder and path
        let regex = template_to_regex("{project_name}.example.com/{deployment_group}").unwrap();
        assert!(regex.is_match("foo.example.com"));
        assert!(regex.is_match("bar.example.com"));
        assert!(!regex.is_match("example.com"));
        assert!(!regex.is_match("foo.bar.example.com"));
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
            None,
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
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_domain_path_based_conflict() {
        let result = validate_custom_domain(
            "example.com",
            "example.com/{project_name}",
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("conflicts with the project default domain pattern"));
    }

    #[test]
    fn test_validate_custom_domain_path_based_ok() {
        let result =
            validate_custom_domain("other.com", "example.com/{project_name}", None, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_domain_staging_conflict() {
        let result = validate_custom_domain(
            "foo-bar.preview.example.com",
            "{project_name}.apps.example.com",
            Some("{project_name}-{deployment_group}.preview.example.com"),
            None,
            None,
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
            None,
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
            None,
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
            Some("https://rise.example.com"),
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("conflicts with Rise's public URL"));
    }

    #[test]
    fn test_validate_custom_domain_rise_public_url_ok() {
        let result = validate_custom_domain(
            "mycustomdomain.com",
            "{project_name}.apps.example.com",
            None,
            None,
            Some("https://rise.example.com"),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_domain_mixed_template() {
        // Template with both hostname placeholders and path
        let result = validate_custom_domain(
            "foo.example.com",
            "{project_name}.example.com/{deployment_group}",
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("conflicts with the project default domain pattern"));
    }
}
