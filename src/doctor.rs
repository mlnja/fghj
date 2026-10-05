//! The preflight self-check behind `fghj doctor` and the UI's "Doctor" tab.
//!
//! fghj reaches into more of the host than a typical dev tool: a Docker
//! daemon with BuildKit, two privileged ports on a loopback alias that
//! doesn't exist by default, `/etc/resolver` files pointing at an *ephemeral*
//! DNS port, and a root CA in the System keychain. Each of those can go
//! missing independently — a Docker Desktop upgrade, a `sudo ifconfig lo0
//! -alias`, an OS update that resets the resolver directory, a keychain
//! reset — and when one does, the symptom is almost never the cause. A
//! browser showing `ERR_CONNECTION_REFUSED` looks the same whether the
//! alias is gone, the proxy isn't bound, or DNS is answering from somewhere
//! else entirely.
//!
//! So rather than inferring from desired state, every check here reads the
//! real thing, and the DNS check reads it the way a browser would: through
//! the OS resolver, end to end. That one matters most because the port in
//! `/etc/resolver/fghj.internal` is assigned fresh on every `fghjd` start and
//! recorded nowhere else — a resolver file left behind by a previous process
//! looks perfectly healthy on disk and resolves nothing at all.
//!
//! Checks never mutate. A doctor that fixes things is a doctor you can't
//! trust to tell you what was wrong; the hints say which existing command to
//! run instead.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bollard::Docker;
use serde::{Deserialize, Serialize};

use crate::daemon::control::DaemonControl;
use crate::web::proxy::{HTTP_PORT, HTTPS_PORT, PROXY_IP};
use crate::{dns, web};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Checked, and it's the way it should be.
    Pass,
    /// Not how it should be, but nothing is broken *yet* — either it only
    /// affects a subset of features, or fghj will fix it on the next start.
    Warn,
    /// Something fghj needs is missing or wrong, and it will visibly
    /// misbehave until it's fixed.
    Fail,
}

/// One probe and what it found. `detail` is always populated, including on a
/// pass — "resolved to 127.222.0.1" is the useful part of a green check, and
/// a report you can paste into a bug is worth more than a column of ticks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// Stable machine-readable id, never shown as a heading.
    pub name: String,
    /// One-line human summary of what was checked.
    pub title: String,
    pub verdict: Verdict,
    pub detail: String,
    /// What to do about it — a concrete command wherever one exists. `None`
    /// on a pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The `/daemon/doctor` response body, and what `fghj doctor` deserializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether anything failed outright — `fghj doctor`'s exit code, so it
    /// can gate a shell script or a CI step. A `Warn` deliberately doesn't
    /// count: those are "some feature is unavailable", not "fghj is broken".
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.verdict == Verdict::Fail)
    }
}

impl Check {
    fn pass(name: &str, title: &str, detail: String) -> Self {
        Self {
            name: name.to_string(),
            title: title.to_string(),
            verdict: Verdict::Pass,
            detail,
            hint: None,
        }
    }

    fn warn(name: &str, title: &str, detail: String, hint: &str) -> Self {
        Self {
            name: name.to_string(),
            title: title.to_string(),
            verdict: Verdict::Warn,
            detail,
            hint: Some(hint.to_string()),
        }
    }

    fn fail(name: &str, title: &str, detail: String, hint: &str) -> Self {
        Self {
            name: name.to_string(),
            title: title.to_string(),
            verdict: Verdict::Fail,
            detail,
            hint: Some(hint.to_string()),
        }
    }
}

/// Docker Engine API 1.39 (Engine 18.09) is where the `/session` endpoint
/// BuildKit's gRPC driver needs first appears — the honest capability
/// signal, since nothing in `/version` or `/info` reports "BuildKit" as a
/// server feature. `docker::build_image` has no fallback path anymore, so a
/// daemon below this builds nothing at all.
const BUILDKIT_MIN_API: (u32, u32) = (1, 39);

/// `"1.47"` -> `(1, 47)`. Tolerates a trailing patch segment (`"1.47.0"`)
/// and anything else that parses; returns `None` on anything that doesn't,
/// so an unexpected format reads as "can't tell" rather than "too old".
fn parse_api_version(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Whether `ifconfig lo0` output carries `ip` as an `inet` address.
/// Separated from the `ifconfig` call so the parse is testable without a
/// loopback interface to point it at.
fn lo0_has_alias(ifconfig_output: &str, ip: Ipv4Addr) -> bool {
    let needle = ip.to_string();
    ifconfig_output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("inet") && words.next() == Some(needle.as_str())
    })
}

