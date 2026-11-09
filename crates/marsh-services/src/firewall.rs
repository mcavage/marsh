//! The engine egress firewall (SECURITY-DESIGN A12).
//!
//! sbx attaches Mark's global credentials and the MCP gateway to every sandbox
//! through one proxy, `gateway.docker.internal:3128`, and any container that
//! sets that proxy acts as Mark. The production engine needs no proxy (its pulls
//! go direct), so `engine-up` closes the address for everything in and below the
//! engine container: it DROPs every TCP connection to the gateway's IPv4 and
//! IPv6 addresses except destination port 53, over TCP and over UDP, in a marsh-owned chain
//! [`CHAIN`] that is jumped to from `OUTPUT` (the engine and anything in its
//! netns), `FORWARD` and `DOCKER-USER` (every container network, including
//! user-defined bridges; Docker keeps a `DOCKER-USER` chain in both families).
//!
//! The rules are installed **before** the production dockerd starts, are
//! verified when `ready` is reported (the report carries the result and the
//! daemon refuses a `ready` without it), and are verified again while the engine
//! runs: losing them is losing the engine.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

/// The sbx gateway: the proxy, the DNS server and the VM's default route. Its
/// address is per VM, so it is resolved at every start.
pub const GATEWAY_NAME: &str = "gateway.docker.internal";
/// The marsh-owned chain, in the `filter` table of both address families.
pub const CHAIN: &str = "MARSH-EGRESS";
const USER_CHAIN: &str = "DOCKER-USER";
/// The only gateway port that stays reachable (DNS), over either protocol.
const ALLOWED_PORT: &str = "53";
/// Every other TCP and UDP destination port of the gateway is dropped.
const PROTOCOLS: [&str; 2] = ["tcp", "udp"];
/// Where the sbx proxy listens (used only by the functional probe).
const PROXY_PORT: u16 = 3128;
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// The gateway's addresses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Gateway {
    pub v4: Vec<Ipv4Addr>,
    pub v6: Vec<Ipv6Addr>,
}

impl Gateway {
    /// Every address as text, IPv4 first.
    #[must_use]
    pub fn addresses(&self) -> Vec<String> {
        self.v4
            .iter()
            .map(ToString::to_string)
            .chain(self.v6.iter().map(ToString::to_string))
            .collect()
    }
}

impl Gateway {
    /// The gateway an engine reported in its `ready` (addresses as text).
    ///
    /// # Errors
    /// An address that does not parse, or the refusals of [`classify`].
    pub fn from_reported(addresses: &[String]) -> Result<Self, String> {
        let parsed = addresses
            .iter()
            .map(|text| {
                text.parse::<IpAddr>()
                    .map_err(|_| format!("marsh: the reported gateway address {text:?} is invalid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        classify(parsed)
    }
}

/// The addresses in `getent ahosts*` output (first column), skipping IPv4-mapped
/// IPv6 forms. The daemon uses it to cross-check what an engine reports against
/// the VM's own resolution.
#[must_use]
pub fn parse_getent(output: &str) -> Vec<IpAddr> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse::<IpAddr>().ok())
        .filter(|address| match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().is_none(),
            IpAddr::V4(_) => true,
        })
        .collect()
}

/// Sort resolved addresses into a [`Gateway`].
///
/// # Errors
/// No address at all, or an address that cannot be the sbx gateway and that
/// dropping would hurt (loopback, unspecified, multicast): a TCP drop to
/// `127.0.0.1` would cut dockerd off from containerd.
pub fn classify(resolved: impl IntoIterator<Item = IpAddr>) -> Result<Gateway, String> {
    let mut gateway = Gateway::default();
    for address in resolved {
        if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
            return Err(format!(
                "marsh: {GATEWAY_NAME} resolves to {address}, which cannot be the sbx gateway"
            ));
        }
        match address {
            IpAddr::V4(address) if !gateway.v4.contains(&address) => gateway.v4.push(address),
            IpAddr::V6(address) if !gateway.v6.contains(&address) => gateway.v6.push(address),
            _ => {}
        }
    }
    if gateway.v4.is_empty() && gateway.v6.is_empty() {
        return Err(format!("marsh: {GATEWAY_NAME} did not resolve"));
    }
    gateway.v4.sort();
    gateway.v6.sort();
    Ok(gateway)
}

