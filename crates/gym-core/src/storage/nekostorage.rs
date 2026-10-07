//! Stores recordings on a nekostorage server through its HTTP `api` route.
//!
//! A store creates the parent folder (`mkdir?parents`) and looks for an existing file
//! (`inspect`) before it uploads, so a refused upload never sends the whole file. The upload
//! carries `If-None-Match: *` unless overwriting, which settles races.
//!
//! Listing walks the folder tree with `inspect`. Deleting a file (`delete`) also removes the
//! folders it leaves empty, up to but not including the recordings folder.

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::http::Response;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Agent, Body, RequestBuilder};
use url::Url;

use super::{
    ConflictPolicy, StorageError, StorageKey, StorageProvider, StoreOutcome, StoredFile,
    StoredObject, is_recording_name,
};
use crate::settings::NekostorageSettings;

/// Path segments are percent-encoded except for RFC 3986 unreserved characters.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

const ID_PREFIX: &str = "nekostorage:";
/// `KeepBoth` gives up after this many numbered siblings.
const MAX_COUNTER: u32 = 999;
/// Directory replies list every entry, so they can be long.
const JSON_LIMIT: u64 = 64 * 1024 * 1024;
/// Replies the server sends before acting on a request, so it can be repeated safely.
const RETRY_STATUSES: [u16; 3] = [423, 429, 503];
/// Folders inspected at once while listing; each one can be a call to a cloud backend.
const LIST_CONCURRENCY: usize = 4;

/// A normalized `api` route plus the folder recordings go to. Identifies stored recordings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NekostorageLocation {
    /// The route without a trailing slash, e.g. `http://127.0.0.1:8080/api`.
    base: String,
    /// `base` without its scheme, for display.
    display: String,
    folder: Vec<String>,
    /// Plain `http://` to another machine, so the token crosses the network unencrypted.
    unencrypted: bool,
}

impl NekostorageLocation {
    pub fn parse(settings: &NekostorageSettings) -> Result<Self, String> {
        Self::new(&settings.url, &settings.folder)
    }

    fn new(url: &str, folder: &str) -> Result<Self, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("enter the address of the server's api route".into());
        }
        let url = Url::parse(url).map_err(|err| format!("invalid server address: {err}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("the server address must start with http:// or https://".into());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("the server address can't contain ? or #".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("put the token in the Token field, not in the address".into());
        }
        // `Url` lowercases the host and drops default ports.
        let host = url.host_str().unwrap_or_default();
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let display = format!("{authority}{}", url.path().trim_end_matches('/'));
        let folder = folder
            .split('/')
            .filter(|part| !part.is_empty())
            .map(|part| match part {
                "." | ".." => Err(format!("the folder can't contain {part:?}")),
                _ => Ok(part.to_owned()),
            })
            .collect::<Result<_, _>>()?;
        let loopback = match url.host() {
            Some(url::Host::Domain(name)) => name == "localhost" || name.ends_with(".localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        Ok(Self {
            base: format!("{}://{display}", url.scheme()),
            display,
            folder,
            unencrypted: url.scheme() == "http" && !loopback,
        })
    }

    /// Identifies recordings stored here, e.g. `nekostorage:http://127.0.0.1:8080/api#/Music`.
    pub fn id(&self) -> String {
        format!("{ID_PREFIX}{}#{}", self.base, self.folder_path())
    }

    pub fn from_id(id: &str) -> Option<Self> {
        // The route has no fragment, so the first `#` ends it.
        let (url, folder) = id.strip_prefix(ID_PREFIX)?.split_once('#')?;
        Self::new(url, folder).ok()
    }

    /// Whether requests, and so the token, cross the network unencrypted.
    pub fn is_unencrypted(&self) -> bool {
        self.unencrypted
    }

    pub fn same_server(&self, other: &Self) -> bool {
        self.base == other.base
    }

    /// `host:port/route/folder`.
    pub fn display(&self) -> String {
        if self.folder.is_empty() {
            self.display.clone()
        } else {
            format!("{}{}", self.display, self.folder_path())
        }
    }

    fn folder_path(&self) -> String {
        show(&self.folder)
    }

    fn path_for(&self, key: &StorageKey) -> Vec<String> {
        self.folder
            .iter()
            .cloned()
            .chain(key.components().map(str::to_owned))
            .collect()
    }

    fn url(&self, action: &str, path: &[String]) -> String {
        let mut url = format!("{}/{action}", self.base);
        for segment in path {
            url.push('/');
            url.extend(utf8_percent_encode(segment, SEGMENT));
        }
        url
    }
}

/// A remote path as `/a/b`.
fn show(path: &[String]) -> String {
    format!("/{}", path.join("/"))
}

/// Retries for requests the server can't have acted on: connection failures and
/// 423, 429 and 503 replies.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Attempts, including the first.
    pub attempts: u32,
    /// Wait before the first retry; each further retry waits four times longer.
    pub backoff: Duration,
    /// Upper bound for a server's `Retry-After`.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            backoff: Duration::from_secs(2),
            max_retry_after: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Wait before retry number `retry` (from 1).
    fn delay(&self, retry: u32, retry_after: Option<Duration>) -> Duration {
        match retry_after {
            Some(wait) => wait.min(self.max_retry_after),
            None => self.backoff * 4u32.saturating_pow(retry - 1),
        }
    }
}

