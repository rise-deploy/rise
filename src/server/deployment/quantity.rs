//! Kubernetes resource quantity parsing and validation utilities.
//!
//! Supports parsing CPU quantities (millicores) and memory quantities (bytes)
//! in standard Kubernetes formats.

use anyhow::{bail, Context, Result};

/// Parse a CPU quantity string into millicores.
///
/// Supported formats:
/// - `"500m"` → 500 (millicores)
/// - `"1"` → 1000 (1 core = 1000 millicores)
/// - `"2.5"` → 2500
pub fn parse_cpu_millicores(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty CPU quantity");
    }

    if let Some(millis) = s.strip_suffix('m') {
        let value: u64 = millis
            .parse()
            .with_context(|| format!("invalid CPU millicores value: {s}"))?;
        Ok(value)
    } else {
        let value: f64 = s
            .parse()
            .with_context(|| format!("invalid CPU quantity: {s}"))?;
        if !value.is_finite() {
            bail!("CPU quantity must be a finite number: {s}");
        }
        if value < 0.0 {
            bail!("CPU quantity must be non-negative: {s}");
        }
        Ok((value * 1000.0).round() as u64)
    }
}

/// Parse a memory quantity string into bytes.
///
/// Supported formats:
/// - `"256Mi"` → 268435456
/// - `"1Gi"` → 1073741824
/// - `"512Ki"` → 524288
/// - `"1Ti"` → 1099511627776
/// - `"1048576"` → 1048576 (bare bytes)
pub fn parse_memory_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty memory quantity");
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix("Ti") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix("Gi") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix("Ki") {
        (n, 1u64 << 10)
    } else {
        (s, 1)
    };

    let value: u64 = num_str
        .parse()
        .with_context(|| format!("invalid memory quantity: {s}"))?;
    value
        .checked_mul(multiplier)
        .with_context(|| format!("memory quantity too large: {s}"))
}

/// Split a resource value into its `(request, limit)` strings.
///
/// Accepts either a fixed value (`"256m"`), where request == limit, or a
/// `request-limit` range (`"128m-1"`). Both sides must parse with `parse`, and
/// the request may not exceed the limit. `kind` names the resource in errors.
fn split_request_limit(
    value: &str,
    kind: &str,
    parse: impl Fn(&str) -> Result<u64>,
) -> Result<(String, String)> {
    let value = value.trim();
    let (req, lim) = match value.split('-').collect::<Vec<_>>().as_slice() {
        [single] => (single.trim().to_string(), single.trim().to_string()),
        [req, lim] => (req.trim().to_string(), lim.trim().to_string()),
        _ => bail!("invalid {kind} value '{value}': expected 'value' or 'request-limit'"),
    };
    let req_v = parse(&req).with_context(|| format!("invalid {kind} request: {req}"))?;
    let lim_v = parse(&lim).with_context(|| format!("invalid {kind} limit: {lim}"))?;
    if req_v > lim_v {
        bail!("{kind} request {req} exceeds limit {lim}");
    }
    Ok((req, lim))
}

/// Parse a CPU value into `(request, limit)` strings (see [`split_request_limit`]).
pub fn parse_cpu_request_limit(value: &str) -> Result<(String, String)> {
    split_request_limit(value, "CPU", parse_cpu_millicores)
}

/// Parse a memory value into `(request, limit)` strings (see [`split_request_limit`]).
pub fn parse_memory_request_limit(value: &str) -> Result<(String, String)> {
    split_request_limit(value, "memory", parse_memory_bytes)
}

/// Validate a CPU value (fixed or `request-limit` range) against `[min, max]`.
///
/// Enforces `min <= request <= limit <= max` (the request/limit ordering is
/// checked by [`parse_cpu_request_limit`]).
pub fn validate_cpu_range(value: &str, min: &str, max: &str) -> Result<()> {
    let (req, lim) =
        parse_cpu_request_limit(value).with_context(|| format!("invalid CPU value: {value}"))?;
    let req_v = parse_cpu_millicores(&req)?;
    let lim_v = parse_cpu_millicores(&lim)?;
    let lo = parse_cpu_millicores(min).with_context(|| format!("invalid min CPU: {min}"))?;
    let hi = parse_cpu_millicores(max).with_context(|| format!("invalid max CPU: {max}"))?;
    if req_v < lo {
        bail!("CPU request {req} is below the allowed minimum {min}");
    }
    if lim_v > hi {
        bail!("CPU limit {lim} is above the allowed maximum {max}");
    }
    Ok(())
}