async fn docker_checks(docker: &Docker) -> Vec<Check> {
    let version = match docker.version().await {
        Ok(v) => v,
        Err(e) => {
            return vec![
                Check::fail(
                    "docker",
                    "Docker Engine reachable",
                    format!("{e}"),
                    "start Docker (Docker Desktop, OrbStack, colima…) and make sure \
                     `docker ps` works, then restart fghjd",
                ),
                // Reported rather than skipped: a tab with a check missing
                // reads as a UI bug, where an explicit "couldn't tell" reads
                // as what it is.
                Check::warn(
                    "buildkit",
                    "BuildKit available",
                    "not determined — the Docker daemon is unreachable".to_string(),
                    "fix the Docker check above first",
                ),
            ];
        }
    };

    let server = version.version.unwrap_or_else(|| "unknown".to_string());
    let api = version.api_version.unwrap_or_else(|| "unknown".to_string());
    let docker_check = Check::pass(
        "docker",
        "Docker Engine reachable",
        format!("server {server}, API {api}"),
    );

    let buildkit = match parse_api_version(&api) {
        Some(parsed) if parsed >= BUILDKIT_MIN_API => Check::pass(
            "buildkit",
            "BuildKit available",
            format!("API {api} supports the BuildKit session endpoint"),
        ),
        Some(_) => Check::fail(
            "buildkit",
            "BuildKit available",
            format!(
                "API {api} predates the BuildKit session endpoint (needs {}.{})",
                BUILDKIT_MIN_API.0, BUILDKIT_MIN_API.1
            ),
            "upgrade Docker — fghj builds every image through BuildKit and has no \
             fallback builder",
        ),
        None => Check::warn(
            "buildkit",
            "BuildKit available",
            format!("could not parse the reported API version ({api})"),
            "builds may still work; report this if they don't",
        ),
    };

    vec![docker_check, buildkit]
}

fn daemon_check(daemon: &DaemonControl) -> Check {
    if daemon.is_active() {
        return Check::pass(
            "daemon",
            "fghjd is active",
            "serving DNS, 80 and 443".to_string(),
        );
    }
    let detail = if daemon.is_idle_requested() {
        "idle — `fghj daemon stop` was the last explicit instruction".to_string()
    } else {
        "idle — activation failed or hasn't happened yet".to_string()
    };
    // Warn, not Fail: idle is a state the operator can legitimately ask for,
    // and every check below it is *expected* to fail while it holds. Calling
    // that a failure would mean `fghj daemon stop` leaves a doctor full of
    // red for a machine that is doing exactly what it was told.
    Check::warn(
        "daemon",
        "fghjd is active",
        detail,
        "run `fghj daemon start` — nothing below this will pass while fghjd is idle",
    )
}

/// How long to wait for the proxy's own loopback address to accept a
/// connection. It's on this machine, behind no network: either something is
/// listening or nothing is, and a second is already generous.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

fn proxy_port_check(port: u16, name: &str, title: &str) -> Check {
    let addr = SocketAddr::V4(SocketAddrV4::new(PROXY_IP, port));
    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(_) => Check::pass(name, title, format!("{addr} accepts connections")),
        Err(e) => Check::fail(
            name,
            title,
            format!("{addr} is not accepting connections: {e}"),
            "run `fghj daemon start`; if it reports the port is in use, something else \
             on this machine has 80/443 (another dev proxy, nginx, Docker Desktop's own \
             port binding)",
        ),
    }
}

fn loopback_alias_check() -> Check {
    let name = "loopback-alias";
    let title = "proxy loopback alias";
    if !cfg!(target_os = "macos") {
        // The alias only exists on macOS: everywhere else the whole of
        // 127/8 already routes to loopback, so there is nothing to check and
        // nothing that can be missing. See `raw_net::add_loopback_alias`.
        return Check::pass(
            name,
            title,
            format!("{PROXY_IP} needs no alias on this platform"),
        );
    }
    let output = Command::new("ifconfig").arg("lo0").output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            if lo0_has_alias(&text, PROXY_IP) {
                Check::pass(name, title, format!("{PROXY_IP} is up on lo0"))
            } else {
                Check::fail(
                    name,
                    title,
                    format!("{PROXY_IP} is not assigned to lo0"),
                    "run `fghj daemon start` — it re-adds the alias; nothing can reach the \
                     proxy without it",
                )
            }
        }
        Ok(o) => Check::warn(
            name,
            title,
            format!(
                "`ifconfig lo0` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            "check the alias by hand with `ifconfig lo0`",
        ),
        Err(e) => Check::warn(
            name,
            title,
            format!("could not run `ifconfig`: {e}"),
            "check the alias by hand with `ifconfig lo0`",
        ),
    }
}