#[derive(Deserialize)]
struct Metadata {
    kind: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    usage: Option<Usage>,
    /// A directory's children. Every directory reply has them; one without is malformed,
    /// and must not pass for an empty folder.
    #[serde(default)]
    entries: Option<Vec<Entry>>,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    kind: String,
    #[serde(default)]
    size: u64,
    /// Unix seconds.
    #[serde(default)]
    modified: Option<i64>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    free: Option<u64>,
}

/// Timeouts for the `inspect` calls of a listing. A server that stops answering mustn't hold
/// a listing for long: it would keep the Library from being refreshed.
#[derive(Clone, Copy, Debug)]
pub struct ListTimeouts {
    pub connect: Duration,
    /// Until the reply starts.
    pub response: Duration,
    /// For the whole reply body; directory replies list every entry.
    pub body: Duration,
}

impl Default for ListTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            response: Duration::from_secs(30),
            body: Duration::from_secs(60),
        }
    }
}

/// Uploads recordings to a folder on a nekostorage server.
pub struct NekostorageStorage {
    location: Result<NekostorageLocation, String>,
    /// The address as entered, shown while it is invalid.
    raw_url: String,
    id: String,
    token: String,
    agent: Agent,
    /// Keeps connections alive across the burst of `inspect` calls a listing makes.
    list_agent: Agent,
    retry: RetryPolicy,
    free_space: Mutex<Option<u64>>,
}

impl NekostorageStorage {
    /// Never fails; an invalid configuration is reported by every operation instead.
    pub fn new(settings: &NekostorageSettings) -> Self {
        Self::build(
            NekostorageLocation::parse(settings),
            settings.url.clone(),
            settings.token.clone(),
        )
    }

    /// The provider for recordings stored at `location`.
    pub fn for_location(location: NekostorageLocation, token: String) -> Self {
        let raw_url = location.base.clone();
        Self::build(Ok(location), raw_url, token)
    }

