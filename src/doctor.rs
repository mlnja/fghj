//! The preflight self-check behind `fghj doctor` and the UI's Doctor tab. Every
//! check reads the real thing; DNS is resolved end to end through the OS,
//! since a stale resolver file looks fine on disk. Checks never mutate; hints
//! say what to run.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use bollard::Docker;
use serde::{Deserialize, Serialize};

use crate::daemon::control::DaemonControl;
use crate::web::proxy::{HTTP_PORT, HTTPS_PORT, PROXY_IP};
use crate::{dns, raw_net, web};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    /// Not broken yet: affects some features, or fghj fixes it on next start.
    Warn,
    Fail,
}

/// `detail` is populated on a pass too, so the report is pasteable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub title: String,
    pub verdict: Verdict,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub checks: Vec<Check>,
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

/// API 1.39 adds the `/session` endpoint BuildKit needs; nothing in `/info`
/// reports BuildKit directly.
const BUILDKIT_MIN_API: (u32, u32) = (1, 39);

/// `None` reads as "can't tell", not "too old".
fn parse_api_version(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
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
    // Idle is something the operator can ask for, and everything below is
    // expected to fail while it holds.
    Check::warn(
        "daemon",
        "fghjd is active",
        detail,
        "run `fghj daemon start` — nothing below this will pass while fghjd is idle",
    )
}

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

fn loopback_alias_check(present: Option<Result<bool>>) -> Check {
    let name = "loopback-alias";
    let title = "proxy loopback alias";
    match present {
        // Only macOS needs it; elsewhere 127/8 already routes to loopback.
        None => Check::pass(
            name,
            title,
            format!("{PROXY_IP} needs no alias on this platform"),
        ),
        Some(Ok(true)) => Check::pass(name, title, format!("{PROXY_IP} is up on loopback")),
        Some(Ok(false)) => Check::fail(
            name,
            title,
            format!("{PROXY_IP} is not assigned to loopback"),
            "run `fghj daemon start` — it re-adds the alias; nothing can reach the \
             proxy without it",
        ),
        Some(Err(e)) => Check::warn(
            name,
            title,
            format!("{e:#}"),
            "check the loopback interface's addresses by hand",
        ),
    }
}

fn resolver_files_check(routing: Option<dns::OsRouting>) -> Check {
    let name = "resolver-files";
    let Some(routing) = routing else {
        return Check::warn(
            name,
            "OS DNS routing",
            "automatic OS DNS routing isn't implemented on this platform".to_string(),
            "point your resolver at fghjd's DNS port manually, or use the \
             container-side names only",
        );
    };
    let title = format!("{} entries", routing.location);
    let has = |zone: &str| routing.zones.iter().any(|(z, _)| z == zone);
    let missing: Vec<&str> = [dns::ZONE, dns::ZONE_RAW]
        .into_iter()
        .filter(|z| !has(z))
        .collect();
    if missing.is_empty() {
        let listed: Vec<String> = routing
            .zones
            .iter()
            .map(|(z, port)| format!("{z} -> 127.0.0.1:{port}"))
            .collect();
        return Check::pass(name, &title, listed.join(", "));
    }
    Check::fail(
        name,
        &title,
        format!("no resolver entry for {}", missing.join(" or ")),
        &format!(
            "run `fghj daemon start` — it rewrites {}; needs root",
            routing.location
        ),
    )
}

