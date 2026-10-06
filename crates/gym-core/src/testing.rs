//! Deterministic fakes for exercising the engine without hardware or a media player.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crossbeam_channel::Sender;

use crate::capture::{
    AudioCaptureBackend, CaptureClock, CaptureError, CaptureEvent, CaptureHandle, CaptureSource,
    CaptureSourceKind, CaptureStream, PcmSpec,
};
use crate::now_playing::{NowPlayingError, NowPlayingEvent, NowPlayingSource, Subscription};

/// A now-playing source driven by the test.
#[derive(Clone, Default)]
pub struct FakeNowPlaying {
    sinks: Arc<Mutex<Vec<Sender<NowPlayingEvent>>>>,
}

impl FakeNowPlaying {
    pub fn emit(&self, event: NowPlayingEvent) {
        for sink in self.sinks.lock().unwrap().iter() {
            let _ = sink.send(event.clone());
        }
    }
}

impl NowPlayingSource for FakeNowPlaying {
    fn start(&self, sink: Sender<NowPlayingEvent>) -> Result<Subscription, NowPlayingError> {
        self.sinks.lock().unwrap().push(sink);
        Ok(Subscription::new(()))
    }
}

struct FakeStream;
impl CaptureStream for FakeStream {}

/// Pushes audio into a running engine as if it came from a device.
pub struct FakeFeeder {
    producer: rtrb::Producer<f32>,
    clock: Arc<CaptureClock>,
    spec: PcmSpec,
    frames: u64,
    pub events: Sender<CaptureEvent>,
}

impl FakeFeeder {
    /// Pushes interleaved samples whose first frame was captured at `at`.
    ///
    /// Blocks (spinning) while the queue is full, so tests never drop audio.
    pub fn feed(&mut self, samples: &[f32], at: SystemTime) {
        self.clock.anchor(self.frames, at);
        let mut rest = samples;
        while !rest.is_empty() {
            let n = self.producer.slots().min(rest.len());
            let n = n - n % self.spec.channels as usize;
            if n == 0 {
                std::thread::yield_now();
                continue;
            }
            let mut chunk = self.producer.write_chunk_uninit(n).unwrap();
            let (first, second) = chunk.as_mut_slices();
            let split = first.len();
            for (slot, &s) in first.iter_mut().zip(&rest[..split]) {
                slot.write(s);
            }
            for (slot, &s) in second.iter_mut().zip(&rest[split..n]) {
                slot.write(s);
            }
            // SAFETY: every slot of the chunk was initialized above.
            unsafe { chunk.commit_all() };
            rest = &rest[n..];
        }
        self.frames += (samples.len() / self.spec.channels as usize) as u64;
    }
}

/// A capture backend with one source; `start` hands the feeder to the test.
pub struct FakeCapture {
    spec: PcmSpec,
    feeder: Mutex<Option<Sender<FakeFeeder>>>,
}

impl FakeCapture {
    /// Returns the backend and a receiver that yields the feeder once capture starts.
    pub fn new(spec: PcmSpec) -> (Self, crossbeam_channel::Receiver<FakeFeeder>) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        (
            Self {
                spec,
                feeder: Mutex::new(Some(tx)),
            },
            rx,
        )
    }

    fn source(&self) -> CaptureSource {
        CaptureSource {
            id: "fake".into(),
            name: "Fake".into(),
            kind: CaptureSourceKind::OutputLoopback,
            is_default: true,
            channels: self.spec.channels,
            sample_rate: self.spec.sample_rate,
        }
    }
}

impl AudioCaptureBackend for FakeCapture {
    fn list_sources(&self) -> Result<Vec<CaptureSource>, CaptureError> {
        Ok(vec![self.source()])
    }

