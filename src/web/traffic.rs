//! HTTP traffic capture for the reverse proxy, per node and in memory only.
//!
//! Recording is switched on per node and stays on until it is switched off
//! or fghjd restarts; nothing here touches the disk. Bodies are kept as they
//! went over the wire and decoded only when someone opens them. Which bodies
//! are kept at all is decided by content type, see [`classify`].

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use hyper::header::{self, HeaderMap};
use hyper::{Request, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast};

/// The ring per node: whichever limit is hit first evicts the oldest.
pub const MAX_EXCHANGES: usize = 1000;
pub const NODE_BUDGET: usize = 64 << 20;
/// Across every node at once, so a forgotten recording can't eat the daemon.
pub const GLOBAL_BUDGET: usize = 128 << 20;
/// Per body, as stored. The wire size is still counted past it.
pub const BODY_CAP: usize = 8 << 20;
/// A body compressed under [`BODY_CAP`] may inflate far past it.
const DECODED_CAP: u64 = 32 << 20;

/// Which bodies a recording keeps. Headers are always kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyMode {
    /// Headers and sizes only.
    None,
    /// Text-like bodies; binary ones are recorded by size and type.
    Text,
    /// Everything, up to [`BODY_CAP`].
    All,
}

/// Every node's log, keyed by workspace id and node id. Logs are created on
/// first use and live as long as fghjd does.
pub struct TrafficStore {
    logs: Mutex<HashMap<(String, String), Arc<TrafficLog>>>,
    total: Arc<AtomicUsize>,
    /// Every change in every log, for the sidecar's capture stream. Nobody
    /// subscribes in fghjd, and a send with no receivers costs nothing.
    feed: broadcast::Sender<Changed>,
    modes_changed: Arc<Notify>,
}

impl Default for TrafficStore {
    fn default() -> Self {
        Self {
            logs: Mutex::default(),
            total: Arc::default(),
            feed: broadcast::channel(1024).0,
            modes_changed: Arc::default(),
        }
    }
}

/// One exchange that changed, and the node it was recorded for.
#[derive(Clone)]
pub struct Changed {
    pub node: Arc<str>,
    pub ex: Arc<Exchange>,
}

impl TrafficStore {
    pub fn log(&self, workspace: &str, node_id: &str) -> Arc<TrafficLog> {
        let key = (workspace.to_string(), node_id.to_string());
        self.logs
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| {
                Arc::new(TrafficLog::new(
                    self.total.clone(),
                    node_id.into(),
                    self.feed.clone(),
                    self.modes_changed.clone(),
                ))
            })
            .clone()
    }

    /// Wakes on any node's mode changing.
    pub fn modes_changed(&self) -> Arc<Notify> {
        self.modes_changed.clone()
    }

    pub fn subscribe_all(&self) -> broadcast::Receiver<Changed> {
        self.feed.subscribe()
    }

    /// Everything in every ring, for a subscriber that just joined or lagged.
    pub fn snapshot_all(&self) -> Vec<Changed> {
        let logs: Vec<_> = self.logs.lock().unwrap().values().cloned().collect();
        logs.iter()
            .flat_map(|log| {
                let entries: Vec<_> = log.ring.lock().unwrap().entries.iter().cloned().collect();
                entries.into_iter().map(|ex| Changed {
                    node: log.node.clone(),
                    ex,
                })
            })
            .collect()
    }

    /// The sidecar's recording set, as fghjd last sent it: the nodes listed
    /// record, every other one stops and is cleared.
    pub fn apply_modes(&self, workspace: &str, modes: &HashMap<String, BodyMode>) {
        for (node, mode) in modes {
            let log = self.log(workspace, node);
            if log.mode() != Some(*mode) {
                log.set_mode(Some(*mode));
            }
        }
        let others: Vec<_> = self
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|((ws, node), _)| ws == workspace && !modes.contains_key(node))
            .map(|(_, log)| log.clone())
            .collect();
        for log in others {
            if log.mode().is_some() {
                log.set_mode(None);
            }
            log.clear();
        }
    }

    /// Node id to mode, for every node recording in `workspace`.
    pub fn recording(&self, workspace: &str) -> HashMap<String, BodyMode> {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .filter(|((ws, _), _)| ws == workspace)
            .filter_map(|((_, node), log)| log.mode().map(|m| (node.clone(), m)))
            .collect()
    }
}

#[derive(Clone)]
pub enum Update {
    Upsert(Box<Summary>),
    Mode(Option<BodyMode>),
    Cleared,
}

pub struct TrafficLog {
    node: Arc<str>,
    mode: Mutex<Option<BodyMode>>,
    ring: Mutex<Ring>,
    next_id: AtomicU64,
    /// So a shipped exchange from before a clear doesn't come back.
    cleared_at_ms: AtomicU64,
    updates: broadcast::Sender<Update>,
    feed: broadcast::Sender<Changed>,
    modes_changed: Arc<Notify>,
    total: Arc<AtomicUsize>,
}

