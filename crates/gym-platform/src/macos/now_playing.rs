//! System-wide "now playing" information through the MediaRemote adapter.
//!
//! Since macOS 15.4 only Apple-signed processes may read MediaRemote. The vendored adapter is
//! loaded into `/usr/bin/perl`, which is entitled, and streams JSON lines on stdout.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use crossbeam_channel::Sender;
use serde_json::{Map, Value};

use gym_core::model::{Artwork, NowPlaying, PlayerInfo, TrackId, TrackMetadata};
use gym_core::now_playing::{NowPlayingError, NowPlayingEvent, NowPlayingSource, Subscription};

const PERL: &str = "/usr/bin/perl";
const FRAMEWORK_NAME: &str = "MediaRemoteAdapter.framework";
const SCRIPT_NAME: &str = "mediaremote-adapter.pl";

pub struct AdapterNowPlaying {
    script: PathBuf,
    framework: PathBuf,
}

impl AdapterNowPlaying {
    /// Finds the adapter: an explicit override, the app bundle, or the build output.
    pub fn locate() -> Result<Self, NowPlayingError> {
        let candidates = [
            std::env::var_os("GYM_MEDIAREMOTE_FRAMEWORK")
                .zip(std::env::var_os("GYM_MEDIAREMOTE_SCRIPT"))
                .map(|(f, s)| (PathBuf::from(s), PathBuf::from(f))),
            std::env::current_exe().ok().and_then(|exe| {
                let contents = exe.parent()?.parent()?;
                Some((
                    contents.join("Resources").join(SCRIPT_NAME),
                    contents.join("Frameworks").join(FRAMEWORK_NAME),
                ))
            }),
            Some((
                PathBuf::from(env!("GYM_MEDIAREMOTE_SCRIPT")),
                PathBuf::from(env!("GYM_MEDIAREMOTE_FRAMEWORK")),
            )),
        ];
        candidates
            .into_iter()
            .flatten()
            .find(|(script, framework)| script.is_file() && framework.is_dir())
            .map(|(script, framework)| Self { script, framework })
            .ok_or_else(|| NowPlayingError::Unavailable("MediaRemote adapter not found".into()))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(PERL);
        command.arg(&self.script).arg(&self.framework);
        command
    }

    /// Runs the adapter's own entitlement check.
    fn self_test(&self) -> Result<(), NowPlayingError> {
        let mut child = self
            .command()
            .arg("test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait()? {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(NowPlayingError::Unavailable(format!(
                    "adapter self-test failed ({status}): {}",
                    stderr.trim()
                )));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err(NowPlayingError::Unavailable(
                    "adapter self-test timed out".into(),
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct StreamGuard {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    supervisor: Option<JoinHandle<()>>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
    }
}

impl NowPlayingSource for AdapterNowPlaying {
    fn start(&self, sink: Sender<NowPlayingEvent>) -> Result<Subscription, NowPlayingError> {
        self.self_test()?;
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let mut command = self.command();
        command
            .args(["stream", "--micros", "--debounce=50"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let supervisor = {
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            thread::Builder::new()
                .name("now-playing-adapter".into())
                .spawn(move || supervise(command, stop, child, sink))?
        };
        Ok(Subscription::new(StreamGuard {
            stop,
            child,
            supervisor: Some(supervisor),
        }))
    }
}

/// Keeps the streaming helper alive, restarting it with backoff when it exits.
fn supervise(
    mut command: Command,
    stop: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    sink: Sender<NowPlayingEvent>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut parser = PayloadParser::new(resolve_player_name);
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = sink.send(NowPlayingEvent::SourceError(err.to_string()));
                sleep_unless_stopped(&stop, backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        *slot.lock().unwrap() = Some(child);
        let stderr_reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            text
        });

        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(event) = parser.apply_line(&line, SystemTime::now())
                && sink.send(event).is_err()
            {
                stop.store(true, Ordering::SeqCst);
                break;
            }
        }

        if let Some(mut child) = slot.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let stderr = stderr_reader.join().unwrap_or_default();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        tracing::warn!(stderr = %stderr.trim(), "now playing helper exited; restarting");
        let _ = sink.send(NowPlayingEvent::SourceError(
            "now playing helper exited".into(),
        ));
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        sleep_unless_stopped(&stop, backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Looks up a player's display name from its process or bundle identifier.
fn resolve_player_name(bundle_id: &str, pid: Option<i32>) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;

    let app = pid
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .filter(|app| {
            app.bundleIdentifier()
                .is_some_and(|id| id.to_string() == bundle_id)
        })
        .or_else(|| {
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
                bundle_id,
            ))
            .firstObject()
        })?;
    app.localizedName().map(|name| name.to_string())
}

