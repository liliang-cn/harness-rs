//! Where the browser is allowed to go.
//!
//! A headless browser that a model can steer is an SSRF primitive with a
//! JavaScript engine attached. On a multi-tenant server the interesting targets
//! are one hop away and unauthenticated: `http://169.254.169.254/latest/meta-data/`
//! (cloud instance credentials), `http://127.0.0.1:<port>` (the host's own admin
//! APIs), `http://10.x` (the rest of the VPC). Nothing in the prompt has to be
//! malicious for this to fire — a page the agent was told to read can contain a
//! link, a redirect, or a `<meta refresh>` pointing at any of them.
//!
//! The consumer of this crate knows its own network; this crate does not. So the
//! guard is a *trait the host supplies* ([`UrlPolicy`]), with a default
//! ([`PublicOnlyPolicy`]) that is safe for the common case: public internet only.
//!
//! ## Why the checks look paranoid
//!
//! `127.0.0.1` is the *least* interesting way to write loopback. All of these
//! reach it, and Chrome resolves every one of them:
//!
//! | written as            | is                |
//! |-----------------------|-------------------|
//! | `2130706433`          | `127.0.0.1`       |
//! | `0x7f.1`              | `127.0.0.1`       |
//! | `0177.0.0.1`          | `127.0.0.1`       |
//! | `[::1]`               | IPv6 loopback     |
//! | `[::ffff:127.0.0.1]`  | v4-mapped loopback|
//! | `localhost`           | (by name)         |
//! | `whatever.example`    | if its A record says so |
//!
//! A string-prefix check on `"127."` catches exactly one row of that table,
//! which is why [`PublicOnlyPolicy`] canonicalises the host through the same
//! relaxed integer parsing a browser uses and, by default, resolves names before
//! deciding.
//!
//! ## What this cannot do
//!
//! DNS rebinding. We resolve the name, like the check, and then Chrome resolves
//! it again, independently, when it actually connects; a record with a 0-second
//! TTL can differ between the two. Closing that needs a resolving proxy or
//! `--host-resolver-rules` pinning, which is a deployment decision, not a
//! library one. The tool applies a second, cheaper mitigation instead: it
//! re-checks the URL the page *landed on* after every navigation
//! (see [`crate::page`]), which catches the redirect-into-the-VPC case even
//! though it cannot catch a same-name rebind.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A navigation the policy refused, with a reason meant to be shown to the
/// model verbatim — it must be specific enough that the model stops retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenied {
    pub url: String,
    pub reason: String,
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "navigation to `{}` denied: {}", self.url, self.reason)
    }
}

impl std::error::Error for PolicyDenied {}

/// Decides whether the browser may open a URL.
///
/// Consulted before *every* navigation — the initial `open`, and again on the
/// URL the page settled on afterwards. Implementations must be cheap and must
/// not panic; they run on the request path.
///
/// Blanket-implemented for closures, so a host with a one-line rule does not
/// need a type:
///
/// ```
/// use harness_tools_browser::{BrowserTool, UrlPolicy};
/// let tool = BrowserTool::new().with_policy(|url: &str| {
///     if url.starts_with("https://docs.internal.example/") {
///         Ok(())
///     } else {
///         Err("only the internal docs host is reachable from this agent".to_string())
///     }
/// });
/// # let _ = &tool;
/// ```
pub trait UrlPolicy: Send + Sync + 'static {
    fn check(&self, url: &str) -> Result<(), PolicyDenied>;
}

impl<F> UrlPolicy for F
where
    F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
{
    fn check(&self, url: &str) -> Result<(), PolicyDenied> {
        self(url).map_err(|reason| PolicyDenied {
            url: url.to_string(),
            reason,
        })
    }
}

