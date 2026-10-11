use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

use crate::daemon_log;

mod macos;

pub(crate) const ZONE: &str = "fghj.internal";
const ZONE_SUFFIX: &str = ".fghj.internal";

/// Answered per name with a `raw_net::resolve` virtual IP, never TLS.
pub(crate) const ZONE_RAW: &str = "fghj.raw.internal";

/// The host server's answer for `ZONE`; the proxy routes by name from there.
/// Only ever handed to clients on this host.
pub const ANSWER: Ipv4Addr = crate::web::proxy::PROXY_IP;

const ANSWER_TTL: u32 = 5;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

pub(crate) fn in_zone(qname: &str) -> bool {
    qname == ZONE || qname.ends_with(ZONE_SUFFIX)
}

pub(crate) fn matches_zone(qname: &str, zone: &str) -> bool {
    qname == zone || qname.ends_with(&format!(".{zone}"))
}

/// Implemented by the registry on the host and by `FileRoutes` in the sidecar.
pub trait ZoneSource: Send + Sync {
    fn answer_for(&self, qname: &str) -> Option<Ipv4Addr>;
}

/// IANA special-use TLDs (RFC 2606/6762), which can't collide with a real
/// domain. `web::ca` builds its `nameConstraints` from this list.
pub(crate) const RESERVED_ALIAS_TLDS: &[&str] = &["local", "test", "internal", "localhost"];

pub fn is_reserved_alias(host: &str) -> bool {
    RESERVED_ALIAS_TLDS
        .iter()
        .any(|tld| host == *tld || host.ends_with(&format!(".{tld}")))
}

/// In-zone names always; others only under a reserved TLD *and* routed, so a
/// browser never gets trust for a name nothing declared.
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
    /// Echoed verbatim: RFC 1035 preserves the question's case.
    question_bytes: Vec<u8>,
}

/// `None` for anything this minimal server doesn't handle; the caller drops it.
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

fn build_response(query: &Query, answer: Option<Ipv4Addr>) -> Vec<u8> {
    let zone_hit = answer.is_some();
    let answer_hit = zone_hit && query.qtype == TYPE_A && query.qclass == CLASS_IN;

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

/// Pure passthrough: the reply's id and question already match the client's.
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

/// Ephemeral, not 5353: Chrome and Bonjour routinely hold that. The resolver
/// config is written with whatever port we got.
pub async fn bind() -> Result<UdpSocket> {
    UdpSocket::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind DNS server on 127.0.0.1")
}

/// `zones` is consulted per query, so changes apply immediately. Unrecognized
/// names go to `upstream` (the sidecar) or get NXDOMAIN (the host).
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

/// Only macOS is implemented. Rerun as wildcard zones come and go.
pub fn install_os_resolver_config(port: u16, wildcard_zones: &[String]) -> Result<()> {
    if cfg!(target_os = "macos") {
        let mut zones: Vec<&str> = vec![ZONE, ZONE_RAW];
        zones.extend(wildcard_zones.iter().map(String::as_str));
        macos::sync_resolver(Path::new(macos::RESOLVER_DIR), port, &zones)
    } else {
        daemon_log::warn(format!(
            "fghjd: automatic OS DNS routing for *.{ZONE} isn't implemented on this platform yet — \
             point your resolver at 127.0.0.1:{port} for that zone manually"
        ));
        Ok(())
    }
}

/// Otherwise a stopped fghjd leaves the names pointed at a dead port.
pub fn clear_os_resolver_config() {
    if cfg!(target_os = "macos") {
        macos::clear_resolver(Path::new(macos::RESOLVER_DIR));
    }
}

/// Read back from the OS, so drift from the desired state shows up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRouting {
    pub location: &'static str,
    pub zones: Vec<(String, u16)>,
}

/// `None` where unsupported, so callers can tell it from "not routed".
pub fn os_routing_status() -> Option<OsRouting> {
    if cfg!(target_os = "macos") {
        Some(OsRouting {
            location: macos::RESOLVER_DIR,
            zones: macos::managed_resolver_zones(Path::new(macos::RESOLVER_DIR)),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _busy = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();

        let a = bind().await.unwrap();
        let b = bind().await.unwrap();
        assert_ne!(
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port()
        );
    }

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

    #[tokio::test]
    async fn serve_forwards_unrecognized_queries_to_upstream_verbatim() {
        let fake_upstream = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_addr = fake_upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (len, src) = fake_upstream.recv_from(&mut buf).await.unwrap();
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

        let resp = build_response(&query, None);
        let (_, flags1) = header_flags(&resp);
        assert_eq!(flags1 & 0x0F, 3, "RCODE must be NXDOMAIN once inactive");
    }

    #[test]
    fn os_routing_status_reports_the_platform_mechanism_or_none() {
        let status = os_routing_status();
        if cfg!(target_os = "macos") {
            let routing = status.expect("macOS has OS DNS routing");
            assert_eq!(routing.location, "/etc/resolver");
            assert!(routing.zones.is_sorted());
        } else {
            assert_eq!(status, None);
        }
    }
}