#[derive(Default)]
struct Ring {
    entries: VecDeque<Arc<Exchange>>,
    bytes: usize,
}

impl TrafficLog {
    fn new(
        total: Arc<AtomicUsize>,
        node: Arc<str>,
        feed: broadcast::Sender<Changed>,
        modes_changed: Arc<Notify>,
    ) -> Self {
        Self {
            node,
            mode: Mutex::new(None),
            ring: Mutex::default(),
            next_id: AtomicU64::new(1),
            cleared_at_ms: AtomicU64::new(0),
            updates: broadcast::channel(1024).0,
            feed,
            modes_changed,
            total,
        }
    }

    pub fn mode(&self) -> Option<BodyMode> {
        *self.mode.lock().unwrap()
    }

    /// `None` stops recording. What was captured stays until cleared.
    pub fn set_mode(&self, mode: Option<BodyMode>) {
        *self.mode.lock().unwrap() = mode;
        let _ = self.updates.send(Update::Mode(mode));
        self.modes_changed.notify_one();
    }

    pub fn clear(&self) {
        let mut ring = self.ring.lock().unwrap();
        for ex in ring.entries.drain(..) {
            ex.data.lock().unwrap().in_ring = false;
        }
        self.total.fetch_sub(ring.bytes, Ordering::Relaxed);
        ring.bytes = 0;
        self.cleared_at_ms.store(now_ms(), Ordering::Relaxed);
        drop(ring);
        let _ = self.updates.send(Update::Cleared);
    }

    /// Subscribe before taking the snapshot: an exchange that changes in
    /// between arrives twice, and an upsert is idempotent.
    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    pub fn summaries(&self) -> Vec<Summary> {
        let entries: Vec<_> = self.ring.lock().unwrap().entries.iter().cloned().collect();
        entries.iter().map(|ex| ex.summary()).collect()
    }

    pub fn get(&self, id: u64) -> Option<Arc<Exchange>> {
        let ring = self.ring.lock().unwrap();
        ring.entries.iter().find(|ex| ex.id == id).cloned()
    }

    /// Starts recording one request, or `None` when this node isn't recording.
    pub fn begin<B>(
        self: &Arc<Self>,
        req: &Request<B>,
        source: String,
        scheme: &'static str,
    ) -> Option<Recording> {
        let mode = self.mode()?;
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| req.uri().host())
            .unwrap_or("")
            .to_string();
        let request_headers = header_list(req.headers());
        let data = Data {
            started_at_ms: now_ms(),
            started: Instant::now(),
            mode,
            method: req.method().to_string(),
            url: req
                .uri()
                .path_and_query()
                .map_or("/", |p| p.as_str())
                .to_string(),
            host,
            scheme: scheme.to_string(),
            source,
            accounted: headers_weight(&request_headers),
            request_headers,
            response_headers: Vec::new(),
            status: None,
            ttfb_ms: None,
            duration_ms: None,
            error: None,
            upgraded: false,
            request: Captured::new(mode, req.headers()),
            response: Captured::pending(),
            in_ring: true,
        };
        let ex = self.push(data, None);
        let rec = Recording {
            log: self.clone(),
            ex,
        };
        rec.publish();
        Some(rec)
    }

    fn push(&self, data: Data, remote: Option<(u64, u64)>) -> Arc<Exchange> {
        let ex = Arc::new(Exchange {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            remote,
            data: Mutex::new(data),
        });
        let mut ring = self.ring.lock().unwrap();
        let weight = ex.data.lock().unwrap().accounted;
        ring.bytes += weight;
        self.total.fetch_add(weight, Ordering::Relaxed);
        ring.entries.push_back(ex.clone());
        self.evict(&mut ring);
        ex
    }

    fn publish(&self, ex: &Arc<Exchange>) {
        let _ = self.updates.send(Update::Upsert(Box::new(ex.summary())));
        if self.feed.receiver_count() > 0 {
            let _ = self.feed.send(Changed {
                node: self.node.clone(),
                ex: ex.clone(),
            });
        }
    }

    /// Adds or updates an exchange another proxy recorded, keyed by that
    /// proxy's `origin` and its own id. Dropped while this node isn't
    /// recording, and when it started before the last clear.
    pub fn ingest(&self, origin: u64, shipped: Shipped) {
        if self.mode().is_none() {
            return;
        }
        let key = (origin, shipped.id);
        let mut data = shipped.data;
        data.in_ring = true;
        let mut ring = self.ring.lock().unwrap();
        let known = ring
            .entries
            .iter()
            .rev()
            .find(|ex| ex.remote == Some(key))
            .cloned();
        let ex = match known {
            Some(ex) => {
                {
                    let mut d = ex.data.lock().unwrap();
                    data.accounted = data.weight();
                    ring.bytes = ring.bytes + data.accounted - d.accounted;
                    self.total.fetch_add(data.accounted, Ordering::Relaxed);
                    self.total.fetch_sub(d.accounted, Ordering::Relaxed);
                    *d = data;
                }
                self.evict(&mut ring);
                ex
            }
            None if data.started_at_ms < self.cleared_at_ms.load(Ordering::Relaxed) => return,
            None => {
                drop(ring);
                data.accounted = data.weight();
                self.push(data, Some(key))
            }
        };
        self.publish(&ex);
    }

    /// Never evicts the newest entry, so a single huge exchange is still seen.
    fn evict(&self, ring: &mut Ring) {
        while ring.entries.len() > 1
            && (ring.entries.len() > MAX_EXCHANGES
                || ring.bytes > NODE_BUDGET
                || self.total.load(Ordering::Relaxed) > GLOBAL_BUDGET)
        {
            let Some(old) = ring.entries.pop_front() else {
                break;
            };
            let mut data = old.data.lock().unwrap();
            data.in_ring = false;
            ring.bytes -= data.accounted;
            self.total.fetch_sub(data.accounted, Ordering::Relaxed);
        }
    }

    /// Brings the ring's byte count in line with what `ex` now holds.
    fn account(&self, ex: &Exchange) {
        let mut ring = self.ring.lock().unwrap();
        {
            let mut data = ex.data.lock().unwrap();
            if !data.in_ring {
                return;
            }
            let weight = data.weight();
            ring.bytes = ring.bytes + weight - data.accounted;
            self.total.fetch_add(weight, Ordering::Relaxed);
            self.total.fetch_sub(data.accounted, Ordering::Relaxed);
            data.accounted = weight;
        }
        self.evict(&mut ring);
    }
}