/// The default: http(s) to a public internet address, nothing else.
///
/// Rejects non-http(s) schemes (`file:`, `data:`, `javascript:`, `chrome:` …),
/// then canonicalises the host and rejects loopback, private, link-local,
/// CGNAT, multicast, unspecified and reserved ranges in both address families.
#[derive(Debug, Clone)]
pub struct PublicOnlyPolicy {
    /// Resolve hostnames and check every address they map to.
    ///
    /// On by default, because otherwise the guard is decorative: anyone can
    /// point a public name at `127.0.0.1` (`localtest.me` already does). Turn it
    /// off only where DNS is unavailable or the check must not block — the
    /// literal-IP checks still apply.
    pub resolve_dns: bool,
    /// Hosts allowed through regardless, compared case-insensitively against the
    /// canonical host. This is how a test — or a host that really does want the
    /// agent on one internal service — punches a hole without disabling the rest.
    pub allow_hosts: Vec<String>,
}

impl Default for PublicOnlyPolicy {
    fn default() -> Self {
        Self {
            resolve_dns: true,
            allow_hosts: Vec::new(),
        }
    }
}

impl PublicOnlyPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Same rules, but never touches the resolver. Used by unit tests, and by
    /// hosts that front the agent with their own egress proxy.
    pub fn literal_only() -> Self {
        Self {
            resolve_dns: false,
            allow_hosts: Vec::new(),
        }
    }

    /// Allow one host through the private-address rules.
    ///
    /// The e2e test uses this to reach its own throwaway `127.0.0.1:<port>`
    /// server — which is precisely the shape of hole an attacker wants, so it is
    /// deliberately explicit, per-host, and never a default.
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        self.allow_hosts.push(host.into().to_ascii_lowercase());
        self
    }
}

impl UrlPolicy for PublicOnlyPolicy {
    fn check(&self, url: &str) -> Result<(), PolicyDenied> {
        let deny = |reason: String| PolicyDenied {
            url: url.to_string(),
            reason,
        };

        let parsed = ParsedUrl::parse(url).ok_or_else(|| deny("not a parseable URL".into()))?;

        if !matches!(parsed.scheme.as_str(), "http" | "https") {
            return Err(deny(format!(
                "scheme `{}` is not allowed; only http and https are",
                parsed.scheme
            )));
        }
        if parsed.host.is_empty() {
            return Err(deny("URL has no host".into()));
        }

        let host = parsed.host.to_ascii_lowercase();
        if self.allow_hosts.contains(&host) {
            return Ok(());
        }

        // A literal address, however it was spelled. Decided without DNS.
        if let Some(ip) = parse_host_ip(&host) {
            return match classify(ip) {
                Some(why) => Err(deny(format!("{host} is {why}"))),
                None => Ok(()),
            };
        }

        // A name. Reject the well-known local suffixes outright: they are never
        // public, and on some resolvers they do not resolve at all, which would
        // otherwise let them through the resolution step below.
        for suffix in [
            "localhost",
            ".localhost",
            ".local",
            ".internal",
            ".localdomain",
            ".home.arpa",
        ] {
            let hit = if let Some(bare) = suffix.strip_prefix('.') {
                host.ends_with(suffix) || host == bare
            } else {
                host == suffix
            };
            if hit {
                return Err(deny(format!("`{host}` is a local-only name")));
            }
        }

        if !self.resolve_dns {
            return Ok(());
        }

        // Resolve and judge every answer: a name is only public if *all* of its
        // addresses are. One private A record is enough to make the whole name
        // unusable, because we do not control which one Chrome picks.
        use std::net::ToSocketAddrs;
        let port = parsed
            .port
            .unwrap_or(if parsed.scheme == "https" { 443 } else { 80 });
        let addrs = match (host.as_str(), port).to_socket_addrs() {
            Ok(a) => a.collect::<Vec<_>>(),
            // A name that does not resolve cannot be reached by the browser
            // either, so this is a fail-closed no-op rather than a real risk.
            Err(e) => return Err(deny(format!("`{host}` does not resolve ({e})"))),
        };
        if addrs.is_empty() {
            return Err(deny(format!("`{host}` resolves to no addresses")));
        }
        for a in &addrs {
            if let Some(why) = classify(a.ip()) {
                return Err(deny(format!(
                    "`{host}` resolves to {} which is {why}",
                    a.ip()
                )));
            }
        }
        Ok(())
    }
}