    fn start(&self, _source_id: Option<&str>) -> Result<CaptureHandle, CaptureError> {
        let capacity = self.spec.sample_rate as usize * self.spec.channels as usize * 4;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let clock = Arc::new(CaptureClock::default());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let feeder = FakeFeeder {
            producer,
            clock: Arc::clone(&clock),
            spec: self.spec,
            frames: 0,
            events: events_tx,
        };
        if let Some(tx) = self.feeder.lock().unwrap().take() {
            let _ = tx.send(feeder);
        }
        Ok(CaptureHandle {
            source: self.source(),
            spec: self.spec,
            samples: consumer,
            clock,
            events: events_rx,
            stream: Box::new(FakeStream),
        })
    }
}

/// A nekostorage `api` route at `<url>` on a local port, keeping files in memory.
///
/// Implements `upload`, `mkdir` and `inspect` as documented, including the bearer token.
pub struct FakeNekostorage {
    url: String,
    state: Arc<Mutex<NekoState>>,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct NekoState {
    token: Option<String>,
    /// Folder paths like `/a/b`; `/` is the view root.
    dirs: std::collections::BTreeSet<String>,
    files: std::collections::BTreeMap<String, Vec<u8>>,
    /// Statuses returned to the next uploads instead of handling them.
    upload_replies: std::collections::VecDeque<u16>,
    uploads: usize,
    free: Option<u64>,
    redirect: bool,
}

type NekoReply = tiny_http::Response<std::io::Cursor<Vec<u8>>>;

impl FakeNekostorage {
    /// Starts the server; requests must carry `token` when one is given.
    pub fn start(token: Option<&str>) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind fake server"));
        let port = server.server_addr().to_ip().expect("TCP address").port();
        let state = Arc::new(Mutex::new(NekoState {
            token: token.map(Into::into),
            dirs: ["/".to_owned()].into(),
            files: Default::default(),
            upload_replies: Default::default(),
            uploads: 0,
            free: Some(6_436_315_136),
            redirect: false,
        }));
        let thread = {
            let (server, state) = (Arc::clone(&server), Arc::clone(&state));
            std::thread::spawn(move || {
                for mut request in server.incoming_requests() {
                    let reply = neko_reply(&mut state.lock().unwrap(), &mut request);
                    let _ = request.respond(reply);
                }
            })
        };
        Self {
            url: format!("http://127.0.0.1:{port}/api"),
            state,
            server,
            thread: Some(thread),
        }
    }

    /// The route, e.g. `http://127.0.0.1:1234/api`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Contents of the file at `path` (like `/a/b.flac`).
    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(path).cloned()
    }

    /// Paths of all files.
    pub fn files(&self) -> Vec<String> {
        self.state.lock().unwrap().files.keys().cloned().collect()
    }

    pub fn add_file(&self, path: &str, contents: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.into(), contents.into());
    }

    /// The next uploads get these statuses without being handled.
    pub fn queue_upload_replies(&self, statuses: &[u16]) {
        self.state
            .lock()
            .unwrap()
            .upload_replies
            .extend(statuses.iter().copied());
    }

    /// Upload requests received, including rejected ones.
    pub fn uploads(&self) -> usize {
        self.state.lock().unwrap().uploads
    }

    /// Free space reported in `usage`; `None` reports `usage: null`.
    pub fn set_free(&self, free: Option<u64>) {
        self.state.lock().unwrap().free = free;
    }

    /// Answers every request with a redirect to another host.
    pub fn redirect_everything(&self) {
        self.state.lock().unwrap().redirect = true;
    }
}

impl Drop for FakeNekostorage {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn neko_status(code: u16) -> NekoReply {
    tiny_http::Response::from_string(code.to_string()).with_status_code(code)
}

fn neko_json(code: u16, value: serde_json::Value) -> NekoReply {
    let header = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
    tiny_http::Response::from_string(value.to_string())
        .with_status_code(code)
        .with_header(header)
}

fn neko_header(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_owned())
}

fn neko_parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((parent, _)) => parent.into(),
    }
}

fn neko_metadata(path: &str, kind: &str, size: usize) -> serde_json::Value {
    let name = (path != "/").then(|| path.rsplit('/').next().unwrap_or(path));
    serde_json::json!({
        "path": path, "name": name, "kind": kind, "size": size,
        "modified": 1_791_286_876u64, "created": 1_791_286_876u64,
        "etag": null, "content_type": null,
    })
}