/// Resolve the gateway through the engine container's resolver (A and AAAA).
///
/// # Errors
/// The name does not resolve, or resolves to something unusable.
pub fn resolve_gateway() -> Result<Gateway, String> {
    let resolved = (GATEWAY_NAME, 0_u16)
        .to_socket_addrs()
        .map_err(|error| format!("marsh: cannot resolve {GATEWAY_NAME}: {error}"))?;
    classify(resolved.map(|socket| socket.ip()))
}

/// What `ready` carries about the firewall (A12): the addresses covered and
/// whether installation and verification succeeded. A `ready` without a
/// verified report with at least one address is refused by the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FirewallReport {
    pub addresses: Vec<String>,
    /// The rules are present, in the right place, and nothing else is in the chain.
    pub verified: bool,
    /// A TCP connection from the engine to the gateway's proxy port did not connect.
    pub connect_blocked: bool,
}

/// Whether a `ready` may be accepted for the firewall it reports.
///
/// # Errors
/// The reason, when the report is absent, unverified, covers no address or the
/// functional probe connected.
pub fn require_report(report: Option<&FirewallReport>) -> Result<(), String> {
    let Some(report) = report else {
        return Err("marsh: the engine reported ready without a firewall report".to_owned());
    };
    if report.addresses.is_empty() {
        return Err("marsh: the engine firewall covers no gateway address".to_owned());
    }
    if !report.verified {
        return Err("marsh: the engine firewall is not verified".to_owned());
    }
    if !report.connect_blocked {
        return Err("marsh: the engine can still connect to the sbx gateway".to_owned());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Family {
    V4,
    V6,
}

impl Family {
    const fn tool(self) -> &'static str {
        match self {
            Self::V4 => "iptables",
            Self::V6 => "ip6tables",
        }
    }
}

fn tool(family: Family, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(family.tool())
        .arg("-w")
        .args(arguments)
        .output()
        .map_err(|error| format!("{}: {error}", family.tool()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{} {} failed ({}): {}",
            family.tool(),
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn exists(family: Family, arguments: &[&str]) -> bool {
    tool(family, arguments).is_ok()
}

/// Install the rules for every address, in both families. Idempotent: the chain
/// is flushed and refilled and each jump is removed and put back at position 1.
///
/// # Errors
/// The command that failed. The caller must not start the production engine.
pub fn install(gateway: &Gateway) -> Result<(), String> {
    if !gateway.v4.is_empty() {
        install_family(
            Family::V4,
            &gateway
                .v4
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            &["OUTPUT", "FORWARD", USER_CHAIN],
        )?;
    }
    if !gateway.v6.is_empty() {
        install_family(
            Family::V6,
            &gateway
                .v6
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            &["OUTPUT", "FORWARD", USER_CHAIN],
        )?;
    }
    Ok(())
}

fn install_family(family: Family, addresses: &[String], jump_from: &[&str]) -> Result<(), String> {
    if !exists(family, &["-S", CHAIN]) {
        tool(family, &["-N", CHAIN])?;
    }
    tool(family, &["-F", CHAIN])?;
    for address in addresses {
        for protocol in PROTOCOLS {
            tool(
                family,
                &[
                    "-A",
                    CHAIN,
                    "-d",
                    address,
                    "-p",
                    protocol,
                    "!",
                    "--dport",
                    ALLOWED_PORT,
                    "-j",
                    "DROP",
                ],
            )?;
        }
    }
    for chain in jump_from {
        // Docker creates DOCKER-USER itself; making it first means the jump is
        // already in place when dockerd starts and keeps the chain it finds.
        if !exists(family, &["-S", chain]) {
            tool(family, &["-N", chain])?;
        }
        while exists(family, &["-D", chain, "-j", CHAIN]) {}
        tool(family, &["-I", chain, "1", "-j", CHAIN])?;
    }
    Ok(())
}

/// A rule line as comparable tokens: the host mask of `-d` is dropped (and the
/// address re-printed canonically), and the `-m tcp` / `-m udp` match the
/// kernel adds for `-p tcp` / `-p udp` is removed. Nothing else is tolerated,
/// so any extra match (`-s`, `-i`, `-o`, `--sport`, `-m state`, a comment, ...)
/// makes a rule differ.
fn normalized(line: &str) -> Vec<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut tokens = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        match words[index] {
            "-m" if matches!(words.get(index + 1), Some(&("tcp" | "udp"))) => index += 2,
            "-d" if index + 1 < words.len() => {
                let value = words[index + 1];
                let bare = value.split_once('/').map_or(value, |(address, mask)| {
                    if matches!(mask, "32" | "128") {
                        address
                    } else {
                        value
                    }
                });
                let canonical = bare
                    .parse::<IpAddr>()
                    .map_or_else(|_| value.to_owned(), |address| address.to_string());
                tokens.push("-d".to_owned());
                tokens.push(canonical);
                index += 2;
            }
            other => {
                tokens.push(other.to_owned());
                index += 1;
            }
        }
    }
    tokens
}

/// The `-A <chain> ...` rules of one chain in `iptables -S` output, in order,
/// normalized.
fn rules_of(listing: &str, chain: &str) -> Vec<Vec<String>> {
    listing
        .lines()
        .map(normalized)
        .filter(|tokens| {
            tokens.first().map(String::as_str) == Some("-A")
                && tokens.get(1).map(String::as_str) == Some(chain)
        })
        .collect()
}

fn chain_declared(listing: &str, chain: &str) -> bool {
    let declaration = vec!["-N".to_owned(), chain.to_owned()];
    listing.lines().any(|line| normalized(line) == declaration)
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The canonical content of [`CHAIN`] for these addresses, in order: for each
/// address its TCP DROP and then its UDP DROP.
fn expected_rules(addresses: &[String]) -> Vec<Vec<String>> {
    let mut rules = Vec::new();
    for address in addresses {
        for protocol in PROTOCOLS {
            rules.push(words(&[
                "-A",
                CHAIN,
                "-d",
                address,
                "-p",
                protocol,
                "!",
                "--dport",
                ALLOWED_PORT,
                "-j",
                "DROP",
            ]));
        }
    }
    rules
}

/// Check one family's `-S` listing against the canonical expected state.
///
/// [`CHAIN`] must be declared and hold exactly the expected rules in the
/// expected order (equality of the normalized rule list, so a narrowed, negated,
/// reordered, extra or missing rule all fail). The first rule of `OUTPUT` must
/// be exactly the jump to it; so must the first rule of `DOCKER-USER`; and the
/// first rule of `FORWARD` must be exactly that jump or exactly the jump to
/// `DOCKER-USER`.
///
/// # Errors
/// The first difference found.
fn check_listing(family: Family, listing: &str, addresses: &[String]) -> Result<(), String> {
    let name = family.tool();
    if !chain_declared(listing, CHAIN) {
        return Err(format!("{name}: chain {CHAIN} is missing"));
    }
    let actual = rules_of(listing, CHAIN);
    let expected = expected_rules(addresses);
    if actual != expected {
        let difference = expected
            .iter()
            .zip(actual.iter())
            .position(|(wanted, found)| wanted != found)
            .unwrap_or_else(|| expected.len().min(actual.len()));
        return Err(format!(
            "{name}: chain {CHAIN} differs from the expected rules at rule {} (found {} rules, expected {}): {}",
            difference + 1,
            actual.len(),
            expected.len(),
            actual
                .get(difference)
                .map_or_else(|| "(none)".to_owned(), |rule| rule.join(" "))
        ));
    }
    let first = |chain: &str| rules_of(listing, chain).into_iter().next();
    let jump = |chain: &str, target: &str| Some(words(&["-A", chain, "-j", target]));
    if first("OUTPUT") != jump("OUTPUT", CHAIN) {
        return Err(format!(
            "{name}: OUTPUT does not start with the jump to {CHAIN}"
        ));
    }
    if first(USER_CHAIN) != jump(USER_CHAIN, CHAIN) {
        return Err(format!(
            "{name}: {USER_CHAIN} does not start with the jump to {CHAIN}"
        ));
    }
    let forward = first("FORWARD");
    if forward != jump("FORWARD", CHAIN) && forward != jump("FORWARD", USER_CHAIN) {
        return Err(format!(
            "{name}: FORWARD does not reach {CHAIN} before any other rule"
        ));
    }
    Ok(())
}

/// Check the `iptables -S` and `ip6tables -S` listings against the canonical
/// state for `gateway`. Pure: the engine feeds it its own listings, and the
/// daemon feeds it the ones it read itself. A family with no gateway address is
/// not checked (and needs no listing).
///
/// # Errors
/// What differs, or a missing listing.
pub fn check_rules(gateway: &Gateway, v4: Option<&str>, v6: Option<&str>) -> Result<(), String> {
    for (family, addresses, listing) in [
        (
            Family::V4,
            gateway
                .v4
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            v4,
        ),
        (
            Family::V6,
            gateway
                .v6
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            v6,
        ),
    ] {
        if addresses.is_empty() {
            continue;
        }
        let listing = listing.ok_or_else(|| format!("{}: no listing to check", family.tool()))?;
        check_listing(family, listing, &addresses)?;
    }
    Ok(())
}

/// Verify the installed rules for `gateway` (IPv4 and IPv6).
///
/// # Errors
/// What is missing or out of place.
pub fn verify(gateway: &Gateway) -> Result<(), String> {
    let v4 = if gateway.v4.is_empty() {
        None
    } else {
        Some(tool(Family::V4, &["-S"])?)
    };
    let v6 = if gateway.v6.is_empty() {
        None
    } else {
        Some(tool(Family::V6, &["-S"])?)
    };
    check_rules(gateway, v4.as_deref(), v6.as_deref())
}

/// Whether a TCP connection from here to the gateway's proxy port connects. A
/// DROP rule makes it time out; refusing or timing out both count as blocked.
#[must_use]
pub fn proxy_connects(gateway: &Gateway) -> bool {
    let targets = gateway
        .v4
        .iter()
        .map(|address| IpAddr::V4(*address))
        .chain(gateway.v6.iter().map(|address| IpAddr::V6(*address)));
    targets
        .map(|address| SocketAddr::new(address, PROXY_PORT))
        .any(|socket| TcpStream::connect_timeout(&socket, PROBE_TIMEOUT).is_ok())
}

/// Verify the rules and probe them, for the `ready` report.
///
/// # Errors
/// What is wrong; the engine must not report ready.
pub fn report(gateway: &Gateway) -> Result<FirewallReport, String> {
    verify(gateway)?;
    Ok(FirewallReport {
        addresses: gateway.addresses(),
        verified: true,
        connect_blocked: !proxy_connects(gateway),
    })
}

/// Whether the gateway now resolves to an address the installed rules do not
/// cover. A failed resolution is not a change.
#[must_use]
pub fn gateway_moved(installed: &Gateway) -> Option<String> {
    let now = resolve_gateway().ok()?;
    now.v4
        .iter()
        .find(|address| !installed.v4.contains(address))
        .map(ToString::to_string)
        .or_else(|| {
            now.v6
                .iter()
                .find(|address| !installed.v6.contains(address))
                .map(ToString::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn gateway_addresses_are_sorted_into_families_and_unusable_ones_refused() {
        let gateway = classify([
            address("172.17.0.38"),
            address("fd8a:a739:6825::26"),
            address("172.17.0.38"),
        ])
        .unwrap();
        assert_eq!(gateway.addresses(), ["172.17.0.38", "fd8a:a739:6825::26"]);
        for refused in ["127.0.0.1", "0.0.0.0", "::1", "224.0.0.1"] {
            assert!(classify([address(refused)]).is_err(), "{refused}");
        }
        assert!(classify(Vec::new()).is_err());
    }

    #[test]
    fn a_ready_needs_a_verified_report_with_addresses() {
        let good = FirewallReport {
            addresses: vec!["172.17.0.38".into()],
            verified: true,
            connect_blocked: true,
        };
        assert!(require_report(Some(&good)).is_ok());
        assert!(require_report(None).is_err());
        for bad in [
            FirewallReport {
                addresses: Vec::new(),
                ..good.clone()
            },
            FirewallReport {
                verified: false,
                ..good.clone()
            },
            FirewallReport {
                connect_blocked: false,
                ..good
            },
        ] {
            assert!(require_report(Some(&bad)).is_err(), "{bad:?}");
        }
    }

    const V4: &str = "-P INPUT ACCEPT\n-P FORWARD DROP\n-P OUTPUT ACCEPT\n-N DOCKER-USER\n\
        -N MARSH-EGRESS\n-A FORWARD -j DOCKER-USER\n-A FORWARD -j DOCKER-FORWARD\n\
        -A OUTPUT -j MARSH-EGRESS\n-A DOCKER-USER -j MARSH-EGRESS\n-A DOCKER-USER -j RETURN\n\
        -A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j DROP\n\
        -A MARSH-EGRESS -d 172.17.0.38/32 -p udp -m udp ! --dport 53 -j DROP\n";

    const TCP: &str = "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j DROP";
    const UDP: &str = "-A MARSH-EGRESS -d 172.17.0.38/32 -p udp -m udp ! --dport 53 -j DROP";

    fn addresses() -> Vec<String> {
        vec!["172.17.0.38".to_owned()]
    }

    fn without(line: &str) -> String {
        V4.lines()
            .filter(|rule| *rule != line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn replacing(from: &str, to: &str) -> String {
        assert!(V4.contains(from), "{from}");
        V4.replace(from, to)
    }

    #[test]
    fn the_rule_check_accepts_the_installed_shape() {
        assert!(check_listing(Family::V4, V4, &addresses()).is_ok());
        // The shape Docker leaves after it rewrites FORWARD, and an IPv6 host mask.
        let moved = replacing(
            "-A FORWARD -j DOCKER-FORWARD\n",
            "-A FORWARD -j DOCKER-FORWARD\n-A FORWARD -j MARSH-EGRESS\n",
        );
        assert!(check_listing(Family::V4, &moved, &addresses()).is_ok());
        let v6 = "-N DOCKER-USER\n-N MARSH-EGRESS\n-A FORWARD -j DOCKER-USER\n-A OUTPUT -j MARSH-EGRESS\n\
            -A DOCKER-USER -j MARSH-EGRESS\n\
            -A MARSH-EGRESS -d fd8a:a739:6825::26/128 -p tcp -m tcp ! --dport 53 -j DROP\n\
            -A MARSH-EGRESS -d fd8a:a739:6825::26/128 -p udp -m udp ! --dport 53 -j DROP\n";
        assert!(check_listing(Family::V6, v6, &["fd8a:a739:6825::26".to_owned()]).is_ok());
    }

    #[test]
    fn every_loss_or_narrowing_of_the_rules_is_refused() {
        let wanted = addresses();
        for missing in [
            "-N MARSH-EGRESS",
            "-A OUTPUT -j MARSH-EGRESS",
            "-A DOCKER-USER -j MARSH-EGRESS",
            TCP,
            UDP,
        ] {
            assert!(
                check_listing(Family::V4, &without(missing), &wanted).is_err(),
                "{missing}"
            );
        }
        // Narrowed or altered DROP rules: a source, an interface, a source port,
        // another state match, a wrong protocol, a wrong destination, a negated
        // destination, a mask other than the host mask, a different target.
        for narrowed in [
            "-A MARSH-EGRESS -s 172.17.0.2/32 -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -s 192.0.2.1/32 -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -o eth0 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -i eth0 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp --sport 1024:65535 ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -m state --state NEW -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -m comment --comment x -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p udp -m udp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS ! -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.39/32 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.0/16 -p tcp -m tcp ! --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp --dport 53 -j DROP",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j REJECT",
            "-A MARSH-EGRESS -d 172.17.0.38/32 -p tcp -m tcp ! --dport 53 -j ACCEPT",
        ] {
            assert!(
                check_listing(Family::V4, &replacing(TCP, narrowed), &wanted).is_err(),
                "{narrowed}"
            );
        }
        let udp_narrowed =
            "-A MARSH-EGRESS -s 172.17.0.2/32 -d 172.17.0.38/32 -p udp -m udp ! --dport 53 -j DROP";
        assert!(check_listing(Family::V4, &replacing(UDP, udp_narrowed), &wanted).is_err());
        // Wrong position, extra rules, a wrong set of addresses.
        let reordered = replacing(&format!("{TCP}\n{UDP}\n"), &format!("{UDP}\n{TCP}\n"));
        assert!(check_listing(Family::V4, &reordered, &wanted).is_err());
        for extra in ["-A MARSH-EGRESS -j RETURN", "-A MARSH-EGRESS -j ACCEPT"] {
            let before = format!("-N MARSH-EGRESS\n{extra}\n");
            assert!(
                check_listing(
                    Family::V4,
                    &replacing("-N MARSH-EGRESS\n", &before),
                    &wanted
                )
                .is_err(),
                "{extra}"
            );
            assert!(
                check_listing(Family::V4, &format!("{V4}{extra}\n"), &wanted).is_err(),
                "{extra}"
            );
        }
        assert!(check_listing(Family::V4, V4, &["172.17.0.39".to_owned()]).is_err());
        assert!(
            check_listing(
                Family::V4,
                V4,
                &["172.17.0.38".to_owned(), "172.17.0.39".to_owned()]
            )
            .is_err()
        );
        // The jumps: narrowed, in the wrong place, or behind another rule.
        for (from, to) in [
            (
                "-A OUTPUT -j MARSH-EGRESS",
                "-A OUTPUT -o eth0 -j MARSH-EGRESS",
            ),
            (
                "-A DOCKER-USER -j MARSH-EGRESS\n",
                "-A DOCKER-USER -s 10.0.0.0/8 -j MARSH-EGRESS\n",
            ),
            (
                "-A DOCKER-USER -j MARSH-EGRESS\n",
                "-A DOCKER-USER -j RETURN\n-A DOCKER-USER -j MARSH-EGRESS\n",
            ),
            (
                "-A OUTPUT -j MARSH-EGRESS",
                "-A OUTPUT -j ACCEPT\n-A OUTPUT -j MARSH-EGRESS",
            ),
            (
                "-A FORWARD -j DOCKER-USER\n",
                "-A FORWARD -i lo -j ACCEPT\n-A FORWARD -j DOCKER-USER\n",
            ),
            (
                "-A FORWARD -j DOCKER-USER\n",
                "-A FORWARD -s 172.17.0.0/16 -j DOCKER-USER\n",
            ),
        ] {
            assert!(
                check_listing(Family::V4, &replacing(from, to), &wanted).is_err(),
                "{to}"
            );
        }
    }

    #[test]
    fn getent_output_and_reported_addresses_parse() {
        let text = "172.17.0.38     STREAM gateway.docker.internal\n::ffff:172.17.0.38 STREAM x\nfd8a:a739:6825::26 STREAM y\n";
        assert_eq!(
            parse_getent(text),
            [address("172.17.0.38"), address("fd8a:a739:6825::26")]
        );
        let reported = ["172.17.0.38".to_owned(), "fd8a:a739:6825::26".to_owned()];
        assert_eq!(
            Gateway::from_reported(&reported).unwrap().addresses(),
            reported
        );
        assert!(Gateway::from_reported(&["not-an-ip".to_owned()]).is_err());
        assert!(Gateway::from_reported(&["127.0.0.1".to_owned()]).is_err());
    }
}