/// A policy that permits everything, for tests and for hosts that do their own
/// egress filtering at the network layer.
///
/// Named to be conspicuous in a code review. If you find this in a
/// multi-tenant deployment without an egress firewall in front of it, that is
/// the bug.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllPolicy;

impl UrlPolicy for AllowAllPolicy {
    fn check(&self, _url: &str) -> Result<(), PolicyDenied> {
        Ok(())
    }
}

/// `None` when the address is fine to visit; `Some(reason)` when it is not.
fn classify(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            // v4-mapped (`::ffff:a.b.c.d`) and v4-compatible (`::a.b.c.d`) carry a
            // v4 address inside a v6 literal; judge the address they actually
            // reach, not the notation.
            if let Some(v4) = v6_embedded_v4(v6) {
                return classify_v4(v4);
            }
            classify_v6(v6)
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_loopback() {
        return Some("a loopback address");
    }
    if ip.is_unspecified() || o[0] == 0 {
        return Some("in the unspecified 0.0.0.0/8 range");
    }
    if ip.is_private() {
        return Some("a private (RFC1918) address");
    }
    if ip.is_link_local() {
        // 169.254.169.254 is the cloud metadata endpoint on AWS/GCP/Azure/DO.
        return Some("a link-local address (cloud metadata lives here)");
    }
    if o[0] == 100 && (64..128).contains(&o[1]) {
        return Some("in the CGNAT 100.64.0.0/10 range");
    }
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return Some("in the IETF-reserved 192.0.0.0/24 range");
    }
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return Some("in the benchmarking 198.18.0.0/15 range");
    }
    if ip.is_multicast() {
        return Some("a multicast address");
    }
    if ip.is_broadcast() {
        return Some("the broadcast address");
    }
    if o[0] >= 240 {
        return Some("in the reserved 240.0.0.0/4 range");
    }
    None
}

fn classify_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let seg = ip.segments();
    if ip.is_loopback() {
        return Some("the IPv6 loopback address");
    }
    if ip.is_unspecified() {
        return Some("the unspecified IPv6 address");
    }
    if (seg[0] & 0xfe00) == 0xfc00 {
        return Some("an IPv6 unique-local (fc00::/7) address");
    }
    if (seg[0] & 0xffc0) == 0xfe80 {
        return Some("an IPv6 link-local (fe80::/10) address");
    }
    if (seg[0] & 0xff00) == 0xff00 {
        return Some("an IPv6 multicast address");
    }
    // NAT64 well-known prefix 64:ff9b::/96 wraps a v4 address; if it survived
    // this far the embedded v4 was public, so it is allowed.
    None
}

