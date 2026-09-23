use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

use crate::daemon_log;

/// The zone this server is authoritative for. Every node's domain
/// (`runs::start_node`) is derived into this zone from its id, workspace,
/// and run id — never author-declared, so nothing outside this zone needs
/// answering.
pub(crate) const ZONE: &str = "fghj.internal";
const ZONE_SUFFIX: &str = ".fghj.internal";

/// The raw/direct zone: reachable in-network via Docker's own embedded DNS
/// already (a node's `raw_domain` is a real Docker network alias — see
/// `runs.rs`), and — once a `raw_net` virtual IP is answered for it here —
/// reachable from the host too, NAT'd straight to the container's real
/// published port. Never TLS-terminated, never certified (`cert_eligible`
/// only matches `ZONE`), and never answered with the shared `ANSWER`
/// constant: every name under this zone gets its own distinct
/// `raw_net::resolve` address instead.
pub(crate) const ZONE_RAW: &str = "fghj.raw.internal";

/// Every workspace runs on one machine, so every `*.fghj.internal` name
/// resolves to the same place regardless of which service it names — the
/// TLS reverse proxy (`web::proxy`, SPEC.md Subsystem C) is what routes by name
/// once it terminates on this address. The host server always answers with
/// this for `ZONE`; the in-network sidecar (`fghj-sidecar.rs`) answers with
/// its own discovered IP instead; a `ZONE_RAW` name gets its own distinct
/// virtual IP instead of this constant at all — see `ZoneSource::answer_for`.
pub const ANSWER: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Short TTL: this is a dev-loop tool, not a public zone, so answers should
/// never be cached long enough to survive a `fghjd` restart onto a different
/// answer.
const ANSWER_TTL: u32 = 5;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// Whether `qname` (already lowercased) is the zone apex or a subdomain of
/// it. `pub(crate)` so `web::ca::DynamicCertResolver` can reuse the exact same
/// "is this name ours" rule rather than re-deriving it.
pub(crate) fn in_zone(qname: &str) -> bool {
    qname == ZONE || qname.ends_with(ZONE_SUFFIX)
}

/// Whether `qname` (already lowercased) is `zone` itself or a subdomain of
/// it — the same apex-or-subdomain rule `in_zone` hardcodes for the one
/// fixed `ZONE`, generalized so it can also be applied to a user-declared
/// `#Service.wildcard_hosts` suffix at query time.
pub(crate) fn matches_zone(qname: &str, zone: &str) -> bool {
    qname == zone || qname.ends_with(&format!(".{zone}"))
}

/// Whether a DNS server (`serve`) should claim authority for a query name,
/// and if so, which IP to answer with — the general "is this mine, and what
/// do I say" hook. Implemented by `WorkspaceRegistry` in `daemon.rs` (the
/// host server: `ANSWER` for `in_zone(qname)`/an active `wildcard_hosts`
/// suffix, a per-node `raw_net::resolve` address under `ZONE_RAW`,
/// or `None`) and by `FileRoutes` in `fghj-sidecar.rs` (the in-network
/// sidecar: its own discovered IP for anything already in its polled route
/// table, `None` otherwise) — one server, two entirely different notions of
/// "recognized" and "what to answer", both expressed the same way.
pub trait ZoneSource: Send + Sync {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr>;
}

/// IANA reserved special-use TLDs (RFC 2606 / 6762) — never delegated on the
/// real internet, so a hostname under one of these can't collide with a real
/// production domain. This is the eligibility gate for an `#AdditionalHost`
/// to get a certificate from fghj's local CA at all (see
/// `web::ca::DynamicCertResolver::resolve_for`); anything else is treated as
/// potentially real and only ever gets proxied over plain HTTP, never
/// certified.
const RESERVED_ALIAS_TLDS: &[&str] = &["local", "test", "internal", "localhost"];

pub fn is_reserved_alias(host: &str) -> bool {
    RESERVED_ALIAS_TLDS
        .iter()
        .any(|tld| host == *tld || host.ends_with(&format!(".{tld}")))
}