    fn build(
        location: Result<NekostorageLocation, String>,
        raw_url: String,
        token: String,
    ) -> Self {
        let id = location
            .as_ref()
            .map_or_else(|_| format!("{ID_PREFIX}invalid"), NekostorageLocation::id);
        Self {
            location,
            raw_url,
            id,
            token,
            agent: upload_agent(),
            list_agent: list_agent(ListTimeouts::default()),
            retry: RetryPolicy::default(),
            free_space: Mutex::new(None),
        }
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_list_timeouts(mut self, timeouts: ListTimeouts) -> Self {
        self.list_agent = list_agent(timeouts);
        self
    }

    /// Checks the settings without contacting the server.
    pub fn validate(settings: &NekostorageSettings) -> Result<(), String> {
        NekostorageLocation::parse(settings).map(|_| ())
    }

    fn location(&self) -> Result<&NekostorageLocation, StorageError> {
        self.location
            .as_ref()
            .map_err(|err| StorageError::Unavailable(err.clone()))
    }

    fn authorize<B>(&self, request: RequestBuilder<B>) -> RequestBuilder<B> {
        if self.token.is_empty() {
            request
        } else {
            request.header("Authorization", format!("Bearer {}", self.token))
        }
    }

    /// Sends a request, repeating it while the server can't have acted on it.
    fn send(
        &self,
        attempt: impl FnMut() -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, StorageError> {
        self.send_with(false, attempt)
    }

    /// Like [`send`](Self::send); an `idempotent` request is also repeated once after a
    /// failure partway through, such as a pooled connection the server had closed. Only once,
    /// so a server that stopped answering isn't waited on again and again.
    fn send_with(
        &self,
        idempotent: bool,
        mut attempt: impl FnMut() -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, StorageError> {
        let mut retry = 0;
        let mut repeated_transfer = false;
        loop {
            let last = retry + 1 >= self.retry.attempts;
            let retry_after = match attempt() {
                Ok(response) if !last && RETRY_STATUSES.contains(&response.status().as_u16()) => {
                    tracing::warn!(status = %response.status(), "nekostorage is busy; retrying");
                    retry_after(&response)
                }
                Ok(response) => return Ok(response),
                Err(err) if !last && before_request(&err) => {
                    tracing::warn!(%err, "cannot reach nekostorage; retrying");
                    None
                }
                Err(err) if idempotent && !repeated_transfer => {
                    repeated_transfer = true;
                    tracing::warn!(%err, "nekostorage request failed; repeating it once");
                    None
                }
                Err(err) => return Err(transport_error(err)),
            };
            retry += 1;
            thread::sleep(self.retry.delay(retry, retry_after));
        }
    }

    fn upload(
        &self,
        location: &NekostorageLocation,
        path: &[String],
        source: &Path,
        replace: bool,
    ) -> Result<Response<Body>, StorageError> {
        let url = location.url("upload", path);
        let content_type = path.last().and_then(|name| content_type(name));
        self.send(|| {
            // `File` sends its size as `Content-Length`.
            let file = File::open(source)?;
            let mut request = self.authorize(self.agent.put(&url));
            if !replace {
                request = request.header("If-None-Match", "*");
            }
            if let Some(content_type) = content_type {
                request = request.header("Content-Type", content_type);
            }
            request.send(file)
        })
    }

    fn create_folder(
        &self,
        location: &NekostorageLocation,
        path: &[String],
    ) -> Result<(), StorageError> {
        if path.is_empty() {
            return Ok(());
        }
        let url = format!("{}?parents", location.url("mkdir", path));
        let response = self.send(|| self.authorize(self.agent.post(&url)).send_empty())?;
        match response.status().as_u16() {
            200 | 201 => Ok(()),
            404 => Err(not_an_api_route()),
            _ => Err(status_error(&response, path)),
        }
    }

    fn inspect(
        &self,
        location: &NekostorageLocation,
        path: &[String],
    ) -> Result<Option<Metadata>, StorageError> {
        let url = location.url("inspect", path);
        let response = self.send(|| self.authorize(self.agent.get(&url)).call())?;
        match response.status().as_u16() {
            200 => read_json(response).map(Some),
            404 => Ok(None),
            _ => Err(status_error(&response, path)),
        }
    }

    fn exists_at(
        &self,
        location: &NekostorageLocation,
        path: &[String],
    ) -> Result<bool, StorageError> {
        let url = location.url("inspect", path);
        let response = self.send(|| self.authorize(self.agent.head(&url)).call())?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error(&response, path)),
        }
    }

    /// Inspects the folder, or its nearest existing ancestor, and remembers its free space.
    fn reach_folder(&self) -> Result<(), StorageError> {
        let location = self.location()?;
        // The folder is created by the first upload; until then, ask its nearest ancestor.
        for depth in (0..=location.folder.len()).rev() {
            let path = &location.folder[..depth];
            match self.inspect(location, path)? {
                Some(metadata) if metadata.kind == "directory" => {
                    self.remember_usage(&metadata);
                    return Ok(());
                }
                Some(_) => {
                    return Err(StorageError::Remote(format!(
                        "{} is a file on the server",
                        show(path)
                    )));
                }
                None => {}
            }
        }
        Err(not_an_api_route())
    }

    /// One folder of a listing, or `None` when a subfolder vanished meanwhile. A missing root
    /// means nothing was ever stored there.
    fn list_folder(
        &self,
        location: &NekostorageLocation,
        path: &[String],
    ) -> Result<Option<Metadata>, StorageError> {
        let url = location.url("inspect", path);
        let response = self.send_with(true, || self.authorize(self.list_agent.get(&url)).call())?;
        match response.status().as_u16() {
            200 => {
                let metadata: Metadata = read_json(response)?;
                if metadata.kind != "directory" {
                    return Err(StorageError::Remote(format!(
                        "{} is a file on the server",
                        show(path)
                    )));
                }
                if metadata.entries.is_none() {
                    return Err(StorageError::Remote(format!(
                        "unexpected reply from the server: folder {} without entries",
                        show(path)
                    )));
                }
                Ok(Some(metadata))
            }
            404 if path.len() == location.folder.len() => {
                Err(StorageError::Missing(location.display()))
            }
            // Deleted while the listing ran, so it holds nothing any more.
            404 => Ok(None),
            _ => Err(status_error(&response, path)),
        }
    }

    /// Inspects `paths` a few at a time; fails if any of them fails.
    fn list_folders(
        &self,
        location: &NekostorageLocation,
        paths: &[Vec<String>],
    ) -> Result<Vec<(Vec<String>, Metadata)>, StorageError> {
        let chunk = paths.len().div_ceil(LIST_CONCURRENCY).max(1);
        thread::scope(|scope| {
            let workers: Vec<_> = paths
                .chunks(chunk)
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut folders = Vec::new();
                        for path in chunk {
                            if let Some(folder) = self.list_folder(location, path)? {
                                folders.push((path.clone(), folder));
                            }
                        }
                        Ok::<_, StorageError>(folders)
                    })
                })
                .collect();
            let mut folders = Vec::with_capacity(paths.len());
            for worker in workers {
                folders.extend(worker.join().expect("listing thread panicked")?);
            }
            Ok(folders)
        })
    }

    fn delete_at(
        &self,
        location: &NekostorageLocation,
        path: &[String],
    ) -> Result<Response<Body>, StorageError> {
        let url = location.url("delete", path);
        self.send(|| self.authorize(self.agent.delete(&url)).call())
    }

    fn remember_usage(&self, metadata: &Metadata) {
        *self.free_space.lock().unwrap() = metadata.usage.as_ref().and_then(|usage| usage.free);
    }
}

impl StorageProvider for NekostorageStorage {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_location(&self) -> String {
        match &self.location {
            Ok(location) => location.display(),
            Err(_) => self.raw_url.clone(),
        }
    }