/// The v4 address inside `::ffff:a.b.c.d` or the deprecated `::a.b.c.d`, if any.
fn v6_embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let leading_zero = s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0;
    if !leading_zero {
        // 64:ff9b::/96, the NAT64 well-known prefix.
        if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
            return Some(Ipv4Addr::new(
                (s[6] >> 8) as u8,
                s[6] as u8,
                (s[7] >> 8) as u8,
                s[7] as u8,
            ));
        }
        return None;
    }
    if s[5] == 0xffff || (s[5] == 0 && (s[6] != 0 || s[7] != 0)) {
        return Some(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    None
}

/// Turn a URL host component into an IP address if it *is* one, using the same
/// relaxed rules a browser does.
///
/// `std::net::Ipv4Addr::from_str` accepts only strict dotted-quad, so it says
/// "not an IP" to `2130706433` and `0177.0.0.1` — both of which Chrome happily
/// dials as `127.0.0.1`. Getting this wrong is the whole ballgame, so we
/// implement the `inet_aton` grammar the browsers inherited.
pub fn parse_host_ip(host: &str) -> Option<IpAddr> {
    let host = host.trim();
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        // Bracketed literal: IPv6 only, and a scope id (`%eth0`) is not ours to keep.
        let inner = inner.split('%').next().unwrap_or(inner);
        return inner.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    // An unbracketed host with a colon is a bare IPv6 in some sloppy inputs;
    // accept it rather than fall through to "it's a name".
    if host.contains(':') {
        return host.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    parse_ipv4_relaxed(host).map(IpAddr::V4)
}

/// `inet_aton` semantics: 1–4 parts, each decimal / `0`-prefixed octal /
/// `0x`-prefixed hex, with the final part absorbing all remaining low bytes.
fn parse_ipv4_relaxed(s: &str) -> Option<Ipv4Addr> {
    if s.is_empty() || s.ends_with('.') {
        // A trailing dot is the DNS root form (`example.com.`), not an address;
        // and `1.2.3.4.` is rejected by browsers too.
        return None;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in &parts {
        nums.push(parse_ipv4_part(p)?);
    }
    let n = nums.len();
    // Every leading part must fit in one byte; the last absorbs 2^(8*(4-n+1)).
    for v in &nums[..n - 1] {
        if *v > 0xff {
            return None;
        }
    }
    let last = nums[n - 1];
    let last_max: u64 = match n {
        1 => 0xffff_ffff,
        2 => 0x00ff_ffff,
        3 => 0x0000_ffff,
        _ => 0x0000_00ff,
    };
    if last > last_max {
        return None;
    }
    // The n-1 leading parts are one byte each, left-aligned; the last part
    // fills everything below them. `1.2.3.4` → 01 02 03 | 04, `127.1` → 7f |
    // 00 00 01. Note the shift is `8 * (5 - n)` and not `8 * (4 - n)`: the
    // leading parts already sit in the low bytes of `acc` and have to move up
    // past the last part's whole width.
    let acc: u32 = if n == 1 {
        last as u32
    } else {
        let mut lead: u32 = 0;
        for v in &nums[..n - 1] {
            lead = (lead << 8) | (*v as u32);
        }
        (lead << (8 * (5 - n))) | (last as u32)
    };
    Some(Ipv4Addr::from(acc))
}

fn parse_ipv4_part(p: &str) -> Option<u64> {
    if p.is_empty() {
        return None;
    }
    let (radix, digits) = if let Some(hex) = p.strip_prefix("0x").or_else(|| p.strip_prefix("0X")) {
        (16u32, hex)
    } else if p.len() > 1 && p.starts_with('0') {
        (8u32, &p[1..])
    } else {
        (10u32, p)
    };
    if digits.is_empty() {
        // Bare "0x" is not a number; bare "0" already took the decimal branch.
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// The three pieces of a URL this crate needs. Deliberately not a general URL
/// parser — pulling in `url` for scheme/host/port would be the crate's largest
/// dependency, and everything downstream of the host is Chrome's problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl ParsedUrl {
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        // Reject inline control characters up front: Chrome strips tab/CR/LF from
        // URLs before parsing, so `htt\np://` and `http://\t127.0.0.1` are live
        // targets that a naive parser reads as something else entirely.
        if url.chars().any(|c| c.is_control()) {
            return None;
        }
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.trim().to_ascii_lowercase();
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return None;
        }
        // Authority ends at the first `/`, `?` or `#`.
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        // Strip userinfo. `http://expected.example@169.254.169.254/` is the
        // classic way to make a denylist read the wrong host.
        let hostport = match authority.rsplit_once('@') {
            Some((_userinfo, hp)) => hp,
            None => authority,
        };
        let (host, port) =
            if let Some(inner_end) = hostport.strip_prefix('[').map(|_| hostport.find(']')) {
                let idx = inner_end?;
                let host = &hostport[..=idx];
                let port = hostport[idx + 1..]
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok());
                (host, port)
            } else {
                match hostport.rsplit_once(':') {
                    Some((h, p)) => (h, p.parse::<u16>().ok()),
                    None => (hostport, None),
                }
            };
        Some(Self {
            scheme,
            host: host.to_string(),
            port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(url: &str) -> String {
        match PublicOnlyPolicy::literal_only().check(url) {
            Ok(()) => panic!("expected `{url}` to be denied, it was allowed"),
            Err(e) => e.reason,
        }
    }

    fn allowed(url: &str) {
        if let Err(e) = PublicOnlyPolicy::literal_only().check(url) {
            panic!("expected `{url}` to be allowed: {e}");
        }
    }

    #[test]
    fn rejects_loopback_however_it_is_spelled() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/admin",
            "http://127.1/",              // inet_aton short form
            "http://2130706433/",         // decimal
            "http://0x7f000001/",         // hex
            "http://0177.0.0.1/",         // octal
            "http://0x7f.0.0.1/",         // mixed
            "https://[::1]/",             // IPv6
            "http://[::ffff:127.0.0.1]/", // v4-mapped
            "http://[::ffff:7f00:1]/",    // v4-mapped, hex notation
            "http://localhost:3000/",
            "http://foo.localhost/",
            "http://printer.local/",
            "http://db.internal/",
        ] {
            let why = denied(url);
            assert!(!why.is_empty(), "{url} denied without a reason");
        }
    }

    #[test]
    fn rejects_link_local_metadata_endpoint() {
        let why = denied("http://169.254.169.254/latest/meta-data/iam/security-credentials/");
        assert!(why.contains("link-local"), "unexpected reason: {why}");
        // Same address, decimal.
        denied("http://2852039166/");
        // GCP's metadata name is also link-local once resolved; without DNS we
        // only catch the literal, which is why resolve_dns defaults to true.
    }

    #[test]
    fn rejects_private_and_reserved_ranges() {
        for url in [
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://172.31.255.254/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",      // CGNAT
            "http://0.0.0.0/",         // unspecified
            "http://255.255.255.255/", // broadcast
            "http://240.0.0.1/",       // reserved
            "http://224.0.0.1/",       // multicast
            "http://198.18.0.1/",      // benchmarking
            "https://[fe80::1]/",      // v6 link-local
            "https://[fd00::1]/",      // v6 ULA
            "https://[::]/",
            "https://[ff02::1]/",
        ] {
            denied(url);
        }
    }

    #[test]
    fn allows_ordinary_public_targets() {
        allowed("https://example.com/");
        allowed("http://93.184.216.34/");
        allowed("https://[2606:2800:220:1:248:1893:25c8:1946]/");
        allowed("https://sub.domain.example.co.uk:8443/path?q=1#frag");
        // 172.32 is outside RFC1918; the /12 boundary is a classic off-by-one.
        allowed("http://172.32.0.1/");
        allowed("http://100.128.0.1/"); // just past CGNAT
    }

    #[test]
    fn rejects_non_http_schemes() {
        for url in [
            "file:///etc/passwd",
            "chrome://settings/",
            "about://blank",
            "ftp://example.com/",
            "ws://example.com/",
        ] {
            let why = denied(url);
            assert!(why.contains("scheme"), "unexpected reason for {url}: {why}");
        }
        // These have no `://` at all and must not squeak through as "no scheme".
        for url in ["javascript:alert(1)", "data:text/html,<h1>x", "not a url"] {
            denied(url);
        }
    }

    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        // The host here is 169.254.169.254, not example.com.
        let why = denied("http://example.com@169.254.169.254/");
        assert!(why.contains("link-local"), "unexpected reason: {why}");
        // Multiple `@`: the last one wins, same as a browser.
        denied("http://a@b@127.0.0.1/");
    }

    #[test]
    fn control_characters_are_not_a_parser_bypass() {
        denied("http://127.0.0.1\t/");
        denied("http://\n169.254.169.254/");
    }

    #[test]
    fn allow_host_punches_exactly_one_hole() {
        let p = PublicOnlyPolicy::literal_only().allow_host("127.0.0.1");
        assert!(p.check("http://127.0.0.1:34567/index.html").is_ok());
        // …and only that one.
        assert!(p.check("http://127.0.0.2:34567/").is_err());
        assert!(p.check("http://169.254.169.254/").is_err());
        // The hole does not extend to other schemes.
        assert!(p.check("file://127.0.0.1/etc/passwd").is_err());
    }

    #[test]
    fn closures_are_policies() {
        let p = |url: &str| {
            if url.contains("ok") {
                Ok(())
            } else {
                Err("nope".to_string())
            }
        };
        assert!(UrlPolicy::check(&p, "https://ok.example/").is_ok());
        let e = UrlPolicy::check(&p, "https://bad.example/").unwrap_err();
        assert_eq!(e.reason, "nope");
        assert!(e.to_string().contains("bad.example"));
    }

    #[test]
    fn allow_all_is_allow_all() {
        assert!(AllowAllPolicy.check("file:///etc/shadow").is_ok());
    }

    #[test]
    fn relaxed_ipv4_matches_inet_aton() {
        use std::str::FromStr;
        let cases: [(&str, &str); 8] = [
            ("1.2.3.4", "1.2.3.4"),
            ("2130706433", "127.0.0.1"),
            ("127.1", "127.0.0.1"),
            ("127.0.1", "127.0.0.1"),
            ("0x7f.0.0.1", "127.0.0.1"),
            ("0177.0.0.1", "127.0.0.1"),
            ("0xc0a80001", "192.168.0.1"),
            ("3232235777", "192.168.1.1"),
        ];
        for (input, want) in cases {
            assert_eq!(
                parse_ipv4_relaxed(input),
                Some(Ipv4Addr::from_str(want).unwrap()),
                "parsing {input}"
            );
        }
        // Not addresses.
        for bad in [
            "example.com",
            "1.2.3.4.5",
            "256.1.1.1",
            "1.2.3.4.",
            "",
            "0x",
            "99999999999999999999",
            "12a",
        ] {
            assert_eq!(
                parse_ipv4_relaxed(bad),
                None,
                "expected {bad:?} to not parse"
            );
        }
    }

    #[test]
    fn url_parsing_pulls_out_scheme_host_port() {
        let u = ParsedUrl::parse("https://Example.COM:8443/a/b?c#d").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "Example.COM");
        assert_eq!(u.port, Some(8443));

        let u = ParsedUrl::parse("http://[2001:db8::1]:99/x").unwrap();
        assert_eq!(u.host, "[2001:db8::1]");
        assert_eq!(u.port, Some(99));

        let u = ParsedUrl::parse("http://[2001:db8::1]/x").unwrap();
        assert_eq!(u.host, "[2001:db8::1]");
        assert_eq!(u.port, None);

        let u = ParsedUrl::parse("http://host").unwrap();
        assert_eq!(u.host, "host");
        assert_eq!(u.port, None);

        assert!(ParsedUrl::parse("nonsense").is_none());
    }

    #[test]
    fn dns_resolving_policy_rejects_a_name_pointing_at_loopback() {
        // No network needed: "localhost" is in every hosts file, and the
        // local-name suffix rule fires before the resolver anyway.
        let p = PublicOnlyPolicy::new();
        assert!(p.check("http://localhost:1234/").is_err());
        // A name guaranteed by RFC 6761 not to resolve — fail closed.
        let e = p.check("http://nothing.invalid/").unwrap_err();
        assert!(
            e.reason.contains("does not resolve") || e.reason.contains("resolves to"),
            "unexpected reason: {}",
            e.reason
        );
    }
}