fn resolver_files_check() -> Check {
    let name = "resolver-files";
    let title = "/etc/resolver entries";
    if !cfg!(target_os = "macos") {
        return Check::warn(
            name,
            title,
            "automatic OS DNS routing isn't implemented on this platform".to_string(),
            "point your resolver at fghjd's DNS port manually, or use the \
             container-side names only",
        );
    }
    let zones = dns::managed_resolver_zones(Path::new("/etc/resolver"));
    let has = |zone: &str| zones.iter().any(|(z, _)| z == zone);
    let missing: Vec<&str> = [dns::ZONE, dns::ZONE_RAW]
        .into_iter()
        .filter(|z| !has(z))
        .collect();
    if missing.is_empty() {
        let listed: Vec<String> = zones
            .iter()
            .map(|(z, port)| format!("{z} -> 127.0.0.1:{port}"))
            .collect();
        return Check::pass(name, title, listed.join(", "));
    }
    Check::fail(
        name,
        title,
        format!("no resolver file for {}", missing.join(" or ")),
        "run `fghj daemon start` — it rewrites /etc/resolver; needs root",
    )
}

/// The one check that proves the whole DNS path rather than its parts: ask
/// the OS to resolve a name in fghj's zone, exactly as a browser would, and
/// see whether the answer is the proxy.
///
/// This is where a stale `/etc/resolver` file shows up. The port in that
/// file is assigned fresh on each `fghjd` start and stored nowhere else, so a
/// file written by a previous process points at a port nobody is listening
/// on — `resolver_files_check` passes, and every name in the zone silently
/// fails to resolve. Going through `getaddrinfo` catches that, and costs
/// nothing but a lookup.
fn dns_resolution_check() -> Check {
    let name = "dns";
    let title = "fghj.internal resolves through the OS";
    // Any name under the zone works: the server answers `dns::ANSWER` for
    // the whole zone regardless of whether a workspace has claimed the name.
    let probe = format!("doctor.{}", dns::ZONE);
    match (probe.as_str(), 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let v4: Vec<Ipv4Addr> = addrs
                .filter_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
                .collect();
            if v4.contains(&PROXY_IP) {
                Check::pass(name, title, format!("{probe} -> {PROXY_IP}"))
            } else {
                let got = v4
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Check::fail(
                    name,
                    title,
                    format!(
                        "{probe} resolved to [{got}], not {PROXY_IP} — something other than \
                         fghjd is answering for this zone"
                    ),
                    "check /etc/resolver/fghj.internal, then run `fghj daemon restart`",
                )
            }
        }
        Err(e) => Check::fail(
            name,
            title,
            format!("{probe} did not resolve: {e}"),
            "run `fghj daemon restart` — the resolver file may point at a DNS port from a \
             previous fghjd process, which looks fine on disk and resolves nothing",
        ),
    }
}

fn ca_trust_check() -> Check {
    let name = "ca-trust";
    let title = "root CA trusted by the system";
    let cert = web::ca::ca_cert_path(&crate::daemon::ca_dir());
    if !cert.exists() {
        return Check::fail(
            name,
            title,
            format!("{} does not exist", cert.display()),
            "start fghjd once as root (`sudo fghjd`) — it generates the CA on first run",
        );
    }
    if !cfg!(target_os = "macos") {
        return Check::warn(
            name,
            title,
            format!(
                "{} exists, but automatic system trust isn't implemented on this platform",
                cert.display()
            ),
            "add that file to your system trust store by hand so browsers accept \
             fghj's certificates",
        );
    }
    if web::ca::is_trusted_on_macos(&cert) {
        Check::pass(
            name,
            title,
            format!("{} verifies against the System keychain", cert.display()),
        )
    } else {
        Check::fail(
            name,
            title,
            format!("{} is not trusted as a root", cert.display()),
            "restart fghjd — it re-installs trust on start, which raises the macOS \
             authorization prompt you'll need to approve",
        )
    }
}

/// Everything the daemon can check about this host. Blocking probes
/// (`ifconfig`, `security`, `getaddrinfo`, TCP connects) are moved off the
/// async runtime wholesale rather than one at a time — nothing here is
/// concurrent anyway, and the whole set is well under a second on a healthy
/// machine.
fn host_checks() -> Vec<Check> {
    vec![
        loopback_alias_check(),
        proxy_port_check(HTTPS_PORT, "proxy-https", "proxy listening on 443"),
        proxy_port_check(HTTP_PORT, "proxy-http", "proxy listening on 80"),
        resolver_files_check(),
        dns_resolution_check(),
        ca_trust_check(),
    ]
}

/// The full daemon-side report, in the order a failure cascades: Docker,
/// then whether fghjd is active at all, then the host integrations that only
/// mean anything once it is.
pub async fn daemon_report(docker: &Docker, daemon: &DaemonControl) -> Report {
    let mut checks = docker_checks(docker).await;
    checks.push(daemon_check(daemon));
    match tokio::task::spawn_blocking(host_checks).await {
        Ok(host) => checks.extend(host),
        Err(e) => checks.push(Check::fail(
            "host",
            "host integration checks",
            format!("the host checks panicked: {e}"),
            "report this — it's a bug in fghj, not in your setup",
        )),
    }
    Report { checks }
}

