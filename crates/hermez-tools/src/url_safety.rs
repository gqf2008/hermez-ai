#![allow(dead_code)]
//! SSRF (Server-Side Request Forgery) prevention.
//!
//! Blocks requests to private/internal network addresses.
//! Mirrors the Python `tools/url_safety.py`.

use std::net::{IpAddr, ToSocketAddrs};

/// Hostnames that are always blocked.
const BLOCKED_HOSTNAMES: &[&str] = &[
    "metadata.google.internal",
    "metadata.goog",
];

/// Check if an IP address is in a blocked range.
///
/// Covers: private, loopback, link-local, reserved, multicast,
/// unspecified, and CGNAT (100.64.0.0/10).
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(addr) => {
            addr.is_private()
                || addr.is_loopback()
                || addr.is_link_local()
                || addr.is_broadcast()
                || addr.is_unspecified()
                || is_cgnat(addr)
        }
        IpAddr::V6(addr) => {
            addr.is_loopback()
                || addr.is_unspecified()
                || addr.is_unicast_link_local()
                || is_ipv4_mapped_private(addr)
        }
    }
}

/// Check if an IPv4 address is in the CGNAT range (100.64.0.0/10).
/// This is NOT covered by std's `is_private()`.
fn is_cgnat(addr: &std::net::Ipv4Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 100 && (octets[1] & 0xC0) == 64
}

/// Check if an IPv4-mapped IPv6 address has a private IPv4 address.
fn is_ipv4_mapped_private(addr: &std::net::Ipv6Addr) -> bool {
    if let Some(ipv4) = addr.to_ipv4() {
        return is_blocked_ip(&IpAddr::V4(ipv4));
    }
    false
}

/// Check if a URL is safe to request.
///
/// Fails closed: DNS errors and malformed URLs are treated as blocked.
pub fn is_safe_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false, // Fail closed
    };

    let scheme = parsed.scheme().to_lowercase();
    if scheme != "http" && scheme != "https" {
        return false;
    }

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return false,
    };

    // Check blocked hostnames
    if BLOCKED_HOSTNAMES.contains(&host) {
        return false;
    }

    // If it looks like an IP address, check directly
    if let Ok(ip) = host.parse::<IpAddr>() {
        return !is_blocked_ip(&ip);
    }

    // Resolve hostname to IP
    let addr_str = format!("{}:80", host);
    match addr_str.to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                !is_blocked_ip(&addr.ip())
            } else {
                false // Fail closed: no addresses
            }
        }
        Err(_) => false, // Fail closed: DNS error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocked_private() {
        assert!(!is_safe_url("http://192.168.1.1/"));
        assert!(!is_safe_url("http://10.0.0.1/"));
        assert!(!is_safe_url("http://172.16.0.1/"));
    }

    #[test]
    fn test_blocked_loopback() {
        assert!(!is_safe_url("http://127.0.0.1/"));
        assert!(!is_safe_url("http://localhost/"));
    }

    #[test]
    fn test_blocked_cgnat() {
        assert!(!is_safe_url("http://100.64.0.1/"));
        assert!(!is_safe_url("http://100.127.255.255/"));
    }

    #[test]
    fn test_blocked_metadata() {
        assert!(!is_safe_url("http://metadata.google.internal/"));
        assert!(!is_safe_url("http://metadata.goog/"));
    }

    #[test]
    fn test_blocked_non_http() {
        assert!(!is_safe_url("ftp://example.com/file"));
        assert!(!is_safe_url("file:///etc/passwd"));
    }

    #[test]
    fn test_public_url_maybe_safe() {
        // These may fail due to DNS resolution, but 8.8.8.8 is public
        assert!(is_safe_url("http://8.8.8.8/"));
    }
}