/// The one rule for whether `name` can ever get a certificate from fghj's
/// local CA: an in-zone `*.fghj.internal` name always can (that's the zone
/// this whole tool exists to serve), and anything else only can if it's
/// under a reserved-and-never-real TLD *and* actually routed to something —
/// never a blanket "any `.local`-shaped SNI gets a cert," which would let a
/// browser mint trust for a name nothing in this workspace declared. A real,
/// non-reserved-TLD hostname (e.g. a third-party OAuth callback host) is
/// never eligible, `routed` or not — see `web::ca::DynamicCertResolver::resolve_for`,
/// the one caller that actually mints certs off this, and `runs::start_node`,
/// which uses it to tell the UI whether a route can ever be linked as
/// `https://` at all.
pub(crate) fn cert_eligible(name: &str, routed: bool) -> bool {
    in_zone(name) || (is_reserved_alias(name) && routed)
}

struct Query {
    id: u16,
    opcode: u8,
    rd: bool,
    qname: String,
    qtype: u16,
    qclass: u16,
    /// The raw QNAME+QTYPE+QCLASS bytes as received, echoed back verbatim in
    /// the response's question section (RFC 1035 preserves the original
    /// case/encoding of the question).
    question_bytes: Vec<u8>,
}

/// Parses a single-question DNS query. Returns `None` for anything this
/// minimal server doesn't understand (multi-question messages, compressed
/// names in the question, truncated packets) — the caller drops those on the
/// floor rather than replying, same as a real server would for a malformed
/// request.
fn parse_query(buf: &[u8]) -> Option<Query> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let flags0 = buf[2];
    let opcode = (flags0 >> 3) & 0x0F;
    let rd = flags0 & 0x01 != 0;
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    if qdcount != 1 {
        return None;
    }

    let mut pos = 12usize;
    let mut labels = Vec::new();
    loop {
        let len = *buf.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression pointer in a question name: not expected from a real resolver
        }
        pos += 1;
        let label = buf.get(pos..pos + len)?;
        labels.push(String::from_utf8_lossy(label).to_string());
        pos += len;
    }
    let qname = labels.join(".");
    let qtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
    let qclass = u16::from_be_bytes([*buf.get(pos + 2)?, *buf.get(pos + 3)?]);
    let question_end = pos + 4;
    let question_bytes = buf.get(12..question_end)?.to_vec();

    Some(Query {
        id,
        opcode,
        rd,
        qname,
        qtype,
        qclass,
        question_bytes,
    })
}

/// Builds a response for `query`, given the `ZoneSource::answer_for` result
/// for its qname — `Some(ip)` always gets an authoritative NOERROR, with an
/// A answer of `ip` if the question was actually an `A`/`IN` lookup, or a
/// bare NOERROR with zero answers otherwise (the standard way to say "this
/// name exists, just not with a record of that type"). `None` gets
/// NXDOMAIN, non-authoritatively — this server was never asked to speak for
/// it.
fn build_response(query: &Query, answer: Option<Ipv4Addr>) -> Vec<u8> {
    let zone_hit = answer.is_some();
    let answer_hit = zone_hit && query.qtype == TYPE_A && query.qclass == CLASS_IN;

    // Opcode 0 is a standard query, the only kind this server answers.
    let rcode: u8 = if query.opcode != 0 {
        4 // NOTIMP
    } else if zone_hit {
        0 // NOERROR
    } else {
        3 // NXDOMAIN
    };

    let flags0 = 0x80 // QR: response
        | ((query.opcode & 0x0F) << 3)
        | if zone_hit { 0x04 } else { 0 } // AA: authoritative only for our own zone
        | if query.rd { 0x01 } else { 0 };
    let flags1 = rcode & 0x0F;

    let mut resp = Vec::with_capacity(12 + query.question_bytes.len() + 16);
    resp.extend(query.id.to_be_bytes());
    resp.push(flags0);
    resp.push(flags1);
    resp.extend(1u16.to_be_bytes()); // QDCOUNT
    resp.extend((if answer_hit { 1u16 } else { 0u16 }).to_be_bytes()); // ANCOUNT
    resp.extend(0u16.to_be_bytes()); // NSCOUNT
    resp.extend(0u16.to_be_bytes()); // ARCOUNT
    resp.extend(&query.question_bytes);

    if answer_hit {
        resp.extend([0xC0, 0x0C]); // NAME: pointer back to the question's QNAME at offset 12
        resp.extend(TYPE_A.to_be_bytes());
        resp.extend(CLASS_IN.to_be_bytes());
        resp.extend(ANSWER_TTL.to_be_bytes());
        resp.extend(4u16.to_be_bytes()); // RDLENGTH
        resp.extend(answer.unwrap().octets());
    }

    resp
}