/// The two things `fghj` can only check from where the CLI itself runs: that
/// there's a daemon to ask, and that the one external binary the CLI shells
/// out to is present. Everything else is the daemon's to report — it's the
/// process that owns the ports, the resolver files and the CA.
pub fn client_checks() -> Vec<Check> {
    let socket = crate::daemon::socket_path();
    let daemon = match UnixStream::connect(&socket) {
        Ok(_) => Check::pass(
            "fghjd",
            "fghjd control socket",
            format!("connected to {}", socket.display()),
        ),
        Err(e) => Check::fail(
            "fghjd",
            "fghjd control socket",
            format!("{}: {e}", socket.display()),
            "start the daemon with `sudo fghjd` (or via its launchd/systemd service)",
        ),
    };

    // Warn, not Fail: `cue` is only needed by `fghj validate`. The daemon
    // resolves and runs workspaces without it.
    let cue = match Command::new("cue").arg("version").output() {
        Ok(o) if o.status.success() => Check::pass(
            "cue",
            "cue CLI on PATH",
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("installed")
                .trim()
                .to_string(),
        ),
        _ => Check::warn(
            "cue",
            "cue CLI on PATH",
            "not found".to_string(),
            "install it (`brew install cue`) if you want `fghj validate`; nothing else \
             needs it",
        ),
    };

    vec![daemon, cue]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_versions_parse_to_comparable_pairs() {
        assert_eq!(parse_api_version("1.47"), Some((1, 47)));
        assert_eq!(parse_api_version("1.47.0"), Some((1, 47)));
        assert_eq!(parse_api_version(" 1.39 "), Some((1, 39)));
        assert_eq!(parse_api_version("unknown"), None);
        assert_eq!(parse_api_version("1"), None);
        assert_eq!(parse_api_version(""), None);
    }

    /// The comparison is on the pair, not the string: `"1.9"` must not read
    /// as newer than `"1.39"`, which is exactly what a lexical compare would
    /// have done.
    #[test]
    fn buildkit_threshold_compares_numerically_not_lexically() {
        assert!(parse_api_version("1.39").unwrap() >= BUILDKIT_MIN_API);
        assert!(parse_api_version("1.47").unwrap() >= BUILDKIT_MIN_API);
        assert!(parse_api_version("1.9").unwrap() < BUILDKIT_MIN_API);
        assert!(parse_api_version("1.38").unwrap() < BUILDKIT_MIN_API);
    }

    #[test]
    fn lo0_alias_is_found_by_exact_address_not_substring() {
        let output = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
                      \tinet 127.0.0.1 netmask 0xff000000\n\
                      \tinet 127.222.0.1 netmask 0xff000000\n";
        assert!(lo0_has_alias(output, PROXY_IP));
        assert!(lo0_has_alias(output, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!lo0_has_alias(output, Ipv4Addr::new(127, 222, 0, 2)));
    }

    /// `127.222.0.1` is a prefix of `127.222.0.10`, so a naive `contains`
    /// on the whole output would report an alias that isn't there.
    #[test]
    fn a_longer_address_sharing_our_prefix_is_not_a_match() {
        let output = "\tinet 127.222.0.10 netmask 0xff000000\n";
        assert!(!lo0_has_alias(output, PROXY_IP));
    }

    #[test]
    fn only_a_fail_sets_the_exit_code() {
        let report = Report {
            checks: vec![
                Check::pass("a", "a", "fine".to_string()),
                Check::warn("b", "b", "meh".to_string(), "do something"),
            ],
        };
        assert!(!report.has_failures());

        let report = Report {
            checks: vec![Check::fail("c", "c", "broken".to_string(), "fix it")],
        };
        assert!(report.has_failures());
    }

    /// An unreachable Docker daemon is the one failure that can't be
    /// diagnosed any further, so it has to still report *both* Docker
    /// checks — a report that silently drops the BuildKit row reads as a
    /// broken doctor rather than as "couldn't tell".
    #[tokio::test]
    async fn an_unreachable_docker_still_reports_both_of_its_checks() {
        let checks = docker_checks(&crate::docker::undialled_client()).await;
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["docker", "buildkit"]);
        assert_eq!(checks[0].verdict, Verdict::Fail);
        assert_eq!(checks[1].verdict, Verdict::Warn);
        assert!(checks.iter().all(|c| c.hint.is_some()));
    }

    /// The CLI-side checks run with no daemon and no assumptions about the
    /// machine, so they must always produce a verdict for both rather than
    /// erroring out or returning an empty report.
    #[test]
    fn client_checks_always_report_both_items() {
        let checks = client_checks();
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["fghjd", "cue"]);
        assert!(checks.iter().all(|c| !c.detail.is_empty()));
    }
}
