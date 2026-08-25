use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

/// The zone this server is authoritative for. Every node's domain
/// (`runs::start_node`) is derived into this zone from its id, workspace,
/// and run id — never author-declared, so nothing outside this zone needs
/// answering.
pub(crate) const ZONE: &str = "fghj.internal";
const ZONE_SUFFIX: &str = ".fghj.internal";

/// Every workspace runs on one machine, so every `*.fghj.internal` name
/// resolves to the same place regardless of which service it names — the
/// TLS reverse proxy (`proxy.rs`, SPEC.md Subsystem C) is what routes by name
/// once it terminates on this address.
const ANSWER: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Short TTL: this is a dev-loop tool, not a public zone, so answers should
/// never be cached long enough to survive a `fghjd` restart onto a different
/// answer.
const ANSWER_TTL: u32 = 5;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// Whether `qname` (already lowercased) is the zone apex or a subdomain of
/// it. `pub(crate)` so `ca::DynamicCertResolver` can reuse the exact same
/// "is this name ours" rule rather than re-deriving it.
pub(crate) fn in_zone(qname: &str) -> bool {
    qname == ZONE || qname.ends_with(ZONE_SUFFIX)
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

    Some(Query { id, opcode, rd, qname, qtype, qclass, question_bytes })
}

/// Builds a response for `query`. Names inside the `fghj.internal` zone
/// always get an authoritative NOERROR — with an A answer of 127.0.0.1 if the
/// question was actually an `A`/`IN` lookup, or a bare NOERROR with zero
/// answers otherwise (the standard way to say "this name exists, just not
/// with a record of that type"). Anything outside the zone gets NXDOMAIN,
/// non-authoritatively — this server was never asked to speak for it.
fn build_response(query: &Query) -> Vec<u8> {
    let qname_lower = query.qname.to_ascii_lowercase();
    let zone_hit = in_zone(&qname_lower);
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
        resp.extend(ANSWER.octets());
    }

    resp
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
/// already have to handle no response as "try again or give up".
pub async fn serve(socket: UdpSocket) {
    let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    println!("fghjd: DNS server listening on 127.0.0.1:{port}, resolving *.{ZONE} to {ANSWER}");
    let mut buf = [0u8; 512]; // classic DNS-over-UDP message limit; plenty for single-question lookups
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fghjd: DNS recv error: {e}");
                continue;
            }
        };
        let Some(query) = parse_query(&buf[..len]) else {
            continue;
        };
        let response = build_response(&query);
        if let Err(e) = socket.send_to(&response, src).await {
            eprintln!("fghjd: DNS send error to {src}: {e}");
        }
    }
}

/// Routes the OS's resolution of `*.fghj.internal` to this server, per
/// SPEC.md §5 Subsystem B ("Native OS Integration"). Only macOS is wired up
/// today (`/etc/resolver`, the mechanism macOS's system resolver reads);
/// Linux (`systemd-resolved`) and Windows (NRPT) are called out in SPEC.md
/// but not implemented, so lookups there need a manual `/etc/hosts`-style
/// workaround until someone picks that up.
pub fn install_os_resolver_config(port: u16) -> Result<()> {
    if cfg!(target_os = "macos") {
        install_macos_resolver(Path::new("/etc/resolver"), port)
    } else {
        eprintln!(
            "fghjd: automatic OS DNS routing for *.{ZONE} isn't implemented on this platform yet — \
             point your resolver at 127.0.0.1:{port} for that zone manually"
        );
        Ok(())
    }
}

/// The file `install_macos_resolver` writes — exposed so `fghj daemon stop`
/// can remove it on a clean shutdown. Without this, a stopped `fghjd` leaves
/// `*.fghj.internal` routed at a now-dead port instead of failing over to
/// normal DNS, until the next `fghjd` start rewrites the file.
pub fn macos_resolver_path() -> std::path::PathBuf {
    Path::new("/etc/resolver").join(ZONE)
}

fn install_macos_resolver(resolver_dir: &Path, port: u16) -> Result<()> {
    fs::create_dir_all(resolver_dir)
        .with_context(|| format!("failed to create {}", resolver_dir.display()))?;
    let path = resolver_dir.join(ZONE);
    let desired = format!("nameserver 127.0.0.1\nport {port}\n");
    if fs::read_to_string(&path).ok().as_deref() != Some(desired.as_str()) {
        fs::write(&path, &desired).with_context(|| format!("failed to write {}", path.display()))?;
        println!("fghjd: wrote {} — *.{ZONE} lookups now route to this DNS server", path.display());
    }
    Ok(())
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

        let resp = build_response(&query);
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
        let resp = build_response(&query);
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);
    }

    #[test]
    fn non_a_query_in_zone_is_noerror_with_no_answer() {
        const TYPE_AAAA: u16 = 28;
        let raw = encode_query(2, "cart.fghj.internal", TYPE_AAAA);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query);
        let (_, flags1) = header_flags(&resp);
        assert_eq!(flags1 & 0x0F, 0, "name exists, so RCODE should still be NOERROR");
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 0, "no AAAA record exists for this zone");
    }

    #[test]
    fn query_outside_zone_is_nxdomain() {
        let raw = encode_query(3, "example.com", TYPE_A);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query);
        let (flags0, flags1) = header_flags(&resp);
        assert_eq!(flags0 & 0x04, 0, "must not claim authority outside our zone");
        assert_eq!(flags1 & 0x0F, 3, "RCODE must be NXDOMAIN");
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 0);
    }

    #[test]
    fn response_echoes_request_id_and_question() {
        let raw = encode_query(0xBEEF, "auth.fghj.internal", TYPE_A);
        let query = parse_query(&raw).unwrap();
        let resp = build_response(&query);
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0xBEEF);
        assert_eq!(&resp[12..12 + query.question_bytes.len()], &query.question_bytes[..]);
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
        assert_ne!(a.local_addr().unwrap().port(), b.local_addr().unwrap().port());
    }

    /// End-to-end check over a real loopback socket, exercising `serve`
    /// itself rather than just the pure `parse_query`/`build_response`
    /// functions it wraps.
    #[tokio::test]
    async fn serves_real_udp_queries_over_loopback() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(socket));

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let query = encode_query(0xABCD, "checkout.fghj.internal", TYPE_A);
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), client.recv_from(&mut buf))
                .await
                .expect("server should respond within 2s")
                .unwrap();
        let resp = &buf[..len];

        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0xABCD);
        let ancount = u16::from_be_bytes([resp[6], resp[7]]);
        assert_eq!(ancount, 1);
        assert_eq!(&resp[resp.len() - 4..], &ANSWER.octets());
    }

    #[test]
    fn install_macos_resolver_is_idempotent_and_writes_expected_content() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver_dir = tmp.path().join("resolver");

        install_macos_resolver(&resolver_dir, 54321).unwrap();
        let path = resolver_dir.join(ZONE);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "nameserver 127.0.0.1\nport 54321\n");

        // Re-running must not error and must leave the file as-is.
        install_macos_resolver(&resolver_dir, 54321).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
}