/// Forwards a query this server doesn't recognize verbatim to `upstream`
/// (Docker's own embedded resolver, in the sidecar's case) over a fresh UDP
/// socket, and relays whatever comes back to `src` byte-for-byte — pure
/// passthrough, no re-parsing needed since the reply's transaction ID and
/// question section already match what the original client sent. Silently
/// drops the query if `upstream` doesn't answer within `FORWARD_TIMEOUT`,
/// same as any other DNS server would leave the client to retry or give up.
async fn forward_to_upstream(
    query_bytes: &[u8],
    upstream: SocketAddr,
    src: SocketAddr,
    socket: &UdpSocket,
) {
    const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);

    let Ok(upstream_socket) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return;
    };
    if upstream_socket
        .send_to(query_bytes, upstream)
        .await
        .is_err()
    {
        return;
    }
    let mut buf = [0u8; 512];
    let Ok(Ok((len, _))) =
        tokio::time::timeout(FORWARD_TIMEOUT, upstream_socket.recv_from(&mut buf)).await
    else {
        return;
    };
    let _ = socket.send_to(&buf[..len], src).await;
}

/// Binds the DNS listening socket on an OS-assigned ephemeral port, rather
/// than a fixed one (SPEC.md §5 suggests `5353`, mDNS's conventional port —
/// but that's exactly why it's a bad pick: `lsof -iUDP:5353` on a real dev
/// Mac routinely turns up Chrome/Brave/Bonjour already squatting on it for
/// device discovery). Nothing outside this process needs to know the port in
/// advance: `install_os_resolver_config` writes whatever port we actually got
/// into the OS resolver config itself, so there's nothing to collide with.
pub async fn bind() -> Result<UdpSocket> {
    UdpSocket::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind DNS server on 127.0.0.1")
}

/// Serves DNS queries on `socket` forever. Malformed packets (see
/// `parse_query`) are silently dropped rather than answered — UDP callers
/// already have to handle no response as "try again or give up". `zones` is
/// re-consulted fresh on every query (cheap at this server's query volume)
/// so a name starts/stops answering (or starts answering a different IP)
/// within one query of whatever backs `zones.answer_for` changing, with no
/// restart needed. A recognized name is always answered directly with
/// whatever IP `answer_for` returned for it; an unrecognized one is
/// forwarded verbatim to `upstream` if set (the sidecar's use case — Docker's
/// embedded resolver at `127.0.0.11:53`), or answered NXDOMAIN directly if
/// `upstream` is `None` (the host's use case — the OS never sends this
/// server an out-of-zone query to begin with, thanks to
/// `install_os_resolver_config`'s per-domain `/etc/resolver` scoping).
pub async fn serve(socket: UdpSocket, zones: Arc<dyn ZoneSource>, upstream: Option<SocketAddr>) {
    let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    daemon_log::info(format!("fghjd: DNS server listening on 127.0.0.1:{port}"));
    let mut buf = [0u8; 512]; // classic DNS-over-UDP message limit; plenty for single-question lookups
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                daemon_log::warn(format!("fghjd: DNS recv error: {e}"));
                continue;
            }
        };
        let Some(query) = parse_query(&buf[..len]) else {
            continue;
        };
        let qname_lower = query.qname.to_ascii_lowercase();
        let answer = zones.answer_for(&qname_lower);
        if answer.is_none()
            && let Some(upstream) = upstream
        {
            forward_to_upstream(&buf[..len], upstream, src, &socket).await;
            continue;
        }
        let response = build_response(&query, answer);
        if let Err(e) = socket.send_to(&response, src).await {
            daemon_log::warn(format!("fghjd: DNS send error to {src}: {e}"));
        }
    }
}

