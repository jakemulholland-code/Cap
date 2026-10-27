//! The section recorder: the slide plays while the script scrolls, each take
//! records the camera and mic, and a take plays back over its slide. When
//! every section has a take, the project is built and opened in the editor.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use cap_audio::AudioData;
use cap_recording::sources::screen_capture::ScreenCaptureTarget;
use gpui::{AnyElement, Task};

use super::*;
use crate::guided_build;
use crate::guided_listen;
use crate::guided_media::{self, Pace, Player, PlayerVideo, SlideClock};
use crate::guided_prompt::{self, PromptWord};
use crate::guided_recording::{SegmentPlan, Take};
use crate::recording::{self, ActiveRecording, RecordingMode, StartConfig};

const COUNTDOWN_SECS: f64 = 3.0;
const TICK: Duration = Duration::from_millis(16);
const PREVIEW_MAX_WIDTH: u32 = 960;
const CAMERA_MAX_WIDTH: u32 = 480;
const PROMPT_TEXT_SIZE: f32 = 22.;
const PROMPT_WIDTH: f32 = 620.;
const PROMPT_BEFORE: usize = 1;
const PROMPT_AFTER: usize = 3;
const WPM_STEP: f64 = 10.0;
const MIN_WPM: f64 = 80.0;
const MAX_WPM: f64 = 220.0;
/// How long a section's slide plays when nothing longer is asked for.
const HOLD_LIMIT_SECS: f64 = 3600.0;
const SPEED_STEP: f64 = 0.1;

pub(super) enum Phase {
    Idle,
    Starting,
    Countdown {
        began: Instant,
    },
    Rolling {
        video_start: Instant,
    },
    Stopping,
    Loading,
    Reviewing,
    Building {
        done: Arc<AtomicUsize>,
        total: usize,
    },
}

pub(super) struct Recorder {
    pub(super) current: usize,
    phase: Phase,
    wpm: f64,
    active: Option<ActiveRecording>,
    recording_began: Option<Instant>,
    player: Option<Player>,
    shown: Vec<Option<Arc<RenderImage>>>,
    listener: Option<guided_listen::Listener>,
    ticker: Option<Task<()>>,
    message: Option<String>,
    /// Slow the slide to fit the script at `wpm` (normal speed in step mode).
    match_pace: bool,
    /// Pause after each animation until → is pressed.
    step_mode: bool,
    /// A speed picked by hand for this section, instead of the matched one.
    speed: Option<f64>,
    clock: Option<Arc<SlideClock>>,
    finding_beats: bool,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            current: 0,
            phase: Phase::Idle,
            wpm: guided_recording::READ_WORDS_PER_MINUTE,
            active: None,
            recording_began: None,
            player: None,
            shown: Vec::new(),
            listener: None,
            ticker: None,
            message: None,
            match_pace: true,
            step_mode: true,
            speed: None,
            clock: None,
            finding_beats: false,
        }
    }
}

impl Recorder {
    pub(super) fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Reviewing)
    }
}

impl GuidedRecordingWindow {
    pub fn open_recorder(&mut self, cx: &mut Context<Self>) {
        let first_missing = self.session.as_ref().and_then(|session| {
            session
                .sections
                .iter()
                .position(|section| section.chosen_take().is_none())
        });
        self.recorder.current = first_missing.unwrap_or(0);
        self.stage = Stage::Record;
        self.find_beats(cx);
        cx.notify();
    }