/// Validate a memory value (fixed or `request-limit` range) against `[min, max]`.
///
/// Enforces `min <= request <= limit <= max` (the request/limit ordering is
/// checked by [`parse_memory_request_limit`]).
pub fn validate_memory_range(value: &str, min: &str, max: &str) -> Result<()> {
    let (req, lim) = parse_memory_request_limit(value)
        .with_context(|| format!("invalid memory value: {value}"))?;
    let req_v = parse_memory_bytes(&req)?;
    let lim_v = parse_memory_bytes(&lim)?;
    let lo = parse_memory_bytes(min).with_context(|| format!("invalid min memory: {min}"))?;
    let hi = parse_memory_bytes(max).with_context(|| format!("invalid max memory: {max}"))?;
    if req_v < lo {
        bail!("Memory request {req} is below the allowed minimum {min}");
    }
    if lim_v > hi {
        bail!("Memory limit {lim} is above the allowed maximum {max}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cpu_millicores() {
        assert_eq!(parse_cpu_millicores("500m").unwrap(), 500);
        assert_eq!(parse_cpu_millicores("1").unwrap(), 1000);
        assert_eq!(parse_cpu_millicores("2").unwrap(), 2000);
        assert_eq!(parse_cpu_millicores("2.5").unwrap(), 2500);
        assert_eq!(parse_cpu_millicores("0.1").unwrap(), 100);
        assert_eq!(parse_cpu_millicores("100m").unwrap(), 100);
        assert_eq!(parse_cpu_millicores("1000m").unwrap(), 1000);
    }

    #[test]
    fn test_parse_cpu_millicores_errors() {
        assert!(parse_cpu_millicores("").is_err());
        assert!(parse_cpu_millicores("abc").is_err());
        assert!(parse_cpu_millicores("m").is_err());
        assert!(parse_cpu_millicores("NaN").is_err());
        assert!(parse_cpu_millicores("inf").is_err());
        assert!(parse_cpu_millicores("-inf").is_err());
    }

    #[test]
    fn test_parse_memory_bytes() {
        assert_eq!(parse_memory_bytes("256Mi").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("1Gi").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("512Ki").unwrap(), 512 * 1024);
        assert_eq!(parse_memory_bytes("1Ti").unwrap(), 1u64 << 40);
        assert_eq!(parse_memory_bytes("1048576").unwrap(), 1048576);
        assert_eq!(parse_memory_bytes("64Mi").unwrap(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_errors() {
        assert!(parse_memory_bytes("").is_err());
        assert!(parse_memory_bytes("abc").is_err());
        assert!(parse_memory_bytes("Mi").is_err());
        // Overflow: u64::MAX Ti would overflow
        assert!(parse_memory_bytes("18446744073709551615Ti").is_err());
    }

    #[test]
    fn test_validate_cpu_range() {
        assert!(validate_cpu_range("500m", "100m", "2").is_ok());
        assert!(validate_cpu_range("1", "100m", "2").is_ok());
        assert!(validate_cpu_range("2", "100m", "2").is_ok());
        assert!(validate_cpu_range("100m", "100m", "2").is_ok());

        assert!(validate_cpu_range("50m", "100m", "2").is_err());
        assert!(validate_cpu_range("3", "100m", "2").is_err());
    }

    #[test]
    fn test_validate_memory_range() {
        assert!(validate_memory_range("256Mi", "64Mi", "2Gi").is_ok());
        assert!(validate_memory_range("1Gi", "64Mi", "2Gi").is_ok());
        assert!(validate_memory_range("64Mi", "64Mi", "2Gi").is_ok());
        assert!(validate_memory_range("2Gi", "64Mi", "2Gi").is_ok());

        assert!(validate_memory_range("32Mi", "64Mi", "2Gi").is_err());
        assert!(validate_memory_range("4Gi", "64Mi", "2Gi").is_err());
    }

    #[test]
    fn test_parse_cpu_request_limit() {
        // Fixed value: request == limit.
        assert_eq!(
            parse_cpu_request_limit("256m").unwrap(),
            ("256m".to_string(), "256m".to_string())
        );
        // Range: request and limit differ.
        assert_eq!(
            parse_cpu_request_limit("128m-1").unwrap(),
            ("128m".to_string(), "1".to_string())
        );
        // Whitespace is tolerated around the separator.
        assert_eq!(
            parse_cpu_request_limit(" 128m - 500m ").unwrap(),
            ("128m".to_string(), "500m".to_string())
        );
    }

    #[test]
    fn test_parse_cpu_request_limit_errors() {
        // Request exceeds limit.
        assert!(parse_cpu_request_limit("1-128m").is_err());
        // Empty sides.
        assert!(parse_cpu_request_limit("-1").is_err());
        assert!(parse_cpu_request_limit("128m-").is_err());
        // Too many separators.
        assert!(parse_cpu_request_limit("128m-256m-1").is_err());
        // Unparseable side.
        assert!(parse_cpu_request_limit("abc-1").is_err());
    }

    #[test]
    fn test_parse_memory_request_limit() {
        assert_eq!(
            parse_memory_request_limit("256Mi").unwrap(),
            ("256Mi".to_string(), "256Mi".to_string())
        );
        assert_eq!(
            parse_memory_request_limit("64Mi-256Mi").unwrap(),
            ("64Mi".to_string(), "256Mi".to_string())
        );
        assert!(parse_memory_request_limit("256Mi-64Mi").is_err());
    }

    #[test]
    fn test_validate_cpu_range_with_ranges() {
        // request >= min, limit <= max, request <= limit.
        assert!(validate_cpu_range("128m-1", "100m", "2").is_ok());
        assert!(validate_cpu_range("100m-2", "100m", "2").is_ok());
        // Limit above max.
        assert!(validate_cpu_range("128m-3", "100m", "2").is_err());
        // Request below min.
        assert!(validate_cpu_range("50m-1", "100m", "2").is_err());
    }

    #[test]
    fn test_validate_memory_range_with_ranges() {
        assert!(validate_memory_range("64Mi-1Gi", "64Mi", "2Gi").is_ok());
        // Limit above max.
        assert!(validate_memory_range("64Mi-4Gi", "64Mi", "2Gi").is_err());
        // Request below min.
        assert!(validate_memory_range("32Mi-1Gi", "64Mi", "2Gi").is_err());
    }
}