    fn store(
        &self,
        key: &StorageKey,
        source: &Path,
        policy: ConflictPolicy,
    ) -> Result<StoreOutcome, StorageError> {
        let location = self.location()?;
        fs::metadata(source)?;
        let full_path = location.path_for(key);
        let parent = &full_path[..full_path.len() - 1];
        // Numbered siblings share the folder, so it is created once.
        self.create_folder(location, parent)?;
        let replace = policy == ConflictPolicy::Overwrite;
        let mut candidate = key.clone();
        let mut counter = 1;
        let mut recreated_folder = false;
        loop {
            let path = location.path_for(&candidate);
            // `None` means the name is taken.
            let stored = if !replace && self.exists_at(location, &path)? {
                None
            } else {
                let response = self.upload(location, &path, source, replace)?;
                match response.status().as_u16() {
                    200 | 201 => Some(read_json::<Metadata>(response)?),
                    // Another writer got there between the check and the upload.
                    412 => None,
                    // The folder went away meanwhile, e.g. a delete that emptied it.
                    404 if !recreated_folder => {
                        recreated_folder = true;
                        self.create_folder(location, parent)?;
                        continue;
                    }
                    _ => return Err(status_error(&response, &path)),
                }
            };
            if let Some(stored) = stored {
                if let Err(err) = fs::remove_file(source) {
                    // Startup cleanup of the cache folder removes it later.
                    tracing::warn!(%err, "cannot remove an uploaded file");
                }
                match self.inspect(location, parent) {
                    Ok(Some(folder)) => self.remember_usage(&folder),
                    Ok(None) => {}
                    Err(err) => tracing::debug!(%err, "cannot refresh free space"),
                }
                return Ok(StoreOutcome::Stored(StoredObject {
                    provider_id: self.id.clone(),
                    key: candidate,
                    size: stored.size,
                }));
            }
            match policy {
                ConflictPolicy::Skip => {
                    fs::remove_file(source)?;
                    return Ok(StoreOutcome::SkippedExisting(candidate));
                }
                ConflictPolicy::KeepBoth if counter < MAX_COUNTER => {
                    counter += 1;
                    candidate = key.with_counter(counter);
                }
                _ => {
                    return Err(StorageError::Remote(format!(
                        "{} already exists on the server",
                        show(&path)
                    )));
                }
            }
        }
    }

    fn exists(&self, key: &StorageKey) -> Result<bool, StorageError> {
        let location = self.location()?;
        self.exists_at(location, &location.path_for(key))
    }

    fn list(&self) -> Result<Vec<StoredFile>, StorageError> {
        let location = self.location()?;
        let root = location.folder.len();
        let mut files = Vec::new();
        let mut level = vec![location.folder.clone()];
        while !level.is_empty() {
            let mut next = Vec::new();
            for (path, folder) in self.list_folders(location, &level)? {
                if path.len() == root {
                    self.remember_usage(&folder);
                }
                for entry in folder.entries.unwrap_or_default() {
                    if entry.name.starts_with('.') {
                        continue;
                    }
                    let mut child = path.clone();
                    child.push(entry.name);
                    match entry.kind.as_str() {
                        "directory" => next.push(child),
                        "file" if is_recording_name(&child[child.len() - 1]) => {
                            let Some(key) = StorageKey::from_components(&child[root..]) else {
                                tracing::debug!(path = %show(&child), "skipping a name a storage key can't hold");
                                continue;
                            };
                            files.push(StoredFile {
                                key,
                                size: entry.size,
                                modified: entry.modified.and_then(|seconds| {
                                    DateTime::<Utc>::from_timestamp(seconds, 0)
                                }),
                            });
                        }
                        _ => {}
                    }
                }
            }
            level = next;
        }
        Ok(files)
    }

    fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        let location = self.location()?;
        let path = location.path_for(key);
        let response = self.delete_at(location, &path)?;
        match response.status().as_u16() {
            204 => {}
            404 => return Err(StorageError::NotFound(key.clone())),
            _ => return Err(status_error(&response, &path)),
        }
        // Remove folders the deletion left empty, but never the recordings folder itself.
        // Without `recursive` the server refuses folders that still hold something (409).
        for depth in (location.folder.len() + 1..path.len()).rev() {
            match self.delete_at(location, &path[..depth]) {
                Ok(response) if response.status() == 204 => {}
                _ => break,
            }
        }
        Ok(())
    }

    fn local_path(&self, _: &StorageKey) -> Option<std::path::PathBuf> {
        None
    }

    fn available_space(&self) -> Option<u64> {
        *self.free_space.lock().unwrap()
    }

    fn check(&self) -> Result<(), StorageError> {
        let result = self.reach_folder();
        if result.is_err() {
            // An old figure next to an error would look current.
            *self.free_space.lock().unwrap() = None;
        }
        result
    }
}

/// Every request opens a new connection: uploads are minutes apart, so a pooled connection
/// would likely be stale by then, and an upload can't safely be repeated.
fn upload_agent() -> Agent {
    agent_config()
        .max_idle_connections(0)
        .timeout_connect(Some(Duration::from_secs(10)))
        // Large files are streamed on to slow cloud backends before the server replies.
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .build()
        .new_agent()
}

/// Keeps connections alive across the burst of `inspect` calls a listing makes.
fn list_agent(timeouts: ListTimeouts) -> Agent {
    agent_config()
        .max_idle_age(Duration::from_secs(5))
        .timeout_connect(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.response))
        .timeout_recv_body(Some(timeouts.body))
        .build()
        .new_agent()
}

fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    Agent::config_builder()
        .http_status_as_error(false)
        // Never send the token anywhere else; none of the calls used here redirect.
        .max_redirects(0)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent(concat!("GetYourMusic/", env!("CARGO_PKG_VERSION")))
}

