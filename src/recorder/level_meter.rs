//! Live stereo peak meter, refreshed at display rate only while recording.

use std::time::{Duration, Instant};

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AsyncApp, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Task, WeakEntity, Window, div, relative,
};

use gym_core::engine::to_dbfs;

use crate::session::RecordingSession;

const FRAME: Duration = Duration::from_millis(33);
const FLOOR_DB: f32 = -60.0;
const RELEASE_DB_PER_SEC: f32 = 24.0;
const PEAK_HOLD: Duration = Duration::from_millis(1500);
/// Levels above this are close to clipping.
const HOT_DB: f32 = -3.0;
const LOUD_DB: f32 = -12.0;

#[derive(Clone, Copy)]
struct Channel {
    level_db: f32,
    peak_db: f32,
    peak_at: Instant,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            level_db: FLOOR_DB,
            peak_db: FLOOR_DB,
            peak_at: Instant::now(),
        }
    }
}

impl Channel {
    /// Instant attack, linear release in dB, and a held peak marker.
    fn update(&mut self, peak_linear: f32, elapsed: Duration) {
        let db = to_dbfs(peak_linear).max(FLOOR_DB);
        let released = self.level_db - RELEASE_DB_PER_SEC * elapsed.as_secs_f32();
        self.level_db = db.max(released).max(FLOOR_DB);
        if db >= self.peak_db || self.peak_at.elapsed() > PEAK_HOLD {
            self.peak_db = db;
            self.peak_at = Instant::now();
        }
    }
}

fn fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

pub struct LevelMeterView {
    session: Entity<RecordingSession>,
    channels: [Channel; 2],
    last_frame: Instant,
    ticker: Option<Task<()>>,
    _observer: Subscription,
}

impl LevelMeterView {
    pub fn new(session: Entity<RecordingSession>, cx: &mut Context<Self>) -> Self {
        let observer = cx.observe(&session, |this, session, cx| {
            let recording = session.read(cx).meter().is_some();
            match (recording, this.ticker.is_some()) {
                (true, false) => this.start_ticker(cx),
                (false, true) => {
                    this.ticker = None;
                    this.channels = Default::default();
                    cx.notify();
                }
                _ => {}
            }
        });
        Self {
            session,
            channels: Default::default(),
            last_frame: Instant::now(),
            ticker: None,
            _observer: observer,
        }
    }

    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        self.last_frame = Instant::now();
        self.ticker = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor().timer(FRAME).await;
                    let alive = this.update(cx, |meter, cx| meter.refresh(cx));
                    if alive.is_err() {
                        break;
                    }
                }
            }),
        );
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(meter) = self.session.read(cx).meter() else {
            return;
        };
        let levels = meter.take();
        let elapsed = self.last_frame.elapsed();
        self.last_frame = Instant::now();
        for (channel, peak) in self.channels.iter_mut().zip(levels.peak) {
            channel.update(peak, elapsed);
        }
        cx.notify();
    }

    fn zone_color(&self, db: f32, cx: &Context<Self>) -> Hsla {
        if db >= HOT_DB {
            cx.theme().danger
        } else if db >= LOUD_DB {
            cx.theme().warning
        } else {
            cx.theme().success
        }
    }

    fn render_channel(
        &self,
        label: &'static str,
        channel: Channel,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = self.ticker.is_some();
        h_flex()
            .gap_2()
            .child(
                div()
                    .w_3()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h_1p5()
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .when(active, |track| {
                        track
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .rounded_full()
                                    .w(relative(fraction(channel.level_db)))
                                    .bg(self.zone_color(channel.level_db, cx)),
                            )
                            .when(channel.peak_db > FLOOR_DB, |track| {
                                track.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .h_full()
                                        .w_0p5()
                                        .left(relative(fraction(channel.peak_db)))
                                        .bg(self.zone_color(channel.peak_db, cx)),
                                )
                            })
                    }),
            )
    }
}

impl Render for LevelMeterView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1p5()
            .child(self.render_channel("L", self.channels[0], cx))
            .child(self.render_channel("R", self.channels[1], cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_is_gradual_and_attack_is_instant() {
        let mut channel = Channel::default();
        channel.update(1.0, FRAME);
        assert_eq!(channel.level_db, 0.0);
        channel.update(0.0, Duration::from_millis(500));
        assert!((channel.level_db + 12.0).abs() < 0.01);
        assert_eq!(channel.peak_db, 0.0, "peak is held");
    }

    #[test]
    fn fractions_cover_the_meter_range() {
        assert_eq!(fraction(-90.0), 0.0);
        assert_eq!(fraction(0.0), 1.0);
        assert!((fraction(-30.0) - 0.5).abs() < 1e-6);
    }
}