pub struct Exchange {
    pub id: u64,
    /// For one another proxy shipped: its origin and its id there.
    remote: Option<(u64, u64)>,
    data: Mutex<Data>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Everything recorded about one exchange. Serialized whole when the sidecar
/// ships it to fghjd, bodies included.
#[derive(Serialize, Deserialize)]
struct Data {
    started_at_ms: u64,
    /// Only measures a recording in progress, which is never a shipped one.
    #[serde(skip, default = "Instant::now")]
    started: Instant,
    mode: BodyMode,
    method: String,
    url: String,
    host: String,
    scheme: String,
    source: String,
    request_headers: Vec<(String, String)>,
    response_headers: Vec<(String, String)>,
    status: Option<u16>,
    ttfb_ms: Option<f64>,
    duration_ms: Option<f64>,
    error: Option<String>,
    upgraded: bool,
    request: Captured,
    response: Captured,
    /// Bytes this exchange is counted as in its ring.
    #[serde(skip)]
    accounted: usize,
    /// False once evicted or cleared, so late body bytes aren't counted.
    #[serde(skip)]
    in_ring: bool,
}

/// One exchange as the sidecar ships it to fghjd, a line of JSON each.
#[derive(Serialize, Deserialize)]
pub struct Shipped {
    pub node: String,
    pub id: u64,
    data: Data,
}

impl Shipped {
    /// Where the request came from: an IP, or `host`.
    pub fn source(&self) -> &str {
        &self.data.source
    }

    pub fn set_source(&mut self, source: String) {
        self.data.source = source;
    }
}

/// The borrowed side of [`Shipped`], so shipping doesn't copy the bodies.
#[derive(Serialize)]
struct ShippedRef<'a> {
    node: &'a str,
    id: u64,
    data: &'a Data,
}

impl Data {
    fn weight(&self) -> usize {
        headers_weight(&self.request_headers)
            + headers_weight(&self.response_headers)
            + self.request.data.len()
            + self.response.data.len()
            + self.request.parts.iter().map(Part::weight).sum::<usize>()
            + self.response.parts.iter().map(Part::weight).sum::<usize>()
    }

    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    fn state(&self) -> &'static str {
        if self.error.is_some() {
            "error"
        } else if self.status.is_none() {
            "pending"
        } else if self.upgraded {
            if self.duration_ms.is_some() {
                "closed"
            } else {
                "open"
            }
        } else {
            match self.response.state {
                BodyState::Done => "done",
                BodyState::Aborted => "aborted",
                BodyState::Error => "error",
                BodyState::Pending | BodyState::Streaming => "streaming",
            }
        }
    }
}

