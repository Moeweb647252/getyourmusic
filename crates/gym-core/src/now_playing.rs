//! Abstraction over the platform's "now playing" information and a fan-out monitor.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::model::{NowPlaying, PlayerInfo};

#[derive(Debug, thiserror::Error)]
pub enum NowPlayingError {
    #[error("now playing information is not available on this system: {0}")]
    Unavailable(String),
    #[error("now playing helper failed: {0}")]
    Helper(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// Snapshots arrive a few times per track; boxing them would only add allocations.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum NowPlayingEvent {
    /// A new or changed snapshot.
    Updated(NowPlaying),
    /// Nothing is playing anymore.
    Cleared,
    /// A recoverable problem; the source keeps trying.
    SourceError(String),
}

/// Keeps a source running while alive. Dropping it stops the source.
pub struct Subscription(#[allow(dead_code)] Mutex<Box<dyn Any + Send>>);

impl Subscription {
    pub fn new(guard: impl Any + Send) -> Self {
        Self(Mutex::new(Box::new(guard)))
    }
}

/// A platform implementation that reports what the system is playing.
pub trait NowPlayingSource: Send + Sync {
    /// Starts delivering events to `sink` until the returned subscription is dropped.
    fn start(&self, sink: Sender<NowPlayingEvent>) -> Result<Subscription, NowPlayingError>;
}

struct MonitorState {
    current: Option<NowPlaying>,
    players: Vec<PlayerInfo>,
    subscribers: Vec<Sender<NowPlayingEvent>>,
}

/// Runs a [`NowPlayingSource`] once and shares its events with any number of subscribers.
#[derive(Clone)]
pub struct NowPlayingMonitor {
    state: Arc<Mutex<MonitorState>>,
    subscription: Arc<Mutex<Option<Subscription>>>,
}

impl NowPlayingMonitor {
    pub fn start(source: &dyn NowPlayingSource) -> Result<Self, NowPlayingError> {
        let (tx, rx) = unbounded();
        let subscription = source.start(tx)?;
        let state = Arc::new(Mutex::new(MonitorState {
            current: None,
            players: Vec::new(),
            subscribers: Vec::new(),
        }));
        let dispatch_state = Arc::clone(&state);
        thread::Builder::new()
            .name("now-playing-dispatch".into())
            .spawn(move || Self::dispatch(rx, dispatch_state))?;
        Ok(Self {
            state,
            subscription: Arc::new(Mutex::new(Some(subscription))),
        })
    }

    fn dispatch(rx: Receiver<NowPlayingEvent>, state: Arc<Mutex<MonitorState>>) {
        for event in rx {
            let mut state = state.lock().unwrap();
            match &event {
                NowPlayingEvent::Updated(np) => {
                    if !state.players.iter().any(|p| p.id == np.player.id) {
                        state.players.push(np.player.clone());
                    }
                    state.current = Some(np.clone());
                }
                NowPlayingEvent::Cleared => state.current = None,
                NowPlayingEvent::SourceError(message) => {
                    tracing::warn!(%message, "now playing source error");
                }
            }
            state
                .subscribers
                .retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
    }

    /// Stops the source (e.g. its helper process) for every clone of this monitor.
    ///
    /// Call before the process exits: exiting does not run destructors.
    pub fn shutdown(&self) {
        let subscription = self.subscription.lock().unwrap().take();
        drop(subscription);
    }

    /// The latest snapshot, if anything is playing.
    pub fn current(&self) -> Option<NowPlaying> {
        self.state.lock().unwrap().current.clone()
    }

    /// Players seen since the monitor started, in order of appearance.
    pub fn known_players(&self) -> Vec<PlayerInfo> {
        self.state.lock().unwrap().players.clone()
    }

    /// Subscribes to future events. The current snapshot, if any, is delivered first.
    pub fn subscribe(&self) -> Receiver<NowPlayingEvent> {
        let (tx, rx) = unbounded();
        let mut state = self.state.lock().unwrap();
        if let Some(current) = &state.current {
            let _ = tx.send(NowPlayingEvent::Updated(current.clone()));
        }
        state.subscribers.push(tx);
        rx
    }
}