/// Whether the request failed before anything reached the server.
fn before_request(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Timeout(timeout) => {
            matches!(timeout, ureq::Timeout::Resolve | ureq::Timeout::Connect)
        }
        ureq::Error::Io(err) => matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::NetworkUnreachable
                | io::ErrorKind::HostUnreachable
                | io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
}

fn transport_error(err: ureq::Error) -> StorageError {
    if before_request(&err) {
        StorageError::Unavailable(format!("cannot reach the nekostorage server: {err}"))
    } else {
        StorageError::Remote(format!(
            "connection to the nekostorage server failed: {err}"
        ))
    }
}

fn retry_after(response: &Response<Body>) -> Option<Duration> {
    let seconds = response.headers().get("Retry-After")?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

fn not_an_api_route() -> StorageError {
    StorageError::Remote("the server address isn't a nekostorage api route".into())
}

fn status_error(response: &Response<Body>, path: &[String]) -> StorageError {
    let status = response.status();
    let path = show(path);
    StorageError::Remote(match status.as_u16() {
        400 => format!("the server rejected the name {path}"),
        401 => "nekostorage rejected the token".into(),
        403 => format!("permission denied on the server for {path}"),
        404 => format!("{path} was not found on the server"),
        409 => format!("a file or folder is in the way of {path}"),
        413 => "the file is too large for the server".into(),
        423 => format!("{path} is busy on the server"),
        429 => "the server is rate limited; try again later".into(),
        501 => "the server's storage doesn't support this".into(),
        // nekostorage uses 502 for failures of the storage behind it, not of the route.
        502 => "nekostorage's storage backend failed (502); see the server's log".into(),
        507 => "the server's storage is full".into(),
        _ => format!("nekostorage replied {status}"),
    })
}

fn read_json<T: DeserializeOwned>(response: Response<Body>) -> Result<T, StorageError> {
    let text = response
        .into_body()
        .with_config()
        .limit(JSON_LIMIT)
        .read_to_string()
        .map_err(|err| StorageError::Remote(format!("cannot read the server's reply: {err}")))?;
    serde_json::from_str(&text)
        .map_err(|err| StorageError::Remote(format!("unexpected reply from the server: {err}")))
}

fn content_type(name: &str) -> Option<&'static str> {
    match name.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "flac" => Some("audio/flac"),
        "mp3" => Some("audio/mpeg"),
        "m4a" => Some("audio/mp4"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::testing::FakeNekostorage;

    const INSTANT: RetryPolicy = RetryPolicy {
        attempts: 3,
        backoff: Duration::ZERO,
        max_retry_after: Duration::ZERO,
    };

    fn settings(url: &str, token: &str, folder: &str) -> NekostorageSettings {
        NekostorageSettings {
            url: url.into(),
            token: token.into(),
            folder: folder.into(),
        }
    }

    fn location(url: &str, folder: &str) -> NekostorageLocation {
        NekostorageLocation::parse(&settings(url, "", folder)).unwrap()
    }

    fn key(path: &str) -> StorageKey {
        StorageKey::from_components(path.split('/')).unwrap()
    }

    fn storage(server: &FakeNekostorage, token: &str) -> NekostorageStorage {
        NekostorageStorage::new(&settings(server.url(), token, "/Music")).with_retry_policy(INSTANT)
    }

    fn source(dir: &Path, contents: &[u8]) -> PathBuf {
        let path = dir.join(format!("{}.flac", uuid::Uuid::new_v4().simple()));
        fs::write(&path, contents).unwrap();
        path
    }

    fn stored(outcome: StoreOutcome) -> StoredObject {
        match outcome {
            StoreOutcome::Stored(object) => object,
            other => panic!("not stored: {other:?}"),
        }
    }

    #[test]
    fn ids_identify_server_and_folder() {
        let id = location("http://h/api", "/Music").id();
        assert_eq!(id, "nekostorage:http://h/api#/Music");
        for (url, folder) in [
            ("http://H:80/api/", "/Music"),
            ("HTTP://h/api", "Music/"),
            (" http://h/api ", "//Music"),
        ] {
            assert_eq!(location(url, folder).id(), id, "{url} {folder}");
        }
        assert_ne!(location("http://h/api", "/Other").id(), id);
        assert_ne!(location("http://h:8080/api", "/Music").id(), id);
        assert_ne!(location("https://h/api", "/Music").id(), id);

        for (url, folder) in [("http://h/api", "/"), ("https://[::1]:8443", "/音乐/a b")] {
            let original = location(url, folder);
            assert_eq!(NekostorageLocation::from_id(&original.id()), Some(original));
        }
        assert_eq!(location("https://h:8443/api", "/").display(), "h:8443/api");

        assert!(location("http://nas.lan:8080/api", "/").is_unencrypted());
        for url in [
            "https://nas.lan/api",
            "http://127.0.0.1:8080/api",
            "http://[::1]/api",
            "http://localhost/api",
        ] {
            assert!(!location(url, "/").is_unencrypted(), "{url}");
        }
        assert_eq!(location("http://h/api/", "/a/b").display(), "h/api/a/b");
    }

    #[test]
    fn rejects_bad_settings() {
        for (url, folder) in [
            ("", "/"),
            ("not a url", "/"),
            ("ftp://h/api", "/"),
            ("http://h/api?x=1", "/"),
            ("http://h/api#x", "/"),
            ("http://user:pass@h/api", "/"),
            ("http://h/api", "/a/../b"),
        ] {
            assert!(
                NekostorageStorage::validate(&settings(url, "", folder)).is_err(),
                "{url} {folder}"
            );
        }
        for id in [
            "",
            "local",
            "nekostorage:",
            "nekostorage:http://h/api",
            "nekostorage:x#/",
        ] {
            assert!(NekostorageLocation::from_id(id).is_none(), "{id}");
        }
    }

    #[test]
    fn invalid_settings_fail_on_use() {
        let dir = tempfile::tempdir().unwrap();
        let storage = NekostorageStorage::new(&settings("ftp://h", "", "/"));
        assert_eq!(storage.id(), "nekostorage:invalid");
        assert_eq!(storage.display_location(), "ftp://h");
        let file = source(dir.path(), b"x");
        let result = storage.store(&key("a.flac"), &file, ConflictPolicy::KeepBoth);
        assert!(matches!(result, Err(StorageError::Unavailable(_))));
        assert!(file.exists());
    }

    #[test]
    fn encodes_path_segments() {
        let location = location("http://h/api", "/");
        let path = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(location.url("inspect", &[]), "http://h/api/inspect");
        assert_eq!(
            location.url("inspect", &path(&["a b", "猫"])),
            "http://h/api/inspect/a%20b/%E7%8C%AB"
        );
        assert_eq!(
            location.url("upload", &path(&["+#?%&=", "a-b_c.~d"])),
            "http://h/api/upload/%2B%23%3F%25%26%3D/a-b_c.~d"
        );
    }

    #[test]
    fn stores_into_new_folders() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(Some("secret"));
        let storage = storage(&server, "secret");
        let file = source(dir.path(), b"flac bytes");
        let object = stored(
            storage
                .store(
                    &key("Artist/Album/Song.flac"),
                    &file,
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(object.provider_id, location(server.url(), "/Music").id());
        assert_eq!(object.key, key("Artist/Album/Song.flac"));
        assert_eq!(object.size, 10);
        assert_eq!(
            server.file("/Music/Artist/Album/Song.flac").as_deref(),
            Some(&b"flac bytes"[..])
        );
        assert!(!file.exists(), "the provider owns the data after storing");
        assert_eq!(storage.available_space(), Some(6_436_315_136));
    }

    #[test]
    fn honours_conflict_policies() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        let song = key("A/Song.flac");
        let store = |contents: &[u8], policy| {
            storage
                .store(&song, &source(dir.path(), contents), policy)
                .unwrap()
        };

        stored(store(b"one", ConflictPolicy::KeepBoth));
        assert_eq!(
            stored(store(b"two", ConflictPolicy::KeepBoth)).key,
            key("A/Song (2).flac")
        );
        assert_eq!(
            stored(store(b"three", ConflictPolicy::KeepBoth)).key,
            key("A/Song (3).flac")
        );

        let file = source(dir.path(), b"skipped");
        let outcome = storage.store(&song, &file, ConflictPolicy::Skip).unwrap();
        assert_eq!(outcome, StoreOutcome::SkippedExisting(song.clone()));
        assert!(!file.exists());
        assert_eq!(
            server.file("/Music/A/Song.flac").as_deref(),
            Some(&b"one"[..])
        );

        stored(store(b"replaced", ConflictPolicy::Overwrite));
        assert_eq!(
            server.file("/Music/A/Song.flac").as_deref(),
            Some(&b"replaced"[..])
        );
    }

    #[test]
    fn losing_a_race_for_the_name_moves_on() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        // Free when checked, taken by the time the upload arrives.
        server.queue_upload_replies(&[412]);
        let object = stored(
            storage
                .store(
                    &key("Song.flac"),
                    &source(dir.path(), b"x"),
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(object.key, key("Song (2).flac"));
        assert_eq!(server.uploads(), 2);
    }

    #[test]
    fn a_wrong_token_keeps_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(Some("secret"));
        let storage = storage(&server, "wrong");
        let file = source(dir.path(), b"x");
        let err = storage
            .store(&key("a.flac"), &file, ConflictPolicy::KeepBoth)
            .unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");
        assert!(file.exists());
        assert!(storage.check().unwrap_err().to_string().contains("token"));
    }

    #[test]
    fn retries_only_what_the_server_did_not_act_on() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        let upload = |name: &str| {
            storage.store(
                &key(name),
                &source(dir.path(), b"x"),
                ConflictPolicy::KeepBoth,
            )
        };

        server.queue_upload_replies(&[503, 429]);
        stored(upload("a.flac").unwrap());
        assert_eq!(server.uploads(), 3);

        server.queue_upload_replies(&[503, 503, 503]);
        assert!(upload("b.flac").is_err());
        assert_eq!(server.uploads(), 6, "gives up after three attempts");

        server.queue_upload_replies(&[502]);
        let err = upload("c.flac").unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
        assert_eq!(server.uploads(), 7, "a 502 may follow a partial write");
    }

    #[test]
    fn does_not_retry_an_interrupted_upload() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        {
            let connections = Arc::clone(&connections);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    let _ = reader.read_line(&mut request_line);
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                        line.clear();
                    }
                    if request_line.starts_with("HEAD") {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        continue;
                    }
                    connections.fetch_add(1, Ordering::SeqCst);
                    let _ = reader.read_exact(&mut [0; 1000]);
                    // Dropping the stream with unread data resets the connection.
                }
            });
        }
        let storage =
            NekostorageStorage::new(&settings(&format!("http://127.0.0.1:{port}/api"), "", "/"))
                .with_retry_policy(INSTANT);
        let file = source(dir.path(), &vec![0; 4 * 1024 * 1024]);
        let result = storage.store(&key("a.flac"), &file, ConflictPolicy::KeepBoth);
        assert!(matches!(result, Err(StorageError::Remote(_))), "{result:?}");
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert!(file.exists());
    }

    #[test]
    fn reports_existence_and_free_space() {
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        assert_eq!(storage.available_space(), None, "unknown until checked");
        // `/Music` doesn't exist yet, so the root answers.
        storage.check().unwrap();
        assert_eq!(storage.available_space(), Some(6_436_315_136));

        server.add_file("/Music/a.flac", b"x");
        assert!(storage.exists(&key("a.flac")).unwrap());
        assert!(!storage.exists(&key("b.flac")).unwrap());

        server.set_free(None);
        storage.check().unwrap();
        assert_eq!(storage.available_space(), None);

        server.add_file("/Elsewhere", b"x");
        let file_folder = NekostorageStorage::new(&settings(server.url(), "", "/Elsewhere"));
        assert!(
            file_folder
                .check()
                .unwrap_err()
                .to_string()
                .contains("is a file")
        );
    }

    #[test]
    fn lists_recordings_at_any_depth() {
        let server = FakeNekostorage::start(Some("secret"));
        for dir in ["/Music/A/B", "/Music/猫", "/Other"] {
            server.add_dir(dir);
        }
        for (path, size) in [
            ("/Music/A/B/x.flac", 3),
            ("/Music/A/y.MP3", 4),
            ("/Music/z.m4a", 5),
            ("/Music/猫/a b+c.flac", 6),
            ("/Music/notes.txt", 1),
            ("/Music/.hidden.flac", 1),
            ("/Other/w.flac", 1),
        ] {
            server.add_file(path, &vec![0; size]);
        }
        let storage = storage(&server, "secret");
        let mut files = storage.list().unwrap();
        files.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        let listed: Vec<_> = files.iter().map(|f| (f.key.as_str(), f.size)).collect();
        assert_eq!(
            listed,
            [
                ("A/B/x.flac", 3),
                ("A/y.MP3", 4),
                ("z.m4a", 5),
                ("猫/a b+c.flac", 6)
            ]
        );
        assert!(files.iter().all(|f| f.modified.is_some()));
        assert_eq!(storage.available_space(), Some(6_436_315_136));
    }

    #[test]
    fn a_listing_fails_as_a_whole() {
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        assert!(matches!(storage.list(), Err(StorageError::Missing(_))));

        server.add_dir("/Music/A");
        server.add_dir("/Music/B");
        server.add_file("/Music/B/b.flac", b"x");
        assert_eq!(storage.list().unwrap().len(), 1);
        server.break_inspect("/Music/A");
        let err = storage.list().unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }

    #[test]
    fn a_folder_reply_without_entries_fails_the_listing() {
        let server = FakeNekostorage::start(None);
        server.add_dir("/Music/A");
        server.add_file("/Music/A/a.flac", b"x");
        server.omit_entries("/Music/A");
        let err = storage(&server, "").list().unwrap_err();
        assert!(err.to_string().contains("without entries"), "{err}");
    }

    #[test]
    fn folders_that_vanish_during_a_listing_are_skipped() {
        let server = FakeNekostorage::start(None);
        for dir in ["/Music/Gone", "/Music/Kept"] {
            server.add_dir(dir);
        }
        server.add_file("/Music/Gone/g.flac", b"x");
        server.add_file("/Music/Kept/k.flac", b"x");
        server.hide_from_inspect("/Music/Gone");
        let listed: Vec<_> = storage(&server, "")
            .list()
            .unwrap()
            .into_iter()
            .map(|f| f.key.to_string())
            .collect();
        assert_eq!(listed, ["Kept/k.flac"]);

        server.hide_from_inspect("/Music");
        assert!(matches!(
            storage(&server, "").list(),
            Err(StorageError::Missing(_))
        ));
    }

    #[test]
    fn a_stalled_server_ends_the_listing_quickly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Accept and never answer, keeping the connections open.
            let mut open = Vec::new();
            for stream in listener.incoming() {
                open.extend(stream.ok());
            }
        });
        let timeout = Duration::from_millis(200);
        let storage =
            NekostorageStorage::new(&settings(&format!("http://127.0.0.1:{port}/api"), "", "/"))
                .with_retry_policy(INSTANT)
                .with_list_timeouts(ListTimeouts {
                    connect: timeout,
                    response: timeout,
                    body: timeout,
                });
        let started = std::time::Instant::now();
        assert!(storage.list().is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_upload_whose_folder_vanished_recreates_it() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        server.queue_upload_replies(&[404]);
        stored(
            storage
                .store(
                    &key("A/B/x.flac"),
                    &source(dir.path(), b"x"),
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(server.uploads(), 2);
        assert_eq!(server.files(), ["/Music/A/B/x.flac"]);
    }

    #[test]
    fn deletes_and_removes_emptied_folders() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        for name in ["A/B/x.flac", "A/y.flac"] {
            stored(
                storage
                    .store(
                        &key(name),
                        &source(dir.path(), b"x"),
                        ConflictPolicy::KeepBoth,
                    )
                    .unwrap(),
            );
        }

        storage.delete(&key("A/B/x.flac")).unwrap();
        assert_eq!(server.files(), ["/Music/A/y.flac"]);
        assert_eq!(
            server.dirs(),
            ["/", "/Music", "/Music/A"],
            "the emptied folder goes"
        );

        storage.delete(&key("A/y.flac")).unwrap();
        assert!(server.files().is_empty());
        assert_eq!(
            server.dirs(),
            ["/", "/Music"],
            "the recordings folder itself stays"
        );
        assert!(matches!(
            storage.delete(&key("A/y.flac")),
            Err(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn unusual_names_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        let name = "周杰伦/叶惠美/a b+c#?%&.flac";
        stored(
            storage
                .store(
                    &key(name),
                    &source(dir.path(), b"x"),
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(server.files(), [format!("/Music/{name}")]);
    }

    #[test]
    fn never_follows_redirects() {
        let server = FakeNekostorage::start(None);
        let storage = storage(&server, "");
        storage.check().unwrap();
        assert!(storage.available_space().is_some());
        server.redirect_everything();
        let err = storage.check().unwrap_err();
        assert!(err.to_string().contains("307"), "{err}");
        assert_eq!(
            storage.available_space(),
            None,
            "a failed check forgets old space"
        );
    }

    #[test]
    fn a_wrong_route_is_reported() {
        let server = FakeNekostorage::start(None);
        let wrong = format!("{}/v2", server.url().trim_end_matches("/api"));
        let storage =
            NekostorageStorage::new(&settings(&wrong, "", "/")).with_retry_policy(INSTANT);
        let err = storage.check().unwrap_err();
        assert!(err.to_string().contains("api route"), "{err}");
    }

    /// Run with `GYM_NEKOSTORAGE_URL=… GYM_NEKOSTORAGE_TOKEN=… cargo test -p gym-core -- --ignored`.
    ///
    /// Works in a `gym-test-…` folder below `GYM_NEKOSTORAGE_BASE` (a mount such as `/198`)
    /// and removes it again.
    #[test]
    #[ignore = "needs a nekostorage server in GYM_NEKOSTORAGE_URL"]
    fn live_server() {
        let url = std::env::var("GYM_NEKOSTORAGE_URL").expect("GYM_NEKOSTORAGE_URL");
        let token = std::env::var("GYM_NEKOSTORAGE_TOKEN").unwrap_or_default();
        let base = std::env::var("GYM_NEKOSTORAGE_BASE").unwrap_or_default();
        let name = format!(
            "gym-test-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let folder = format!("{}/{name}", base.trim_end_matches('/'));
        let storage = NekostorageStorage::new(&settings(&url, &token, &folder));
        storage.check().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let song = key("Artist/Album/Song 猫.flac");
        let first = stored(
            storage
                .store(
                    &song,
                    &source(dir.path(), b"first"),
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(first.key, song);
        assert_eq!(first.size, 5);
        let second = stored(
            storage
                .store(
                    &song,
                    &source(dir.path(), b"second"),
                    ConflictPolicy::KeepBoth,
                )
                .unwrap(),
        );
        assert_eq!(second.key, key("Artist/Album/Song 猫 (2).flac"));
        let skipped = storage
            .store(&song, &source(dir.path(), b"third"), ConflictPolicy::Skip)
            .unwrap();
        assert_eq!(skipped, StoreOutcome::SkippedExisting(song.clone()));
        assert!(storage.exists(&song).unwrap());
        assert!(!storage.exists(&key("Artist/Album/missing.flac")).unwrap());
        storage.check().unwrap();

        let mut listed: Vec<_> = storage
            .list()
            .unwrap()
            .into_iter()
            .map(|f| (f.key.to_string(), f.size))
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("Artist/Album/Song 猫 (2).flac".to_owned(), 6),
                ("Artist/Album/Song 猫.flac".to_owned(), 5),
            ]
        );
        println!(
            "listed {} files under {folder} on {}; free space: {:?}",
            listed.len(),
            storage.display_location(),
            storage.available_space()
        );

        storage.delete(&song).unwrap();
        storage.delete(&second.key).unwrap();
        assert!(matches!(
            storage.delete(&song),
            Err(StorageError::NotFound(_))
        ));
        assert!(
            storage.list().unwrap().is_empty(),
            "emptied folders are gone"
        );

        // Remove the test folder itself through its parent.
        let parent = NekostorageStorage::new(&settings(&url, &token, &base));
        parent.delete(&key(&name)).unwrap();
        assert!(matches!(storage.list(), Err(StorageError::Missing(_))));
    }
}