fn header_list(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn headers_weight(headers: &[(String, String)]) -> usize {
    headers.iter().map(|(k, v)| k.len() + v.len() + 4).sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Keep {
    Full,
    /// No content type to go by: kept, then dropped at the end unless it
    /// turned out to be text.
    Sniff,
    Drop(Omitted),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Omitted {
    HeadersOnly,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BodyState {
    Pending,
    Streaming,
    Done,
    Aborted,
    Error,
}

#[derive(Serialize, Deserialize)]
struct Captured {
    keep: Keep,
    #[serde(with = "base64_bytes")]
    data: Vec<u8>,
    /// On the wire, past the cap too.
    size: u64,
    truncated: bool,
    state: BodyState,
    content_type: Option<String>,
    content_encoding: Option<String>,
    /// A multipart body in text mode: its text fields and the shape of its files.
    parts: Vec<Part>,
}

impl Captured {
    fn new(mode: BodyMode, headers: &HeaderMap) -> Self {
        let content_type = header_str(headers, header::CONTENT_TYPE);
        let content_encoding = header_str(headers, header::CONTENT_ENCODING)
            .filter(|e| !e.eq_ignore_ascii_case("identity"));
        Self {
            keep: classify(mode, content_type.as_deref()),
            data: Vec::new(),
            size: 0,
            truncated: false,
            state: BodyState::Pending,
            content_type,
            content_encoding,
            parts: Vec::new(),
        }
    }

    fn pending() -> Self {
        Self::new(BodyMode::None, &HeaderMap::new())
    }

    fn push(&mut self, chunk: &[u8]) {
        self.state = BodyState::Streaming;
        self.size += chunk.len() as u64;
        if matches!(self.keep, Keep::Drop(_)) {
            return;
        }
        let room = BODY_CAP.saturating_sub(self.data.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.data.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    fn finish(&mut self, state: BodyState, mode: BodyMode) {
        self.state = state;
        if self.keep == Keep::Sniff && !looks_like_text(&self.decoded().bytes) {
            self.keep = Keep::Drop(Omitted::Binary);
            self.data = Vec::new();
        }
        if mode == BodyMode::Text
            && self.content_encoding.is_none()
            && let Some(boundary) = self.content_type.as_deref().and_then(multipart_boundary)
        {
            self.parts = parse_multipart(&self.data, &boundary, false);
            if !self.parts.is_empty() {
                self.data = Vec::new();
            }
        }
    }

    fn decoded(&self) -> Decoded {
        decode(&self.data, self.content_encoding.as_deref())
    }

    fn view(&self) -> BodyView {
        let omitted = match self.keep {
            Keep::Drop(why) => Some(why),
            _ => None,
        };
        let mut view = BodyView {
            size: self.size,
            content_type: self.content_type.clone(),
            content_encoding: self.content_encoding.clone(),
            state: self.state,
            truncated: self.truncated,
            omitted,
            text: None,
            binary: false,
            decode_error: None,
            parts: (!self.parts.is_empty()).then(|| self.parts.clone()),
        };
        if omitted.is_some() || self.data.is_empty() {
            return view;
        }
        let decoded = self.decoded();
        view.decode_error = decoded.error;
        if let Some(boundary) = self.content_type.as_deref().and_then(multipart_boundary) {
            let parts = parse_multipart(&decoded.bytes, &boundary, true);
            if !parts.is_empty() {
                view.parts = Some(parts);
                return view;
            }
        }
        match text_of(&decoded.bytes) {
            Some(text) => view.text = Some(text),
            None => view.binary = true,
        }
        view
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// The bare `type/subtype`, lowercased.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// No editable list on purpose: the split between "read it" and "it's a
/// blob" is the same for every app.
fn classify(mode: BodyMode, content_type: Option<&str>) -> Keep {
    match mode {
        BodyMode::None => Keep::Drop(Omitted::HeadersOnly),
        BodyMode::All => Keep::Full,
        BodyMode::Text => match content_type.map(essence).as_deref() {
            None | Some("") => Keep::Sniff,
            Some(ct) if is_textual(ct) || ct.starts_with("multipart/") => Keep::Full,
            Some(ct) if is_binary(ct) => Keep::Drop(Omitted::Binary),
            Some(_) => Keep::Sniff,
        },
    }
}

fn is_textual(ct: &str) -> bool {
    let (ty, sub) = ct.split_once('/').unwrap_or((ct, ""));
    ty == "text"
        || matches!(
            sub,
            "json"
                | "x-ndjson"
                | "xml"
                | "x-www-form-urlencoded"
                | "javascript"
                | "x-javascript"
                | "ecmascript"
                | "graphql"
                | "problem+json"
        )
        || sub.ends_with("+json")
        || sub.ends_with("+xml")
}

fn is_binary(ct: &str) -> bool {
    let (ty, sub) = ct.split_once('/').unwrap_or((ct, ""));
    matches!(ty, "image" | "video" | "audio" | "font")
        || matches!(
            sub,
            "octet-stream"
                | "zip"
                | "gzip"
                | "x-tar"
                | "pdf"
                | "wasm"
                | "x-protobuf"
                | "protobuf"
                | "grpc"
                | "vnd.ms-fontobject"
        )
}

/// UTF-8 with no NULs. A body cut at the cap may end mid-character.
fn looks_like_text(bytes: &[u8]) -> bool {
    text_of(bytes).is_some()
}

fn text_of(bytes: &[u8]) -> Option<String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    (!text.contains('\0')).then(|| text.to_string())
}

pub struct Decoded {
    pub bytes: Vec<u8>,
    pub error: Option<String>,
}

/// Best effort: a body truncated at the cap still inflates up to the cut.
fn decode(data: &[u8], encoding: Option<&str>) -> Decoded {
    let reader: Box<dyn Read + '_> = match encoding.map(str::to_ascii_lowercase).as_deref() {
        None => {
            return Decoded {
                bytes: data.to_vec(),
                error: None,
            };
        }
        Some("gzip" | "x-gzip") => Box::new(flate2::read::MultiGzDecoder::new(data)),
        Some("deflate") => Box::new(flate2::read::ZlibDecoder::new(data)),
        Some("br") => Box::new(brotli::Decompressor::new(data, 4096)),
        Some(other) => {
            return Decoded {
                bytes: data.to_vec(),
                error: Some(format!("{other} isn't decoded; showing it as sent")),
            };
        }
    };
    let mut bytes = Vec::new();
    let error = match reader.take(DECODED_CAP).read_to_end(&mut bytes) {
        Ok(_) if bytes.len() as u64 >= DECODED_CAP => {
            Some(format!("decoded body cut at {} MiB", DECODED_CAP >> 20))
        }
        Ok(_) => None,
        Err(_) if !bytes.is_empty() => None,
        Err(e) => Some(format!("couldn't decode {}: {e}", encoding.unwrap_or(""))),
    };
    Decoded { bytes, error }
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    if !essence(content_type).starts_with("multipart/") {
        return None;
    }
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    name: Option<String>,
    filename: Option<String>,
    content_type: Option<String>,
    size: usize,
    /// Text fields always; files only in `All` mode, and only when text.
    text: Option<String>,
}

impl Part {
    fn weight(&self) -> usize {
        self.text.as_ref().map_or(0, String::len) + 64
    }
}

fn parse_multipart(body: &[u8], boundary: &str, keep_files: bool) -> Vec<Part> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    for chunk in split_bytes(body, delimiter.as_bytes()).skip(1) {
        if chunk.starts_with(b"--") {
            break;
        }
        let chunk = chunk.strip_prefix(b"\r\n").unwrap_or(chunk);
        let chunk = chunk.strip_suffix(b"\r\n").unwrap_or(chunk);
        let Some(split) = find(chunk, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&chunk[..split]);
        let content = &chunk[split + 4..];
        let mut part = Part {
            name: None,
            filename: None,
            content_type: None,
            size: content.len(),
            text: None,
        };
        for line in head.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.trim().eq_ignore_ascii_case("content-type") {
                part.content_type = Some(v.trim().to_string());
            } else if k.trim().eq_ignore_ascii_case("content-disposition") {
                for param in v.split(';').skip(1) {
                    if let Some((pk, pv)) = param.split_once('=') {
                        let pv = pv.trim().trim_matches('"').to_string();
                        match pk.trim() {
                            "name" => part.name = Some(pv),
                            "filename" => part.filename = Some(pv),
                            _ => {}
                        }
                    }
                }
            }
        }
        let is_file = part.filename.is_some()
            || part
                .content_type
                .as_deref()
                .is_some_and(|ct| !is_textual(&essence(ct)));
        if !is_file || keep_files {
            part.text = text_of(content);
        }
        parts.push(part);
    }
    parts
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn split_bytes<'a>(mut body: &'a [u8], delimiter: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        match find(body, delimiter) {
            Some(at) => {
                let piece = &body[..at];
                body = &body[at + delimiter.len()..];
                Some(piece)
            }
            None => {
                done = true;
                Some(body)
            }
        }
    })
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Serialize)]
pub struct Summary {
    pub id: u64,
    started_at_ms: u64,
    method: String,
    url: String,
    host: String,
    source: String,
    status: Option<u16>,
    pub state: &'static str,
    upgraded: bool,
    content_type: Option<String>,
    request_size: u64,
    response_size: u64,
    duration_ms: Option<f64>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct Detail {
    #[serde(flatten)]
    summary: Summary,
    scheme: String,
    mode: BodyMode,
    ttfb_ms: Option<f64>,
    request: SideView,
    response: SideView,
}

#[derive(Serialize)]
struct SideView {
    headers: Vec<(String, String)>,
    body: BodyView,
}

#[derive(Serialize)]
struct BodyView {
    size: u64,
    content_type: Option<String>,
    content_encoding: Option<String>,
    state: BodyState,
    truncated: bool,
    omitted: Option<Omitted>,
    text: Option<String>,
    binary: bool,
    decode_error: Option<String>,
    parts: Option<Vec<Part>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Request,
    Response,
}

impl Exchange {
    pub fn summary(&self) -> Summary {
        let d = self.data.lock().unwrap();
        Summary {
            id: self.id,
            started_at_ms: d.started_at_ms,
            method: d.method.clone(),
            url: d.url.clone(),
            host: d.host.clone(),
            source: d.source.clone(),
            status: d.status,
            state: d.state(),
            upgraded: d.upgraded,
            content_type: d.response.content_type.clone(),
            request_size: d.request.size,
            response_size: d.response.size,
            duration_ms: d.duration_ms,
            error: d.error.clone(),
        }
    }

    pub fn detail(&self) -> Detail {
        let summary = self.summary();
        let d = self.data.lock().unwrap();
        Detail {
            summary,
            scheme: d.scheme.clone(),
            mode: d.mode,
            ttfb_ms: d.ttfb_ms,
            request: SideView {
                headers: d.request_headers.clone(),
                body: d.request.view(),
            },
            response: SideView {
                headers: d.response_headers.clone(),
                body: d.response.view(),
            },
        }
    }

    /// One line of the sidecar's capture stream, newline included.
    pub fn ship(&self, node: &str) -> Vec<u8> {
        let d = self.data.lock().unwrap();
        let mut line = serde_json::to_vec(&ShippedRef {
            node,
            id: self.id,
            data: &d,
        })
        .expect("an exchange always serializes");
        line.push(b'\n');
        line
    }

    /// The decoded body and its content type, or `None` if none was kept.
    pub fn body(&self, side: Side) -> Option<(Vec<u8>, Option<String>)> {
        let d = self.data.lock().unwrap();
        let captured = match side {
            Side::Request => &d.request,
            Side::Response => &d.response,
        };
        if matches!(captured.keep, Keep::Drop(_)) || captured.data.is_empty() {
            return None;
        }
        Some((captured.decoded().bytes, captured.content_type.clone()))
    }
}

/// One exchange being recorded, handed along the proxy's request path.
#[derive(Clone)]
pub struct Recording {
    log: Arc<TrafficLog>,
    ex: Arc<Exchange>,
}

impl Recording {
    fn publish(&self) {
        self.log.publish(&self.ex);
    }

    pub fn tap<B: Body>(&self, side: Side, body: B) -> TapBody<B> {
        TapBody {
            inner: body,
            sink: Some((self.clone(), side)),
        }
    }

    pub fn response<B>(&self, resp: &Response<B>, upgraded: bool) {
        {
            let mut d = self.ex.data.lock().unwrap();
            d.status = Some(resp.status().as_u16());
            d.ttfb_ms = Some(d.elapsed_ms());
            d.response_headers = header_list(resp.headers());
            d.response = Captured::new(d.mode, resp.headers());
            d.upgraded = upgraded;
        }
        self.log.account(&self.ex);
        self.publish();
    }

    /// The proxy answered 502 instead.
    pub fn fail(&self, error: &anyhow::Error) {
        {
            let mut d = self.ex.data.lock().unwrap();
            d.status = Some(502);
            d.error = Some(format!("{error:#}"));
            d.duration_ms = Some(d.elapsed_ms());
        }
        self.publish();
    }

    /// An upgraded connection's relay ended.
    pub fn closed(&self) {
        {
            let mut d = self.ex.data.lock().unwrap();
            d.duration_ms = Some(d.elapsed_ms());
        }
        self.publish();
    }

    fn chunk(&self, side: Side, chunk: &[u8]) {
        let mut d = self.ex.data.lock().unwrap();
        match side {
            Side::Request => d.request.push(chunk),
            Side::Response => d.response.push(chunk),
        }
    }

    fn finish(&self, side: Side, state: BodyState) {
        {
            let mut d = self.ex.data.lock().unwrap();
            let mode = d.mode;
            match side {
                Side::Request => d.request.finish(state, mode),
                Side::Response => {
                    d.response.finish(state, mode);
                    d.duration_ms = Some(d.elapsed_ms());
                }
            }
        }
        self.log.account(&self.ex);
        self.publish();
    }
}

/// Passes a body through unchanged, copying it into the recording. Finishes
/// the recording on end, error, or drop, a drop before the end being an abort.
pub struct TapBody<B: Body> {
    inner: B,
    sink: Option<(Recording, Side)>,
}

impl<B> Body for TapBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        let frame = ready!(Pin::new(&mut this.inner).poll_frame(cx));
        match &frame {
            Some(Ok(frame)) => {
                if let (Some(data), Some((rec, side))) = (frame.data_ref(), &this.sink) {
                    rec.chunk(*side, data);
                }
            }
            Some(Err(_)) => {
                if let Some((rec, side)) = this.sink.take() {
                    rec.finish(side, BodyState::Error);
                }
            }
            None => {
                if let Some((rec, side)) = this.sink.take() {
                    rec.finish(side, BodyState::Done);
                }
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B: Body> Drop for TapBody<B> {
    fn drop(&mut self) {
        if let Some((rec, side)) = self.sink.take() {
            // hyper stops polling once a body says it has ended, so a fully
            // read body can be dropped without ever yielding `None`.
            let state = if self.inner.is_end_stream() {
                BodyState::Done
            } else {
                BodyState::Aborted
            };
            rec.finish(side, state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use http_body_util::{BodyExt, Full};
    use std::io::Write;

    fn store_log() -> (TrafficStore, Arc<TrafficLog>) {
        let store = TrafficStore::default();
        let log = store.log("ws", "api");
        (store, log)
    }

    fn request(content_type: Option<&str>, encoding: Option<&str>) -> Request<()> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/things?x=1")
            .header("host", "api.fghj.internal");
        if let Some(ct) = content_type {
            b = b.header("content-type", ct);
        }
        if let Some(e) = encoding {
            b = b.header("content-encoding", e);
        }
        b.body(()).unwrap()
    }

    fn response(content_type: &str, encoding: Option<&str>) -> Response<()> {
        let mut b = Response::builder()
            .status(200)
            .header("content-type", content_type);
        if let Some(e) = encoding {
            b = b.header("content-encoding", e);
        }
        b.body(()).unwrap()
    }

    /// Pushes `body` through a tap the way hyper would and returns the view.
    async fn round_trip(mode: BodyMode, resp: Response<()>, body: Vec<u8>) -> serde_json::Value {
        let (_store, log) = store_log();
        log.set_mode(Some(mode));
        let rec = log
            .begin(&request(None, None), "host".into(), "https")
            .unwrap();
        rec.response(&resp, false);
        let tapped = rec.tap(Side::Response, Full::new(Bytes::from(body)));
        tapped.collect().await.unwrap();
        serde_json::to_value(log.get(rec.ex.id).unwrap().detail()).unwrap()
    }

    #[test]
    fn nothing_is_recorded_while_recording_is_off() {
        let (_store, log) = store_log();
        assert!(
            log.begin(&request(None, None), "host".into(), "https")
                .is_none()
        );
        assert!(log.summaries().is_empty());
    }

    #[tokio::test]
    async fn json_is_kept_in_full() {
        let body = serde_json::to_vec(&serde_json::json!({ "lines": vec!["x"; 100] })).unwrap();
        let v = round_trip(
            BodyMode::Text,
            response("application/json; charset=utf-8", None),
            body.clone(),
        )
        .await;
        let resp = &v["response"]["body"];
        assert_eq!(resp["text"].as_str().unwrap().as_bytes(), &body[..]);
        assert_eq!(resp["size"], body.len());
        assert_eq!(resp["state"], "done");
        assert_eq!(v["state"], "done");
        assert_eq!(v["status"], 200);
    }

    #[tokio::test]
    async fn images_are_dropped_in_text_mode_and_kept_in_all() {
        let png = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3];
        let v = round_trip(BodyMode::Text, response("image/png", None), png.clone()).await;
        assert_eq!(v["response"]["body"]["omitted"], "binary");
        assert_eq!(v["response"]["body"]["size"], png.len());

        let v = round_trip(BodyMode::All, response("image/png", None), png.clone()).await;
        assert_eq!(v["response"]["body"]["omitted"], serde_json::Value::Null);
        assert_eq!(v["response"]["body"]["binary"], true);
    }

    #[tokio::test]
    async fn headers_only_keeps_no_body() {
        let v = round_trip(
            BodyMode::None,
            response("application/json", None),
            b"{}".to_vec(),
        )
        .await;
        assert_eq!(v["response"]["body"]["omitted"], "headers_only");
        assert_eq!(v["response"]["body"]["size"], 2);
    }

    #[tokio::test]
    async fn an_untyped_body_is_kept_only_if_it_is_text() {
        let (_store, log) = store_log();
        log.set_mode(Some(BodyMode::Text));
        let untyped = Response::builder().status(200).body(()).unwrap();
        for (body, kept) in [
            (b"plain words".to_vec(), true),
            (vec![0xff, 0x00, 0xfe], false),
        ] {
            let rec = log
                .begin(&request(None, None), "host".into(), "https")
                .unwrap();
            rec.response(&untyped, false);
            rec.tap(Side::Response, Full::new(Bytes::from(body)))
                .collect()
                .await
                .unwrap();
            let v = serde_json::to_value(log.get(rec.ex.id).unwrap().detail()).unwrap();
            assert_eq!(v["response"]["body"]["text"].is_string(), kept, "{v}");
        }
    }

    #[tokio::test]
    async fn bodies_past_the_cap_are_truncated_but_fully_counted() {
        let body = vec![b'a'; BODY_CAP + 10];
        let v = round_trip(BodyMode::Text, response("text/plain", None), body).await;
        let resp = &v["response"]["body"];
        assert_eq!(resp["truncated"], true);
        assert_eq!(resp["size"], BODY_CAP + 10);
        assert_eq!(resp["text"].as_str().unwrap().len(), BODY_CAP);
    }

    #[tokio::test]
    async fn compressed_bodies_are_decoded_when_viewed() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"{\"zipped\":true}").unwrap();
        let v = round_trip(
            BodyMode::Text,
            response("application/json", Some("gzip")),
            gz.finish().unwrap(),
        )
        .await;
        assert_eq!(v["response"]["body"]["text"], "{\"zipped\":true}");

        let mut br = Vec::new();
        brotli::CompressorWriter::new(&mut br, 4096, 5, 22)
            .write_all(b"brotli!")
            .unwrap();
        let v = round_trip(BodyMode::Text, response("text/plain", Some("br")), br).await;
        assert_eq!(v["response"]["body"]["text"], "brotli!");
    }

    #[tokio::test]
    async fn multipart_keeps_fields_and_only_the_shape_of_files() {
        let (_store, log) = store_log();
        log.set_mode(Some(BodyMode::Text));
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\x00\x01\r\n\
--XyZ--\r\n";
        let rec = log
            .begin(
                &request(Some("multipart/form-data; boundary=XyZ"), None),
                "host".into(),
                "https",
            )
            .unwrap();
        rec.tap(Side::Request, Full::new(Bytes::from_static(body)))
            .collect()
            .await
            .unwrap();
        let v = serde_json::to_value(log.get(rec.ex.id).unwrap().detail()).unwrap();
        let parts = v["request"]["body"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["name"], "title");
        assert_eq!(parts[0]["text"], "hello");
        assert_eq!(parts[1]["filename"], "a.png");
        assert_eq!(parts[1]["size"], 6);
        assert_eq!(parts[1]["text"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_body_dropped_halfway_is_aborted() {
        let (_store, log) = store_log();
        log.set_mode(Some(BodyMode::Text));
        let rec = log
            .begin(&request(None, None), "host".into(), "https")
            .unwrap();
        rec.response(&response("text/plain", None), false);
        let first = Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from_static(b"half")));
        let stream = futures_util::stream::iter([first]).chain(futures_util::stream::pending());
        let mut tapped = rec.tap(Side::Response, http_body_util::StreamBody::new(stream));
        tapped.frame().await.unwrap().unwrap();
        drop(tapped);
        let v = serde_json::to_value(log.get(rec.ex.id).unwrap().detail()).unwrap();
        assert_eq!(v["state"], "aborted");
        assert_eq!(v["response"]["body"]["text"], "half");
    }

    #[test]
    fn the_ring_evicts_the_oldest_past_its_count() {
        let (_store, log) = store_log();
        log.set_mode(Some(BodyMode::None));
        let ids: Vec<u64> = (0..MAX_EXCHANGES + 5)
            .map(|_| {
                log.begin(&request(None, None), "host".into(), "https")
                    .unwrap()
                    .ex
                    .id
            })
            .collect();
        let kept = log.summaries();
        assert_eq!(kept.len(), MAX_EXCHANGES);
        assert_eq!(kept[0].id, ids[5]);
        assert!(log.get(ids[0]).is_none());
    }

    #[tokio::test]
    async fn the_ring_evicts_past_its_byte_budget_and_clear_frees_everything() {
        let store = TrafficStore::default();
        let log = store.log("ws", "api");
        log.set_mode(Some(BodyMode::Text));
        for _ in 0..(NODE_BUDGET / BODY_CAP + 2) {
            let rec = log
                .begin(&request(None, None), "host".into(), "https")
                .unwrap();
            rec.response(&response("text/plain", None), false);
            rec.tap(Side::Response, Full::new(Bytes::from(vec![b'a'; BODY_CAP])))
                .collect()
                .await
                .unwrap();
        }
        let ring_bytes = log.ring.lock().unwrap().bytes;
        assert!(ring_bytes <= NODE_BUDGET, "{ring_bytes}");
        assert!(log.summaries().len() < NODE_BUDGET / BODY_CAP + 2);
        assert_eq!(store.total.load(Ordering::Relaxed), ring_bytes);

        log.clear();
        assert!(log.summaries().is_empty());
        assert_eq!(store.total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn recording_lists_only_that_workspaces_recording_nodes() {
        let store = TrafficStore::default();
        store.log("ws", "api").set_mode(Some(BodyMode::All));
        store.log("ws", "web");
        store.log("other", "db").set_mode(Some(BodyMode::Text));
        assert_eq!(
            store.recording("ws"),
            HashMap::from([("api".to_string(), BodyMode::All)])
        );
    }
}