fn neko_reply(state: &mut NekoState, request: &mut tiny_http::Request) -> NekoReply {
    use tiny_http::Method;

    if state.redirect {
        let location =
            tiny_http::Header::from_bytes("Location", "http://example.invalid/").unwrap();
        return neko_status(307).with_header(location);
    }
    if let Some(token) = &state.token
        && neko_header(request, "Authorization") != Some(format!("Bearer {token}"))
    {
        let challenge = tiny_http::Header::from_bytes("WWW-Authenticate", "Bearer").unwrap();
        return neko_status(401).with_header(challenge);
    }
    let url = request.url().to_owned();
    let (route, query) = url.split_once('?').unwrap_or((&url, ""));
    let Some(rest) = route.strip_prefix("/api/") else {
        return neko_status(404);
    };
    let (action, raw_path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut segments = Vec::new();
    for segment in raw_path.split('/').filter(|s| !s.is_empty()) {
        match percent_encoding::percent_decode_str(segment).decode_utf8() {
            Ok(s) if s != "." && s != ".." => segments.push(s.into_owned()),
            _ => return neko_status(400),
        }
    }
    let path = format!("/{}", segments.join("/"));

    match (action, request.method()) {
        ("upload", Method::Put) => {
            state.uploads += 1;
            if let Some(code) = state.upload_replies.pop_front() {
                return neko_status(code);
            }
            if path == "/" {
                return neko_status(403);
            }
            let parent = neko_parent(&path);
            if state.files.contains_key(&parent) || state.dirs.contains(&path) {
                return neko_status(409);
            }
            if !state.dirs.contains(&parent) {
                return neko_status(404);
            }
            let existed = state.files.contains_key(&path);
            if existed && neko_header(request, "If-None-Match").as_deref() == Some("*") {
                return neko_status(412);
            }
            let expected = request.body_length();
            let mut body = Vec::new();
            if request.as_reader().read_to_end(&mut body).is_err()
                || expected.is_some_and(|len| len != body.len())
            {
                return neko_status(400);
            }
            let size = body.len();
            state.files.insert(path.clone(), body);
            neko_json(
                if existed { 200 } else { 201 },
                neko_metadata(&path, "file", size),
            )
        }
        ("mkdir", Method::Post) => {
            let parents = query
                .split('&')
                .any(|q| matches!(q, "parents" | "parents=true" | "parents=1"));
            if path == "/" || (!parents && state.dirs.contains(&path)) {
                return neko_status(409);
            }
            if !parents && !state.dirs.contains(&neko_parent(&path)) {
                return neko_status(404);
            }
            let mut created = false;
            let mut current = String::new();
            for segment in &segments {
                current = format!("{current}/{segment}");
                if state.files.contains_key(&current) {
                    return neko_status(409);
                }
                created = state.dirs.insert(current.clone());
            }
            neko_json(
                if created { 201 } else { 200 },
                neko_metadata(&path, "directory", 0),
            )
        }
        ("inspect", Method::Get | Method::Head) => {
            if let Some(bytes) = state.files.get(&path) {
                return neko_json(200, neko_metadata(&path, "file", bytes.len()));
            }
            if !state.dirs.contains(&path) {
                return neko_status(404);
            }
            let children = state
                .dirs
                .iter()
                .map(|p| (p, "directory", 0))
                .chain(state.files.iter().map(|(p, b)| (p, "file", b.len())))
                .filter(|(p, _, _)| p.as_str() != "/" && neko_parent(p) == path)
                .map(|(p, kind, size)| neko_metadata(p, kind, size))
                .collect::<Vec<_>>();
            let mut reply = neko_metadata(&path, "directory", 0);
            reply["entries"] = children.into();
            reply["usage"] = match state.free {
                Some(free) => serde_json::json!({ "total": null, "used": null, "free": free }),
                None => serde_json::Value::Null,
            };
            neko_json(200, reply)
        }
        ("upload" | "mkdir" | "inspect", _) => neko_status(405),
        _ => neko_status(404),
    }
}