/// The port in the resolver file changes on each start, so a stale file
/// passes `resolver_files_check` and resolves nothing.
fn dns_resolution_check() -> Check {
    let name = "dns";
    let title = "fghj.internal resolves through the OS";
    // The whole zone answers, claimed or not.
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
                    &format!(
                        "check {} for {}, then run `fghj daemon restart`",
                        dns::os_routing_status().map_or("the OS resolver config", |r| r.location),
                        dns::ZONE
                    ),
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

fn ca_trust_check(cert: &Path, is_trusted: impl FnOnce(&Path) -> Option<bool>) -> Check {
    let name = "ca-trust";
    let title = "root CA trusted by the system";
    if !cert.exists() {
        return Check::fail(
            name,
            title,
            format!("{} does not exist", cert.display()),
            "start fghjd once as root (`sudo fghjd`) — it generates the CA on first run",
        );
    }
    match is_trusted(cert) {
        None => Check::warn(
            name,
            title,
            format!(
                "{} exists, but automatic system trust isn't implemented on this platform",
                cert.display()
            ),
            "add that file to your system trust store by hand so browsers accept \
             fghj's certificates",
        ),
        Some(true) => Check::pass(
            name,
            title,
            format!(
                "{} verifies against {}",
                cert.display(),
                web::trust::store_name()
            ),
        ),
        Some(false) => Check::fail(
            name,
            title,
            format!("{} is not trusted as a root", cert.display()),
            "restart fghjd — it re-installs trust on start, which may raise an OS \
             authorization prompt you'll need to approve",
        ),
    }
}

/// Off the async runtime wholesale; the set takes well under a second.
fn host_checks() -> Vec<Check> {
    vec![
        loopback_alias_check(raw_net::loopback_alias_present(PROXY_IP)),
        proxy_port_check(HTTPS_PORT, "proxy-https", "proxy listening on 443"),
        proxy_port_check(HTTP_PORT, "proxy-http", "proxy listening on 80"),
        resolver_files_check(dns::os_routing_status()),
        dns_resolution_check(),
        ca_trust_check(
            &web::ca::ca_cert_path(&crate::daemon::ca_dir()),
            web::trust::is_trusted,
        ),
    ]
}

/// In cascade order: Docker, then whether fghjd is active, then the rest.
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

/// Only what the CLI can check from where it runs.
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

    // `cue` is only needed by `fghj validate`.
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

    /// Compared as a pair: lexically `"1.9"` > `"1.39"`.
    #[test]
    fn buildkit_threshold_compares_numerically_not_lexically() {
        assert!(parse_api_version("1.39").unwrap() >= BUILDKIT_MIN_API);
        assert!(parse_api_version("1.47").unwrap() >= BUILDKIT_MIN_API);
        assert!(parse_api_version("1.9").unwrap() < BUILDKIT_MIN_API);
        assert!(parse_api_version("1.38").unwrap() < BUILDKIT_MIN_API);
    }

    #[test]
    fn loopback_alias_verdict_follows_the_probe() {
        let unsupported = loopback_alias_check(None);
        assert_eq!(unsupported.verdict, Verdict::Pass);
        assert!(unsupported.detail.contains("needs no alias"));

        assert_eq!(loopback_alias_check(Some(Ok(true))).verdict, Verdict::Pass);

        let missing = loopback_alias_check(Some(Ok(false)));
        assert_eq!(missing.verdict, Verdict::Fail);
        assert!(missing.hint.unwrap().contains("fghj daemon start"));

        let unreadable = loopback_alias_check(Some(Err(anyhow::anyhow!("`ifconfig lo0` failed"))));
        assert_eq!(unreadable.verdict, Verdict::Warn);
        assert!(unreadable.detail.contains("ifconfig lo0"));
    }

    fn routing(zones: &[(&str, u16)]) -> Option<dns::OsRouting> {
        Some(dns::OsRouting {
            location: "/etc/resolver",
            zones: zones.iter().map(|(z, p)| (z.to_string(), *p)).collect(),
        })
    }

    #[test]
    fn resolver_check_warns_where_os_routing_is_unsupported() {
        let check = resolver_files_check(None);
        assert_eq!(check.verdict, Verdict::Warn);
        assert_eq!(check.name, "resolver-files");
    }

    #[test]
    fn resolver_check_passes_with_both_fixed_zones_and_lists_every_zone() {
        let check = resolver_files_check(routing(&[
            (dns::ZONE, 5353),
            (dns::ZONE_RAW, 5353),
            ("myservice.local", 5353),
        ]));
        assert_eq!(check.verdict, Verdict::Pass);
        assert_eq!(check.title, "/etc/resolver entries");
        assert!(check.detail.contains("myservice.local -> 127.0.0.1:5353"));
    }

    #[test]
    fn resolver_check_fails_naming_each_missing_fixed_zone() {
        let check = resolver_files_check(routing(&[(dns::ZONE, 5353), ("myservice.local", 5353)]));
        assert_eq!(check.verdict, Verdict::Fail);
        assert!(check.detail.contains(dns::ZONE_RAW));
        assert!(check.hint.unwrap().contains("/etc/resolver"));

        let none = resolver_files_check(routing(&[]));
        assert_eq!(none.verdict, Verdict::Fail);
        assert!(none.detail.contains(dns::ZONE) && none.detail.contains(dns::ZONE_RAW));
    }

    #[test]
    fn ca_trust_check_fails_on_a_missing_cert_without_probing_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let check = ca_trust_check(&tmp.path().join("ca.pem"), |_| {
            panic!("trust must not be probed for a cert that doesn't exist")
        });
        assert_eq!(check.verdict, Verdict::Fail);
        assert!(check.detail.contains("does not exist"));
    }

    #[test]
    fn ca_trust_verdict_follows_the_trust_store() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = tmp.path().join("ca.pem");
        std::fs::write(&cert, "not inspected").unwrap();

        assert_eq!(ca_trust_check(&cert, |_| None).verdict, Verdict::Warn);
        assert_eq!(ca_trust_check(&cert, |_| Some(true)).verdict, Verdict::Pass);
        let untrusted = ca_trust_check(&cert, |_| Some(false));
        assert_eq!(untrusted.verdict, Verdict::Fail);
        assert!(untrusted.hint.unwrap().contains("restart fghjd"));
    }

    #[tokio::test]
    async fn an_unreachable_docker_still_reports_both_of_its_checks() {
        let checks = docker_checks(&crate::docker::undialled_client()).await;
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["docker", "buildkit"]);
        assert_eq!(checks[0].verdict, Verdict::Fail);
        assert_eq!(checks[1].verdict, Verdict::Warn);
        assert!(checks.iter().all(|c| c.hint.is_some()));
    }

    #[test]
    fn client_checks_always_report_both_items() {
        let checks = client_checks();
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["fghjd", "cue"]);
        assert!(checks.iter().all(|c| !c.detail.is_empty()));
    }
}