/// Routes the OS's resolution of `*.fghj.internal`, plus any currently-active
/// `wildcard_zones` (a running container's declared `wildcard_hosts`), to
/// this server, per SPEC.md §5 Subsystem B ("Native OS Integration"). Only
/// macOS is wired up today (`/etc/resolver`, the mechanism macOS's system
/// resolver reads); Linux (`systemd-resolved`) and Windows (NRPT) are called
/// out in SPEC.md but not implemented, so lookups there need a manual
/// `/etc/hosts`-style workaround until someone picks that up. Called both at
/// `activate` time and on every reconcile tick (see `daemon.rs`), since
/// unlike the one fixed `ZONE`, wildcard zones come and go with whichever
/// containers are currently running.
pub fn install_os_resolver_config(port: u16, wildcard_zones: &[String]) -> Result<()> {
    if cfg!(target_os = "macos") {
        let mut zones: Vec<&str> = vec![ZONE, ZONE_RAW];
        zones.extend(wildcard_zones.iter().map(String::as_str));
        sync_macos_resolver(Path::new("/etc/resolver"), port, &zones)
    } else {
        daemon_log::warn(format!(
            "fghjd: automatic OS DNS routing for *.{ZONE} isn't implemented on this platform yet — \
             point your resolver at 127.0.0.1:{port} for that zone manually"
        ));
        Ok(())
    }
}

/// Reverses `install_os_resolver_config`: removes every fghjd-authored
/// resolver file (the fixed `ZONE` plus any wildcard zone) on a clean
/// shutdown. Without this, a stopped `fghjd` leaves those names routed at a
/// now-dead port instead of failing over to normal DNS, until the next
/// `fghjd` start rewrites them.
pub fn clear_os_resolver_config() {
    if cfg!(target_os = "macos") {
        clear_macos_resolver(Path::new("/etc/resolver"));
    }
}

/// Parses the port back out of a resolver file's content if it matches
/// fghjd's own template — the single source of truth for "is this a file
/// fghjd itself wrote, and if so at what port," used both to recognize
/// fghjd-authored files (`is_fghjd_resolver_content`) and to read the port
/// back for the telemetry status endpoint (`managed_resolver_zones`).
fn parse_fghjd_resolver_port(content: &str) -> Option<u16> {
    content
        .strip_prefix("nameserver 127.0.0.1\nport ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .and_then(|port| port.parse::<u16>().ok())
}

/// Recognizing exactly this shape (regardless of port) is how
/// `sync_macos_resolver`/`clear_macos_resolver` tell "a file fghjd itself
/// created" apart from a resolver file some other tool placed, without
/// needing a separate tracking manifest.
fn is_fghjd_resolver_content(content: &str) -> bool {
    parse_fghjd_resolver_port(content).is_some()
}

/// Reads back which zones are currently routed to this DNS server and at
/// what port, by scanning `resolver_dir` for fghjd-authored files
/// (`parse_fghjd_resolver_port`) — the on-disk state `sync_macos_resolver`
/// last wrote is the source of truth, so this re-parses it rather than
/// tracking a separate list. Backs the telemetry drawer's network-status tab
/// (`daemon.rs`'s `/daemon/net-status`).
pub fn managed_resolver_zones(resolver_dir: &Path) -> Vec<(String, u16)> {
    let Ok(entries) = fs::read_dir(resolver_dir) else {
        return Vec::new();
    };
    let mut zones: Vec<(String, u16)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            let content = fs::read_to_string(&path).ok()?;
            let port = parse_fghjd_resolver_port(&content)?;
            Some((name, port))
        })
        .collect();
    zones.sort();
    zones
}