    /// Sessions saved before step mode existed have no step points yet.
    fn find_beats(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if session.beats.is_some() || self.recorder.finding_beats {
            return;
        }
        self.recorder.finding_beats = true;
        cx.spawn(async move |this, cx| {
            let beats = cx
                .background_executor()
                .spawn(async move { guided_recording::session_beats(&session) })
                .await;
            this.update(cx, |this, cx| {
                this.recorder.finding_beats = false;
                match beats {
                    Ok(beats) => {
                        if let Some(session) = &mut this.session {
                            session.beats = Some(beats);
                            if let Err(error) = session.save() {
                                tracing::warn!(%error, "could not save the guided session");
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "could not find the slide's steps"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// How fast the slide plays: a speed picked by hand, else matched to the
    /// reading pace, else normal.
    fn slide_speed(&self) -> f64 {
        if let Some(speed) = self.recorder.speed {
            return speed;
        }
        match self.section() {
            Some(section) if self.recorder.match_pace && !self.recorder.step_mode => {
                guided_recording::slide_rate(section, self.recorder.wpm)
            }
            _ => 1.0,
        }
    }

    fn change_speed(&mut self, delta: f64, cx: &mut Context<Self>) {
        let speed = ((self.slide_speed() + delta) * 10.0).round() / 10.0;
        let speed = speed.clamp(
            guided_recording::MIN_SLIDE_RATE,
            guided_recording::MAX_SLIDE_RATE,
        );
        self.recorder.speed = Some(speed);
        if let Some(clock) = &self.recorder.clock {
            clock.set_rate(speed);
        }
        cx.notify();
    }

    /// Plays the slide on through one more animation. Pressed while one is
    /// still playing, the slide carries on through the next as well.
    fn next_step(&mut self, cx: &mut Context<Self>) {
        let Some(clock) = self.recorder.clock.clone() else {
            return;
        };
        let Some(end) = self.section().map(|section| section.end) else {
            return;
        };
        let after = if clock.waiting() {
            clock.source()
        } else {
            match clock.stop_point() {
                Some(stop) if stop < end => stop,
                Some(_) => clock.source(),
                None => return,
            }
        };
        let next = self
            .session
            .as_ref()
            .and_then(|session| session.next_beat(self.recorder.current, after));
        clock.resume(next);
        cx.notify();
    }

    pub(super) fn recorder_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match key {
            "space" => {
                self.toggle_take(window, cx);
                true
            }
            "escape" => {
                self.cancel(window, cx);
                true
            }
            "right" if matches!(self.recorder.phase, Phase::Rolling { .. }) => {
                self.next_step(cx);
                true
            }
            "up" if matches!(self.recorder.phase, Phase::Rolling { .. })
                || !self.recorder.busy() =>
            {
                self.change_speed(SPEED_STEP, cx);
                true
            }
            "down"
                if matches!(self.recorder.phase, Phase::Rolling { .. })
                    || !self.recorder.busy() =>
            {
                self.change_speed(-SPEED_STEP, cx);
                true
            }
            "left" if !self.recorder.busy() => {
                self.select_section(self.recorder.current.saturating_sub(1), window, cx);
                true
            }
            "right" if !self.recorder.busy() => {
                self.select_section(self.recorder.current + 1, window, cx);
                true
            }
            _ => false,
        }
    }

    fn section(&self) -> Option<&GuidedSection> {
        self.session.as_ref()?.sections.get(self.recorder.current)
    }

    fn select_section(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if self.recorder.busy() || index >= session.sections.len() {
            return;
        }
        self.stop_player(cx);
        self.recorder.current = index;
        self.recorder.speed = None;
        self.recorder.message = None;
        cx.notify();
    }

    fn toggle_take(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.recorder.phase {
            Phase::Idle | Phase::Reviewing => self.start_take(window, cx),
            Phase::Countdown { .. } => self.cancel(window, cx),
            Phase::Rolling { .. } => self.stop_take(window, cx),
            _ => {}
        }
    }

    fn start_take(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_player(cx);
        let (camera, microphone, config) = {
            let feeds = crate::feeds::Feeds::global(cx).read(cx);
            let config = StartConfig {
                mode: RecordingMode::Studio,
                target: ScreenCaptureTarget::CameraOnly,
                microphone: feeds.microphone.clone(),
                camera: feeds.camera.as_ref().map(|camera| camera.id.clone()),
                device_settings: feeds.requested_device_settings(),
                input_readiness: feeds.input_readiness(),
                system_audio: false,
                excluded_windows: Vec::new(),
                camera_feed: feeds.camera_actor(),
                mic_feed: feeds.mic_actor(),
                mic_meter: feeds.mic_meter(),
                #[cfg(target_os = "linux")]
                linux_instant_camera: None,
                start_gate: None,
            };
            (feeds.camera.is_some(), feeds.microphone.is_some(), config)
        };
        if !camera || !microphone {
            self.recorder.message = Some(
                "Choose a camera and a microphone in Cap's main window, then record.".to_string(),
            );
            cx.notify();
            return;
        }

        self.recorder.phase = Phase::Starting;
        self.recorder.message = None;
        cx.notify();
        let task = gpui_tokio::Tokio::spawn(cx, recording::start(config));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(active)) => {
                        this.recorder.active = Some(active);
                        this.recorder.recording_began = Some(Instant::now());
                        this.recorder.phase = Phase::Countdown {
                            began: Instant::now(),
                        };
                        crate::app_sounds::AppSound::StartRecording.play();
                        this.start_listener(cx);
                        this.start_ticker(window, cx);
                    }
                    Ok(Err(error)) => this.take_failed(format!("{error:#}"), cx),
                    Err(error) => this.take_failed(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Listens to the reader so the prompt keeps their place.
    fn start_listener(&mut self, cx: &mut Context<Self>) {
        let Some(section) = self.section() else {
            return;
        };
        let script: Vec<String> = guided_prompt::prompt_words(&section.text)
            .into_iter()
            .map(|word| word.key)
            .collect();
        let Some(actor) = crate::feeds::Feeds::global(cx).read(cx).mic_actor() else {
            return;
        };
        let (sender, samples) = flume::bounded(64);
        gpui_tokio::Tokio::spawn(cx, async move {
            if let Err(error) = actor
                .ask(cap_recording::feeds::microphone::AddSender(sender))
                .await
            {
                tracing::warn!(%error, "could not listen to the microphone");
            }
        })
        .detach();
        self.recorder.listener = Some(guided_listen::Listener::start(
            samples,
            script,
            self.recorder.wpm,
            crate::transcription::live_model_dir(),
        ));
    }

    fn take_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.recorder.listener = None;
        tracing::error!(%error, "guided take failed");
        self.recorder.phase = Phase::Idle;
        self.recorder.message = Some(format!("Recording failed: {error}"));
        cx.notify();
    }

    fn begin_rolling(&mut self) {
        let speed = self.slide_speed();
        let Some(section) = self.section() else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };
        let first_step = self
            .recorder
            .step_mode
            .then(|| session.next_beat(self.recorder.current, section.start))
            .flatten();
        let clock = SlideClock::new(section.start, section.end, speed, first_step);
        let player = Player::start(
            vec![PlayerVideo {
                path: session.video_path.clone(),
                pace: Pace::Live(clock.clone()),
                max_width: PREVIEW_MAX_WIDTH,
            }],
            None,
            HOLD_LIMIT_SECS,
        );
        self.recorder.clock = Some(clock);
        self.recorder.player = Some(player);
        self.recorder.phase = Phase::Rolling {
            video_start: Instant::now(),
        };
    }

    fn stop_take(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Phase::Rolling { video_start } = self.recorder.phase else {
            return;
        };
        let duration = video_start.elapsed().as_secs_f64();
        self.recorder.listener = None;
        let timing = self
            .recorder
            .clock
            .take()
            .map(|clock| clock.timing())
            .unwrap_or_default();
        let lead = self
            .recorder
            .recording_began
            .map_or(0.0, |began| (video_start - began).as_secs_f64());
        let Some(active) = self.recorder.active.take() else {
            return;
        };
        self.recorder.phase = Phase::Stopping;
        crate::app_sounds::AppSound::StopRecording.play();
        cx.notify();

        let index = self.recorder.current;
        let task = gpui_tokio::Tokio::spawn(cx, active.stop(false, None));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, _window, cx| {
                match result {
                    Ok(Ok(path)) => this.keep_take(
                        index,
                        Take {
                            project_path: path,
                            lead_secs: lead,
                            duration,
                            timing,
                        },
                        cx,
                    ),
                    Ok(Err(error)) => this.take_failed(format!("{error:#}"), cx),
                    Err(error) => this.take_failed(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn keep_take(&mut self, index: usize, mut take: Take, cx: &mut Context<Self>) {
        self.stop_player(cx);
        self.recorder.phase = Phase::Idle;
        let Some(session) = &mut self.session else {
            return;
        };
        let target = session.next_take_path(index);
        match std::fs::create_dir_all(session.takes_dir())
            .and_then(|()| std::fs::rename(&take.project_path, &target))
        {
            Ok(()) => take.project_path = target,
            Err(error) => tracing::warn!(%error, "could not move the take next to the session"),
        }
        let duration = take.duration;
        session.add_take(index, take);
        if let Err(error) = session.save() {
            tracing::warn!(%error, "could not save the guided session");
        }
        let section = &session.sections[index];
        self.recorder.message = Some(format!(
            "Saved take {} of {} ({:.1}s).",
            section.takes.len(),
            section.title,
            duration
        ));
        cx.notify();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.recorder.phase {
            Phase::Countdown { .. } => {
                if let Some(active) = self.recorder.active.take() {
                    let task = gpui_tokio::Tokio::spawn(cx, active.cancel_and_delete());
                    cx.spawn_in(window, async move |_, _| {
                        if let Ok(Err(error)) = task.await {
                            tracing::warn!(%error, "could not discard the guided take");
                        }
                    })
                    .detach();
                }
                self.recorder.clock = None;
                self.recorder.listener = None;
                self.recorder.phase = Phase::Idle;
                self.recorder.message = Some("Take discarded.".to_string());
            }
            Phase::Reviewing => {
                self.stop_player(cx);
                self.recorder.phase = Phase::Idle;
            }
            _ => {}
        }
        cx.notify();
    }

    fn stop_player(&mut self, cx: &mut Context<Self>) {
        self.recorder.player = None;
        if matches!(self.recorder.phase, Phase::Reviewing) {
            self.recorder.phase = Phase::Idle;
        }
        cx.notify();
    }

    fn review_take(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.busy() {
            return;
        }
        let (Some(session), Some(section)) = (&self.session, self.section()) else {
            return;
        };
        let Some(take) = section.chosen_take().cloned() else {
            return;
        };
        let video = session.video_path.clone();
        let audio_video = video.clone();
        let (start, until) = (section.start, section.duration());
        let plan = SegmentPlan {
            lead: 0.0,
            length: take.duration,
            source_start: section.start,
            source_end: section.end,
            timing: take.timing.clone(),
        };
        let slide_pace = if take.timing.is_empty() {
            Pace::Linear {
                file_time: start,
                until,
            }
        } else {
            Pace::Timed(take.timing.clone())
        };
        self.stop_player(cx);
        self.recorder.phase = Phase::Loading;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let media = guided_build::take_media(&take)?;
                    let mut tracks = Vec::new();
                    if let Some((mic, mic_start)) = media.mic {
                        let audio = AudioData::from_file(&mic)?;
                        tracks.push((audio, mic_start - take.lead_secs));
                    }
                    if let Ok(Some(audio)) = guided_media::segment_audio(&audio_video, &plan) {
                        tracks.push((audio, 0.0));
                    }
                    let audio = guided_media::mix(tracks, take.duration);
                    Ok::<_, String>((
                        media.camera,
                        take.lead_secs - media.camera_start,
                        audio,
                        take.duration,
                    ))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match loaded {
                    Ok((camera, camera_time, audio, length)) => {
                        this.recorder.player = Some(Player::start(
                            vec![
                                PlayerVideo {
                                    path: video,
                                    pace: slide_pace,
                                    max_width: PREVIEW_MAX_WIDTH,
                                },
                                PlayerVideo {
                                    path: camera,
                                    pace: Pace::Linear {
                                        file_time: camera_time,
                                        until: HOLD_LIMIT_SECS,
                                    },
                                    max_width: CAMERA_MAX_WIDTH,
                                },
                            ],
                            audio,
                            length,
                        ));
                        this.recorder.phase = Phase::Reviewing;
                        this.start_ticker(window, cx);
                    }
                    Err(error) => {
                        this.recorder.phase = Phase::Idle;
                        this.recorder.message = Some(format!("Could not play the take: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn choose_take(&mut self, take: usize, cx: &mut Context<Self>) {
        let index = self.recorder.current;
        let Some(session) = &mut self.session else {
            return;
        };
        if let Some(section) = session.sections.get_mut(index) {
            section.chosen = Some(take);
        }
        if let Err(error) = session.save() {
            tracing::warn!(%error, "could not save the guided session");
        }
        self.stop_player(cx);
    }

    fn build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if self.recorder.busy() || !session.ready_to_build() {
            return;
        }
        self.stop_player(cx);
        let done = Arc::new(AtomicUsize::new(0));
        self.recorder.phase = Phase::Building {
            done: done.clone(),
            total: session.sections.len(),
        };
        self.recorder.message = None;
        self.start_ticker(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    guided_build::build_project(&session, &|finished, _| {
                        done.store(finished, Ordering::Release)
                    })
                })
                .await;
            this.update_in(cx, |this, _window, cx| {
                this.recorder.phase = Phase::Idle;
                match result {
                    Ok(path) => {
                        this.recorder.message = Some(format!(
                            "Built {} and opened it in the editor.",
                            path.file_name().map_or_else(String::new, |name| name
                                .to_string_lossy()
                                .into_owned())
                        ));
                        cx.defer(move |cx| crate::app_windows::open_editor(path, cx));
                    }
                    Err(error) => {
                        this.recorder.message =
                            Some(format!("Could not build the project: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_ticker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.ticker.is_some() {
            return;
        }
        self.recorder.ticker = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.recorder.phase {
            Phase::Countdown { began } => {
                if began.elapsed().as_secs_f64() >= COUNTDOWN_SECS {
                    self.begin_rolling();
                }
            }
            Phase::Rolling { .. } => {}
            Phase::Reviewing => {
                if self
                    .recorder
                    .player
                    .as_ref()
                    .is_some_and(Player::is_finished)
                {
                    self.recorder.player = None;
                    self.recorder.phase = Phase::Idle;
                }
            }
            Phase::Building { .. } | Phase::Starting | Phase::Stopping | Phase::Loading => {}
            Phase::Idle => {
                self.recorder.ticker = None;
                self.release_frames(window);
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }

    /// The word the reader is on: followed by ear while recording, else at
    /// the wpm setting.
    fn prompt_word(&self, words: usize) -> Option<usize> {
        match self.recorder.phase {
            Phase::Countdown { .. } => Some(0),
            Phase::Rolling { video_start } => Some(match &self.recorder.listener {
                Some(listener) => listener.word(),
                None => {
                    let read = video_start.elapsed().as_secs_f64() * self.recorder.wpm / 60.0;
                    (read as usize).min(words.saturating_sub(1))
                }
            }),
            _ => None,
        }
    }

    /// The player's newest pictures, handing the replaced ones back to the
    /// sprite atlas.
    fn frames(&mut self, window: &mut Window) -> Vec<Option<Arc<RenderImage>>> {
        let frames: Vec<_> = match &self.recorder.player {
            Some(player) => (0..2).map(|index| player.frame(index)).collect(),
            None => return std::mem::take(&mut self.recorder.shown),
        };
        for (old, new) in self.recorder.shown.iter().zip(&frames) {
            if let Some(old) = old
                && new.as_ref().is_none_or(|new| !Arc::ptr_eq(old, new))
            {
                let _ = window.drop_image(old.clone());
            }
        }
        self.recorder.shown = frames.clone();
        frames
    }

    fn release_frames(&mut self, window: &mut Window) {
        for image in self.recorder.shown.drain(..).flatten() {
            let _ = window.drop_image(image);
        }
    }

    pub(super) fn render_recorder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let frames = self.frames(window);
        let Some(session) = &self.session else {
            return div().into_any_element();
        };
        let current = self
            .recorder
            .current
            .min(session.sections.len().saturating_sub(1));
        let Some(section) = session.sections.get(current) else {
            return div().into_any_element();
        };
        let busy = self.recorder.busy();
        let ready = session.ready_to_build();
        let recorded = session.recorded_count();
        let total = session.sections.len();

        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .py(px(8.))
            .border_b_1()
            .border_color(Hsla::from(theme.gray_4))
            .child(
                self.button(
                    "guided-back-to-split",
                    "Edit sections",
                    ButtonVariant::Outline,
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| {
                    if !this.recorder.busy() {
                        this.stop_player(cx);
                        this.stage = Stage::Ready;
                        cx.notify();
                    }
                })),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(Hsla::from(theme.gray_11))
                    .child(match &self.recorder.phase {
                        Phase::Building { done, total } => format!(
                            "Building the project… {} of {total} sections",
                            done.load(Ordering::Acquire)
                        ),
                        _ => format!("{recorded} of {total} sections recorded"),
                    }),
            )
            .child(
                self.button("guided-build", "Build project", ButtonVariant::Primary)
                    .disabled(busy || !ready)
                    .on_click(cx.listener(|this, _, window, cx| this.build(window, cx))),
            );

        let list = div()
            .id("guided-record-sections")
            .flex_none()
            .w(px(230.))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(Hsla::from(theme.gray_4))
            .children(session.sections.iter().enumerate().map(|(index, item)| {
                let selected = index == current;
                let status = match item.takes.len() {
                    0 => "○".to_string(),
                    1 => "●".to_string(),
                    count => format!("● {count}"),
                };
                div()
                    .id(("guided-record-section", index))
                    .flex()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(7.))
                    .text_size(px(12.))
                    .when(selected, |this| this.bg(Hsla::from(theme.gray_4)))
                    .when(!selected, |this| {
                        this.hover(move |style| style.bg(Hsla::from(theme.gray_3)))
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(28.))
                            .text_color(if item.takes.is_empty() {
                                Hsla::from(theme.gray_9)
                            } else {
                                Hsla::from(theme.blue_9)
                            })
                            .child(status),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(format!(
                        "{}. {}",
                        index + 1,
                        item.title
                    )))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_section(index, window, cx)
                    }))
            }));

        let still = self.thumbnails.get(current).cloned().flatten();
        let slide = frames.first().cloned().flatten().or(still);
        let camera = frames.get(1).cloned().flatten();
        let countdown = match self.recorder.phase {
            Phase::Countdown { began } => Some(
                ((COUNTDOWN_SECS - began.elapsed().as_secs_f64())
                    .ceil()
                    .max(1.0)) as u32,
            ),
            _ => None,
        };
        let elapsed = match self.recorder.phase {
            Phase::Rolling { video_start } => Some(video_start.elapsed().as_secs_f64()),
            Phase::Reviewing => self.recorder.player.as_ref().map(Player::elapsed),
            _ => None,
        };
        let speed = self.slide_speed();
        let live_clock = self
            .recorder
            .clock
            .clone()
            .filter(|_| matches!(self.recorder.phase, Phase::Rolling { .. }));
        let waiting = live_clock.as_ref().is_some_and(|clock| clock.waiting());
        let steps_left = live_clock.as_ref().map_or(0, |clock| {
            let mut count = 0;
            let mut after = clock.source();
            while let Some(beat) = session.next_beat(current, after) {
                count += 1;
                after = beat;
            }
            count
        });
        let total_steps = session.beats_in(current);

        let preview = div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(gpui::black())
            .when_some(slide, |this, image| {
                this.child(
                    gpui::img(image)
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
            })
            .when_some(camera, |this, image| {
                this.child(
                    div()
                        .absolute()
                        .right(px(12.))
                        .bottom(px(12.))
                        .w(px(180.))
                        .h(px(120.))
                        .rounded(px(8.))
                        .overflow_hidden()
                        .child(
                            gpui::img(image)
                                .size_full()
                                .object_fit(gpui::ObjectFit::Cover),
                        ),
                )
            })
            .when_some(countdown, |this, count| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(gpui::hsla(0., 0., 0., 0.45))
                        .text_size(px(96.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(gpui::white())
                        .child(count.to_string()),
                )
            })
            .when(waiting, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(16.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px(px(14.))
                                .py(px(6.))
                                .rounded(px(8.))
                                .bg(gpui::hsla(0., 0., 0., 0.7))
                                .text_size(px(15.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(gpui::white())
                                .child(format!(
                                    "Press → for the next animation ({steps_left} left)"
                                )),
                        ),
                )
            })
            .when_some(elapsed, |this, elapsed| {
                let over = live_clock
                    .as_ref()
                    .map_or(elapsed > section.duration(), |clock| clock.at_end());
                this.child(
                    div()
                        .absolute()
                        .left(px(12.))
                        .top(px(12.))
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(6.))
                        .bg(if matches!(self.recorder.phase, Phase::Rolling { .. }) {
                            Hsla::from(theme.red_9)
                        } else {
                            gpui::hsla(0., 0., 0., 0.6)
                        })
                        .text_size(px(12.))
                        .text_color(gpui::white())
                        .child(format!(
                            "{:.1}s · slide {:.0}%{}",
                            elapsed,
                            live_clock.as_ref().map_or(speed, |clock| clock.rate()) * 100.0,
                            if over {
                                " · holding the last frame"
                            } else if waiting {
                                " · waiting"
                            } else {
                                ""
                            }
                        )),
                )
            });

        let words = guided_prompt::prompt_words(&section.text);
        let reading = self.prompt_word(words.len());
        let focus = reading.unwrap_or(0).min(words.len().saturating_sub(1));
        let word_chip = |word: &PromptWord, now: bool, said: bool| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .child(
                    div()
                        .px(px(6.))
                        .rounded(px(5.))
                        .text_color(if said {
                            gpui::hsla(0., 0., 1., 0.4)
                        } else {
                            gpui::white()
                        })
                        .when(now, |this| {
                            this.bg(Hsla::from(theme.blue_9))
                                .font_weight(FontWeight::BOLD)
                        })
                        .child(word.text.clone()),
                )
                .when(word.pause, |this| {
                    this.child(
                        div()
                            .px(px(4.))
                            .text_color(gpui::hsla(0., 0., 1., 0.35))
                            .child("‖"),
                    )
                })
        };
        let before = &words[focus.saturating_sub(PROMPT_BEFORE)..focus];
        let after =
            &words[(focus + 1).min(words.len())..(focus + 1 + PROMPT_AFTER).min(words.len())];
        let listening = self.recorder.listener.as_ref();
        let prompt = div()
            .flex_none()
            .w_full()
            .flex()
            .justify_center()
            .px(px(16.))
            .py(px(10.))
            .child(
                div()
                    .relative()
                    .w(px(PROMPT_WIDTH))
                    .max_w_full()
                    .flex()
                    .items_center()
                    .px(px(16.))
                    .py(px(10.))
                    .rounded(px(12.))
                    .bg(gpui::hsla(0., 0., 0.08, 1.))
                    .text_size(px(PROMPT_TEXT_SIZE))
                    .line_height(px(PROMPT_TEXT_SIZE * 1.35))
                    .font_weight(FontWeight::MEDIUM)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .justify_end()
                            .overflow_hidden()
                            .children(before.iter().map(|word| word_chip(word, false, true))),
                    )
                    .children(
                        words
                            .get(focus)
                            .map(|word| word_chip(word, reading.is_some(), false)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .overflow_hidden()
                            .children(after.iter().map(|word| word_chip(word, false, false))),
                    )
                    .when_some(listening, |this, listener| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(8.))
                                .right(px(8.))
                                .size(px(7.))
                                .rounded_full()
                                .bg(if listener.speaking() {
                                    gpui::hsla(0.39, 0.7, 0.5, 1.)
                                } else {
                                    gpui::hsla(0., 0., 1., 0.25)
                                }),
                        )
                    }),
            );
        let prompt_info = format!(
            "{}. {} · {} words · about {:.0}s at {:.0} wpm · {}",
            current + 1,
            section.title,
            words.len(),
            words.len() as f64 * 60.0 / self.recorder.wpm,
            self.recorder.wpm,
            match listening {
                Some(listener) if listener.hearing() => "the prompt follows your words",
                Some(_) => "the prompt moves while you speak",
                None if crate::transcription::live_model_dir().is_some() => {
                    "the prompt will follow your words"
                }
                None => {
                    "the prompt will move while you speak (download a Parakeet caption model to follow your words)"
                }
            }
        );

        let record_label = match self.recorder.phase {
            Phase::Rolling { .. } => "■ Stop (Space)",
            Phase::Countdown { .. } => "Cancel (Esc)",
            Phase::Starting => "Starting…",
            Phase::Stopping => "Saving…",
            _ if section.takes.is_empty() => "● Record (Space)",
            _ => "● Retake (Space)",
        };
        let reviewing = matches!(self.recorder.phase, Phase::Reviewing);
        let chosen = section.chosen_index();

        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .border_t_1()
            .border_color(Hsla::from(theme.gray_4))
            .child(
                self.button(
                    "guided-record",
                    record_label,
                    if matches!(self.recorder.phase, Phase::Rolling { .. }) {
                        ButtonVariant::Destructive
                    } else {
                        ButtonVariant::Primary
                    },
                )
                .disabled(matches!(
                    self.recorder.phase,
                    Phase::Starting | Phase::Stopping | Phase::Loading | Phase::Building { .. }
                ))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_take(window, cx))),
            )
            .child(
                self.button(
                    "guided-review",
                    if reviewing {
                        "Stop playback"
                    } else {
                        "▶ Play take"
                    },
                    ButtonVariant::Gray,
                )
                .disabled(section.takes.is_empty() || (busy && !reviewing))
                .on_click(cx.listener(|this, _, window, cx| {
                    if matches!(this.recorder.phase, Phase::Reviewing) {
                        this.cancel(window, cx);
                    } else {
                        this.review_take(window, cx);
                    }
                })),
            )
            .children(section.takes.iter().enumerate().map(|(index, take)| {
                let selected = chosen == Some(index);
                self.button(
                    format!("guided-take-{index}"),
                    &format!(
                        "{}Take {} · {:.1}s",
                        if selected { "✓ " } else { "" },
                        index + 1,
                        take.duration
                    ),
                    if selected {
                        ButtonVariant::Dark
                    } else {
                        ButtonVariant::Outline
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _window, cx| this.choose_take(index, cx)))
            }))
            .child(div().flex_1())
            .child(
                self.button("guided-previous", "◀ Previous", ButtonVariant::Gray)
                    .disabled(busy || current == 0)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_section(current.saturating_sub(1), window, cx)
                    })),
            )
            .child(
                self.button("guided-next", "Next ▶", ButtonVariant::Gray)
                    .disabled(busy || current + 1 >= total)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_section(current + 1, window, cx)
                    })),
            );

        let speed_note = if self.recorder.speed.is_some() {
            "set by hand"
        } else if self.recorder.step_mode {
            "normal speed in step mode"
        } else if self.recorder.match_pace {
            "fitted to your reading pace"
        } else {
            "normal speed"
        };
        let label = |text: String| {
            div()
                .text_size(px(12.))
                .text_color(Hsla::from(theme.gray_11))
                .child(text)
        };
        let pace = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .pt(px(10.))
            .child(
                self.button(
                    "guided-step-mode",
                    if self.recorder.step_mode {
                        "✓ Step through animations"
                    } else {
                        "Step through animations"
                    },
                    if self.recorder.step_mode {
                        ButtonVariant::Dark
                    } else {
                        ButtonVariant::Outline
                    },
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.recorder.step_mode = !this.recorder.step_mode;
                    this.recorder.speed = None;
                    cx.notify();
                })),
            )
            .child(label(if self.recorder.finding_beats {
                "finding the animations…".to_string()
            } else {
                format!("{total_steps} pauses here, → plays the next")
            }))
            .child(div().w(px(12.)))
            .child(
                self.button("guided-speed-down", "−", ButtonVariant::Outline)
                    .disabled(busy && live_clock.is_none())
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.change_speed(-SPEED_STEP, cx)),
                    ),
            )
            .child(label(format!("Slide {:.0}%", speed * 100.0)))
            .child(
                self.button("guided-speed-up", "+", ButtonVariant::Outline)
                    .disabled(busy && live_clock.is_none())
                    .on_click(
                        cx.listener(|this, _, _window, cx| this.change_speed(SPEED_STEP, cx)),
                    ),
            )
            .child(label(format!("{speed_note} (↑ ↓)")))
            .when(self.recorder.speed.is_some(), |this| {
                this.child(
                    self.button("guided-speed-reset", "Fit to pace", ButtonVariant::Outline)
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.recorder.speed = None;
                            this.recorder.match_pace = true;
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1())
            .child(
                self.button("guided-slower", "−", ButtonVariant::Outline)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.recorder.wpm = (this.recorder.wpm - WPM_STEP).max(MIN_WPM);
                        if let Some(listener) = &this.recorder.listener {
                            listener.set_wpm(this.recorder.wpm);
                        }
                        this.recorder.speed = None;
                        cx.notify();
                    })),
            )
            .child(label(format!("{:.0} wpm", self.recorder.wpm)))
            .child(
                self.button("guided-faster", "+", ButtonVariant::Outline)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.recorder.wpm = (this.recorder.wpm + WPM_STEP).min(MAX_WPM);
                        if let Some(listener) = &this.recorder.listener {
                            listener.set_wpm(this.recorder.wpm);
                        }
                        this.recorder.speed = None;
                        cx.notify();
                    })),
            );

        let message = self.recorder.message.clone();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(prompt)
            .child(toolbar)
            .child(
                div().flex().flex_1().min_h_0().child(list).child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(preview)
                        .child(
                            div()
                                .px(px(16.))
                                .pt(px(8.))
                                .text_size(px(12.))
                                .text_color(Hsla::from(theme.gray_11))
                                .child(prompt_info),
                        )
                        .when_some(message, |this, message| {
                            this.child(
                                div()
                                    .px(px(16.))
                                    .py(px(6.))
                                    .text_size(px(12.))
                                    .text_color(Hsla::from(theme.gray_11))
                                    .child(message),
                            )
                        })
                        .child(pace)
                        .child(controls),
                ),
            )
            .into_any_element()
    }
}