/// Accumulates the adapter's full and diff payloads into snapshots.
struct PayloadParser {
    fields: Map<String, Value>,
    artwork: Option<(String, Artwork)>,
    names: HashMap<String, String>,
    resolve_name: fn(&str, Option<i32>) -> Option<String>,
}

impl PayloadParser {
    fn new(resolve_name: fn(&str, Option<i32>) -> Option<String>) -> Self {
        Self {
            fields: Map::new(),
            artwork: None,
            names: HashMap::new(),
            resolve_name,
        }
    }

    fn apply_line(&mut self, line: &str, received_at: SystemTime) -> Option<NowPlayingEvent> {
        let message: Value = serde_json::from_str(line).ok()?;
        if message.get("type")?.as_str()? != "data" {
            return None;
        }
        let payload = message.get("payload")?.as_object()?;
        if message
            .get("diff")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            for (key, value) in payload {
                if value.is_null() {
                    self.fields.remove(key);
                } else {
                    self.fields.insert(key.clone(), value.clone());
                }
            }
        } else {
            self.fields = payload.clone();
        }
        Some(self.snapshot(received_at))
    }

    fn text(&self, key: &str) -> Option<String> {
        self.fields
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    fn number(&self, key: &str) -> Option<f64> {
        self.fields.get(key).and_then(Value::as_f64)
    }

    fn micros(&self, key: &str) -> Option<Duration> {
        self.number(key)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| Duration::from_micros(v as u64))
    }

    fn artwork(&mut self) -> Option<Artwork> {
        let data = self.text("artworkData")?;
        if let Some((cached, artwork)) = &self.artwork
            && *cached == data
        {
            return Some(artwork.clone());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .ok()?;
        let artwork = Artwork {
            mime_type: self
                .text("artworkMimeType")
                .unwrap_or_else(|| "image/jpeg".into()),
            data: Arc::from(bytes),
        };
        self.artwork = Some((data, artwork.clone()));
        Some(artwork)
    }

    fn player(&mut self) -> Option<PlayerInfo> {
        // Browser tabs report helper processes; the parent application is the real player.
        let bundle_id = self
            .text("parentApplicationBundleIdentifier")
            .or_else(|| self.text("bundleIdentifier"))?;
        let pid = self.number("processIdentifier").map(|p| p as i32);
        let resolve = self.resolve_name;
        let name = self
            .names
            .entry(bundle_id.clone())
            .or_insert_with(|| resolve(&bundle_id, pid).unwrap_or_else(|| bundle_id.clone()))
            .clone();
        Some(PlayerInfo::new(bundle_id, name))
    }

    fn snapshot(&mut self, received_at: SystemTime) -> NowPlayingEvent {
        let Some(title) = self.text("title") else {
            return NowPlayingEvent::Cleared;
        };
        let Some(player) = self.player() else {
            return NowPlayingEvent::Cleared;
        };
        let track = TrackMetadata {
            title,
            artist: self.text("artist"),
            album: self.text("album"),
            album_artist: None,
            genre: self.text("genre"),
            composer: self.text("composer"),
            track_number: self
                .number("trackNumber")
                .map(|n| n as u32)
                .filter(|n| *n > 0),
            track_total: self
                .number("totalTrackCount")
                .map(|n| n as u32)
                .filter(|n| *n > 0),
            duration: self.micros("durationMicros").filter(|d| !d.is_zero()),
            artwork: self.artwork(),
        };
        let playing = self
            .fields
            .get("playing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let rate = self
            .number("playbackRate")
            .filter(|r| *r > 0.0)
            .unwrap_or(1.0);
        let measured_at = self
            .number("timestampEpochMicros")
            .map(|us| SystemTime::UNIX_EPOCH + Duration::from_micros(us as u64))
            .unwrap_or(received_at);
        let track_id = TrackId::from_metadata(&player, &track);
        NowPlayingEvent::Updated(NowPlaying {
            player,
            track_id,
            track,
            playing,
            rate,
            elapsed: self.micros("elapsedTimeMicros").unwrap_or_default(),
            measured_at,
            received_at,
            is_advertisement: self
                .fields
                .get("isAdvertisement")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> PayloadParser {
        PayloadParser::new(|id, _| Some(format!("Name of {id}")))
    }

    fn updated(event: Option<NowPlayingEvent>) -> NowPlaying {
        match event {
            Some(NowPlayingEvent::Updated(np)) => np,
            other => panic!("expected update, got {other:?}"),
        }
    }

    const FULL: &str = r#"{"type":"data","diff":false,"payload":{"bundleIdentifier":"com.tencent.QQMusicMac","processIdentifier":42,"title":"晴天","artist":"周杰伦","album":"叶惠美","durationMicros":269000000,"elapsedTimeMicros":1500000,"timestampEpochMicros":1759700000000000,"playing":true,"playbackRate":1,"artworkMimeType":"image/png","artworkData":"iVBORw0KGgo="}}"#;

    #[test]
    fn parses_full_payloads() {
        let mut p = parser();
        let np = updated(p.apply_line(FULL, SystemTime::now()));
        assert_eq!(np.player.id, "com.tencent.QQMusicMac");
        assert_eq!(np.player.name, "Name of com.tencent.QQMusicMac");
        assert_eq!(np.track.title, "晴天");
        assert_eq!(np.track.artist.as_deref(), Some("周杰伦"));
        assert_eq!(np.track.duration, Some(Duration::from_secs(269)));
        assert_eq!(np.elapsed, Duration::from_millis(1500));
        assert!(np.playing);
        assert_eq!(
            np.measured_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_759_700_000)
        );
        let art = np.track.artwork.unwrap();
        assert_eq!(art.mime_type, "image/png");
        assert_eq!(&art.data[..4], b"\x89PNG");
    }

    #[test]
    fn merges_diffs_and_removes_nulls() {
        let mut p = parser();
        p.apply_line(FULL, SystemTime::now());
        let np = updated(p.apply_line(
            r#"{"type":"data","diff":true,"payload":{"playing":false,"playbackRate":0,"album":null}}"#,
            SystemTime::now(),
        ));
        assert!(!np.playing);
        assert_eq!(np.rate, 1.0);
        assert_eq!(np.track.album, None);
        assert_eq!(np.track.title, "晴天");
    }

    #[test]
    fn empty_payload_clears() {
        let mut p = parser();
        p.apply_line(FULL, SystemTime::now());
        let event = p.apply_line(
            r#"{"type":"data","diff":false,"payload":{}}"#,
            SystemTime::now(),
        );
        assert!(matches!(event, Some(NowPlayingEvent::Cleared)));
        assert!(p.apply_line("not json", SystemTime::now()).is_none());
    }

    #[test]
    fn browser_tabs_use_the_parent_application() {
        let mut p = parser();
        let np = updated(p.apply_line(
            r#"{"type":"data","diff":false,"payload":{"bundleIdentifier":"com.google.Chrome.helper","parentApplicationBundleIdentifier":"com.google.Chrome","title":"Video","playing":true}}"#,
            SystemTime::now(),
        ));
        assert_eq!(np.player.id, "com.google.Chrome");
    }
}