/// Writes (or refreshes) one `resolver_dir/<zone>` file per entry in `zones`
/// — idempotently, same as before — then removes any *other* file in that
/// directory whose content matches fghjd's own template
/// (`is_fghjd_resolver_content`) but whose zone isn't in `zones` anymore,
/// e.g. a `wildcard_hosts` suffix whose owning container just stopped. A
/// resolver file some other tool created is never touched, since its
/// content won't match the template.
fn sync_macos_resolver(resolver_dir: &Path, port: u16, zones: &[&str]) -> Result<()> {
    fs::create_dir_all(resolver_dir)
        .with_context(|| format!("failed to create {}", resolver_dir.display()))?;
    let desired = format!("nameserver 127.0.0.1\nport {port}\n");

    for zone in zones {
        let path = resolver_dir.join(zone);
        if fs::read_to_string(&path).ok().as_deref() != Some(desired.as_str()) {
            fs::write(&path, &desired)
                .with_context(|| format!("failed to write {}", path.display()))?;
            daemon_log::info(format!(
                "fghjd: wrote {} — *.{zone} lookups now route to this DNS server",
                path.display()
            ));
        }
    }

    if let Ok(entries) = fs::read_dir(resolver_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if zones.contains(&name) {
                continue;
            }
            if fs::read_to_string(&path)
                .ok()
                .is_some_and(|c| is_fghjd_resolver_content(&c))
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// Removes every fghjd-authored resolver file in `resolver_dir`
/// unconditionally (content-based, same rule as `sync_macos_resolver`) —
/// the full-teardown counterpart used on `deactivate`.
fn clear_macos_resolver(resolver_dir: &Path) {
    let Ok(entries) = fs::read_dir(resolver_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::read_to_string(&path)
            .ok()
            .is_some_and(|c| is_fghjd_resolver_content(&c))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-encodes a single-question DNS query, mirroring what a real
    /// resolver sends, so `parse_query`/`build_response` can be tested
    /// end-to-end without a socket.
    fn encode_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(id.to_be_bytes());
        buf.push(0x01); // flags0: RD=1
        buf.push(0x00); // flags1
        buf.extend(1u16.to_be_bytes()); // QDCOUNT
        buf.extend(0u16.to_be_bytes());
        buf.extend(0u16.to_be_bytes());
        buf.extend(0u16.to_be_bytes());
        for label in name.split('.') {
            buf.push(label.len() as u8);
            buf.extend(label.as_bytes());
        }
        buf.push(0);
        buf.extend(qtype.to_be_bytes());
        buf.extend(CLASS_IN.to_be_bytes());
        buf
    }

    fn header_flags(resp: &[u8]) -> (u8, u8) {
        (resp[2], resp[3])
    }

    #[test]
    fn is_reserved_alias_matches_special_use_tlds_only() {
        assert!(is_reserved_alias("aikido.local"));
        assert!(is_reserved_alias("deep.sub.aikido.local"));
        assert!(is_reserved_alias("demo.test"));
        assert!(is_reserved_alias("demo.internal"));
        assert!(is_reserved_alias("demo.localhost"));
        assert!(
            !is_reserved_alias("app.local.aikido.io"),
            "the TLD is .io, not .local — the last label is what counts"
        );
        assert!(!is_reserved_alias("aikido.io"));
        assert!(!is_reserved_alias("example.com"));
    }

    #[test]
    fn in_zone_matches_apex_and_subdomains_only() {
        assert!(in_zone("fghj.internal"));
        assert!(in_zone("cart.fghj.internal"));
        assert!(in_zone("deep.sub.cart.fghj.internal"));
        assert!(!in_zone("fghj.internal.evil.com"));
        assert!(!in_zone("notfghj.internal"));
        assert!(!in_zone("example.com"));
    }

    #[test]
    fn a_query_in_zone_resolves_to_localhost() {
        let raw = encode_query(0x1234, "cart.fghj.internal", TYPE_A);
        let query = parse_query(&raw).expect("valid query parses");
        assert_eq!(query.qname, "cart.fghj.internal");

        let resp = build_response(&query, Some(ANSWER));
        let (flags0, flags1) = header_flags(&resp);
        assert_eq!(flags0 & 0x80, 0x80, "QR bit must be set on a response");
        assert_eq!(flags0 & 0x04, 0x04, "AA bit must be set for our own zone");
        assert_eq!(flags1 & 0x0F, 0, "RCODE must be NOERROR");
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);
        assert_eq!(&resp[resp.len() - 4..], &ANSWER.octets());
    }

    #[test]
    fn apex_domain_also_resolves() {
        let raw = encode_query(1, "fghj.internal", TYPE_A);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query, Some(ANSWER));
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);
    }

    #[test]
    fn non_a_query_in_zone_is_noerror_with_no_answer() {
        const TYPE_AAAA: u16 = 28;
        let raw = encode_query(2, "cart.fghj.internal", TYPE_AAAA);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query, Some(ANSWER));
        let (_, flags1) = header_flags(&resp);
        assert_eq!(
            flags1 & 0x0F,
            0,
            "name exists, so RCODE should still be NOERROR"
        );
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 0, "no AAAA record exists for this zone");
    }

    #[test]
    fn query_outside_zone_is_nxdomain() {
        let raw = encode_query(3, "example.com", TYPE_A);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query, None);
        let (flags0, flags1) = header_flags(&resp);
        assert_eq!(
            flags0 & 0x04,
            0,
            "must not claim authority outside our zone"
        );
        assert_eq!(flags1 & 0x0F, 3, "RCODE must be NXDOMAIN");
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 0);
    }

    #[test]
    fn response_echoes_request_id_and_question() {
        let raw = encode_query(0xBEEF, "auth.fghj.internal", TYPE_A);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query, Some(ANSWER));
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0xBEEF);
        assert_eq!(
            &resp[12..12 + query.question_bytes.len()],
            &query.question_bytes[..]
        );
    }

    #[test]
    fn multi_question_packets_are_rejected() {
        let mut raw = encode_query(4, "cart.fghj.internal", TYPE_A);
        raw[4] = 0;
        raw[5] = 2; // claim QDCOUNT=2 without a second question actually present
        assert!(parse_query(&raw).is_none());
    }

    #[tokio::test]
    async fn bind_uses_an_ephemeral_port_and_never_collides() {
        // Simulates something else already squatting on a fixed port (e.g.
        // mDNSResponder/Chrome/Brave routinely do on 5353) — `bind` must
        // never fail here, since it doesn't ask for any specific port.
        let _busy = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();

        let a = bind().await.unwrap();
        let b = bind().await.unwrap();
        assert_ne!(
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port()
        );
    }

    /// End-to-end check over a real loopback socket, exercising `serve`
    /// itself rather than just the pure `parse_query`/`build_response`
    /// functions it wraps. Carries its own `answer_ip` (rather than always
    /// `ANSWER`) so the same helper can stand in for either the host server
    /// or a sidecar-style server answering its own discovered IP.
    struct StaticZones {
        answer_ip: Ipv4Addr,
        wildcard_zones: Vec<String>,
    }

    impl ZoneSource for StaticZones {
        fn answer_for(&self, qname: &str) -> Option<Ipv4Addr> {
            if in_zone(qname) || self.wildcard_zones.iter().any(|z| matches_zone(qname, z)) {
                Some(self.answer_ip)
            } else {
                None
            }
        }
    }

    fn no_zones() -> Arc<dyn ZoneSource> {
        Arc::new(StaticZones {
            answer_ip: ANSWER,
            wildcard_zones: Vec::new(),
        })
    }

    #[tokio::test]
    async fn serves_real_udp_queries_over_loopback() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(socket, no_zones(), None));

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let query = encode_query(0xABCD, "checkout.fghj.internal", TYPE_A);
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("server should respond within 2s")
        .unwrap();
        let resp = &buf[..len];

        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0xABCD);
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);
        assert_eq!(&resp[resp.len() - 4..], &ANSWER.octets());
    }

    /// A sidecar-style server answers with its own discovered IP, not always
    /// `127.0.0.1` — whatever `ZoneSource::answer_for` returns must actually
    /// be threaded through, not hardcoded.
    #[tokio::test]
    async fn serve_answers_with_the_configured_answer_ip_not_always_localhost() {
        let sidecar_ip = Ipv4Addr::new(172, 20, 0, 5);
        let zones: Arc<dyn ZoneSource> = Arc::new(StaticZones {
            answer_ip: sidecar_ip,
            wildcard_zones: Vec::new(),
        });
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(socket, zones, None));

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let query = encode_query(0x1111, "checkout.fghj.internal", TYPE_A);
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("server should respond within 2s")
        .unwrap();
        let resp = &buf[..len];
        assert_eq!(&resp[resp.len() - 4..], &sidecar_ip.octets());
    }

    /// The sidecar's forwarding behavior: a query outside `zones` gets
    /// relayed byte-for-byte to `upstream`, and whatever `upstream` answers
    /// gets relayed back to the original client verbatim.
    #[tokio::test]
    async fn serve_forwards_unrecognized_queries_to_upstream_verbatim() {
        let fake_upstream = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_addr = fake_upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (len, src) = fake_upstream.recv_from(&mut buf).await.unwrap();
            // A canned "reply" — the point is only that these exact bytes
            // come back to the original client unmodified, not that this is
            // a well-formed DNS message.
            let canned_reply = [buf[..len].to_vec(), vec![0xDE, 0xAD, 0xBE, 0xEF]].concat();
            fake_upstream.send_to(&canned_reply, src).await.unwrap();
        });

        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(socket, no_zones(), Some(upstream_addr)));

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let query = encode_query(0x2222, "some-plain-docker-service", TYPE_A);
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("forwarded reply should arrive within 2s")
        .unwrap();
        let resp = &buf[..len];
        assert_eq!(
            &resp[..query.len()],
            &query[..],
            "original query echoed back by the canned upstream reply"
        );
        assert_eq!(&resp[resp.len() - 4..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn a_query_under_an_active_wildcard_zone_resolves_to_localhost() {
        let raw = encode_query(5, "acme.myservice.local", TYPE_A);
        let query = parse_query(&raw).unwrap();

        let resp = build_response(&query, Some(ANSWER));
        let (flags0, flags1) = header_flags(&resp);
        assert_eq!(flags0 & 0x04, 0x04, "AA bit must be set for an active zone");
        assert_eq!(flags1 & 0x0F, 0, "RCODE must be NOERROR");
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);

        // A name under a zone that isn't currently active must still be
        // NXDOMAIN — the set is consulted fresh per query, not cached.
        let resp = build_response(&query, None);
        let (_, flags1) = header_flags(&resp);
        assert_eq!(flags1 & 0x0F, 3, "RCODE must be NXDOMAIN once inactive");
    }

    #[test]
    fn sync_macos_resolver_is_idempotent_and_writes_expected_content() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_macos_resolver(&resolver_dir, 54321, &[ZONE]).unwrap();
        let path = resolver_dir.join(ZONE);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "nameserver 127.0.0.1\nport 54321\n");

        // Re-running must not error and must leave the file as-is.
        sync_macos_resolver(&resolver_dir, 54321, &[ZONE]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn sync_macos_resolver_writes_multiple_zones_and_prunes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_macos_resolver(&resolver_dir, 1234, &[ZONE, "myservice.local"]).unwrap();
        assert!(resolver_dir.join(ZONE).exists());
        assert!(resolver_dir.join("myservice.local").exists());

        // A foreign file (content some other tool wrote) must survive.
        let foreign = resolver_dir.join("example.com");
        fs::write(&foreign, "nameserver 8.8.8.8\n").unwrap();

        // The wildcard zone's owning container stopped — it drops out of
        // the wanted set and its file must be removed, but the foreign
        // file and the fixed zone's file must be untouched.
        sync_macos_resolver(&resolver_dir, 1234, &[ZONE]).unwrap();
        assert!(resolver_dir.join(ZONE).exists());
        assert!(!resolver_dir.join("myservice.local").exists());
        assert_eq!(
            fs::read_to_string(&foreign).unwrap(),
            "nameserver 8.8.8.8\n"
        );
    }

    #[test]
    fn clear_macos_resolver_removes_only_fghjd_authored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_macos_resolver(&resolver_dir, 1234, &[ZONE, "myservice.local"]).unwrap();
        let foreign = resolver_dir.join("example.com");
        fs::write(&foreign, "nameserver 8.8.8.8\n").unwrap();

        clear_macos_resolver(&resolver_dir);

        assert!(!resolver_dir.join(ZONE).exists());
        assert!(!resolver_dir.join("myservice.local").exists());
        assert!(foreign.exists());
    }

    #[test]
    fn managed_resolver_zones_reads_back_fghjd_authored_zones_only() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        sync_macos_resolver(&resolver_dir, 5353, &[ZONE, "myservice.local"]).unwrap();
        fs::write(resolver_dir.join("example.com"), "nameserver 8.8.8.8\n").unwrap();

        let mut zones = managed_resolver_zones(&resolver_dir);
        zones.sort();
        let mut expected = vec![
            (ZONE.to_string(), 5353),
            ("myservice.local".to_string(), 5353),
        ];
        expected.sort();
        assert_eq!(zones, expected);
    }

    #[test]
    fn managed_resolver_zones_of_a_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(managed_resolver_zones(&tmp.path().join("nonexistent")).is_empty());
    }
}
