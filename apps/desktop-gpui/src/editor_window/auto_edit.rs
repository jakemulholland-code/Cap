//! Auto edit: preview and cut the silent, idle stretches of a recording, its
//! coughs and other short noises, and the retakes its transcript reveals.
//!
//! Detection and the cut itself are pure functions in
//! [`crate::editor_auto_edit`]; this module owns the panel state, the
//! transcript it runs on, the timeline highlights, stepping and playing
//! through the cuts, and the single history entry the cut lands as.

use super::*;
use crate::editor_auto_edit::{
    self as auto_edit, AudioLevels, AutoEditCut, AutoEditSettings, CutReason, MIN_GAP_OPTIONS,
    PADDING_OPTIONS, RETAKE_OPTIONS, THRESHOLD_OPTIONS, TRANSITION_OPTIONS,
};

pub(super) const AUTO_EDIT_MENU_WIDTH: f32 = 340.;
const PREVIEW_LEAD_SECS: f64 = 1.0;
const PREVIEW_TAIL_SECS: f64 = 0.75;

pub(super) struct AutoEditPanel {
    settings: AutoEditSettings,
    cuts: Vec<AutoEditCut>,
    levels: Option<AudioLevels>,
    focus: Option<usize>,
    play_all: bool,
    left: Pixels,
    anchor: ClipMenuAnchor,
    max_height: Pixels,
}

impl EditorWindow {
    pub(super) fn toggle_auto_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto_edit.take().is_some() {
            cx.notify();
            return;
        }
        if !self.project_ready() {
            return;
        }
        self.clip_speed = None;
        self.text_menu = None;
        if self.split_mode {
            self.toggle_split_mode(cx);
        }
        edits::ensure_timeline(&mut self.project, &self.clip_display_durations);
        let viewport = window.viewport_size();
        let placement =
            clip_menu_placement(0, window.mouse_position(), viewport, AUTO_EDIT_MENU_WIDTH);
        let room = f32::from(viewport.height)
            - match placement.anchor {
                ClipMenuAnchor::Above { bottom } => f32::from(bottom),
                ClipMenuAnchor::Below { top } => f32::from(top),
            }
            - 12.;
        self.auto_edit = Some(AutoEditPanel {
            settings: self.auto_edit_settings,
            cuts: Vec::new(),
            levels: None,
            focus: None,
            play_all: false,
            left: placement.left,
            anchor: placement.anchor,
            max_height: px(room.max(240.)),
        });
        self.load_auto_edit_activity(window, cx);
        self.load_auto_edit_transcript(window, cx);
        self.refresh_auto_edit_preview();
        cx.notify();
    }

    fn load_auto_edit_activity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto_edit_activity.is_some() || self.loading_auto_edit_activity {
            return;
        }
        self.loading_auto_edit_activity = true;
        let path = self.project_path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let activity = cx
                .background_executor()
                .spawn(async move { auto_edit::load_recording_activity(&path) })
                .await;
            this.update_in(cx, |this, _window, cx| {
                this.loading_auto_edit_activity = false;
                this.auto_edit_activity = Some(Arc::new(activity));
                this.refresh_auto_edit_preview();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The project's own captions when it has them, otherwise the transcript
    /// auto edit made for itself.
    fn auto_edit_caption_segments(&self) -> Option<&[cap_project::CaptionSegment]> {
        self.project
            .captions
            .as_ref()
            .filter(|captions| {
                captions.source_timed
                    && captions
                        .segments
                        .iter()
                        .any(|segment| !segment.words.is_empty())
            })
            .map(|captions| captions.segments.as_slice())
            .or_else(|| self.auto_edit_transcript.as_deref().map(Vec::as_slice))
    }

    fn auto_edit_transcription_model(&self) -> Option<&'static str> {
        crate::transcription::refresh_downloaded_models();
        let downloaded = crate::transcription::ui_snapshot(&self.project_path).downloaded;
        std::iter::once(self.selected_caption_model().name)
            .chain(
                crate::editor_tabs::available_caption_models()
                    .iter()
                    .map(|model| model.name),
            )
            .find(|model| downloaded.contains(*model))
    }

    /// Transcribes the recording in the background, without touching the
    /// project's captions, so retakes and coughs can be told from speech.
    fn load_auto_edit_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto_edit_caption_segments().is_some() || self.loading_auto_edit_transcript {
            return;
        }
        let Some(model) = self.auto_edit_transcription_model() else {
            self.auto_edit_transcript_error =
                Some("Download a captions model in the Captions tab to find retakes.".to_string());
            return;
        };
        self.auto_edit_transcript_error = None;
        self.loading_auto_edit_transcript = true;
        let language = self.sidebar.caption_language;
        let path = self.project_path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let task = gpui_tokio::Tokio::spawn(cx, async move {
                tokio::task::spawn_blocking(move || {
                    crate::transcription::transcribe_blocking(&path, model, language)
                })
                .await
                .map_err(|error| format!("Transcription task panicked: {error}"))?
            });
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(format!("Transcription task failed: {error}")),
            };
            this.update_in(cx, |this, _window, cx| {
                this.loading_auto_edit_transcript = false;
                match result {
                    Ok(segments) => this.auto_edit_transcript = Some(Arc::new(segments)),
                    Err(error) => {
                        tracing::warn!("auto edit transcription failed: {error}");
                        this.auto_edit_transcript_error = Some(
                            crate::transcription::caption_generation_error_message(&error),
                        );
                    }
                }
                this.refresh_auto_edit_preview();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn refresh_auto_edit_preview(&mut self) {
        let words = self
            .auto_edit_caption_segments()
            .map(|segments| auto_edit::transcript_words(segments, &self.clip_display_durations))
            .unwrap_or_default();
        let Some(panel) = self.auto_edit.as_mut() else {
            return;
        };
        let Some(timeline) = self.project.timeline.as_ref() else {
            panel.cuts.clear();
            panel.focus = None;
            return;
        };
        let activity = self
            .auto_edit_activity
            .as_deref()
            .map_or(&[][..], |activity| activity.as_slice());
        panel.levels = auto_edit::measure_levels(
            timeline,
            &self.timeline.mic_waveforms,
            &self.timeline.system_waveforms,
        );
        panel.cuts = auto_edit::detect_cuts(
            timeline,
            &self.timeline.mic_waveforms,
            &self.timeline.system_waveforms,
            activity,
            &words,
            panel.settings,
        );
        panel.focus = panel.focus.filter(|focus| *focus < panel.cuts.len());
    }

    fn update_auto_edit_settings(
        &mut self,
        change: impl FnOnce(&mut AutoEditSettings),
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.auto_edit.as_mut() else {
            return;
        };
        change(&mut panel.settings);
        self.auto_edit_settings = panel.settings;
        self.refresh_auto_edit_preview();
        cx.notify();
    }

    fn auto_edit_waiting(&self) -> Option<&'static str> {
        let panel = self.auto_edit.as_ref()?;
        if self
            .timeline
            .mic_waveforms
            .iter()
            .all(|table| table.is_empty())
            && self
                .timeline
                .system_waveforms
                .iter()
                .all(|table| table.is_empty())
        {
            return Some("Analysing audio…");
        }
        if self.loading_auto_edit_transcript
            && (panel.settings.cut_noises || panel.settings.retakes != auto_edit::RetakeMode::Keep)
        {
            return Some("Transcribing to find retakes and coughs…");
        }
        (panel.settings.keep_screen_activity && self.auto_edit_activity.is_none())
            .then_some("Reading screen activity…")
    }

    /// Selects a cut and brings it into view, returning where it sits on the
    /// output timeline.
    fn focus_auto_edit_cut(&mut self, index: usize) -> Option<(f64, f64)> {
        let panel = self.auto_edit.as_mut()?;
        let cut = *panel.cuts.get(index)?;
        panel.focus = Some(index);
        let range = auto_edit::cut_output_range(self.project.timeline.as_ref()?, &cut)?;
        if !self.view.transform.segment_visible(range.0, range.1) {
            let total = self.total_duration();
            let lead = self.view.transform.zoom * 0.25;
            self.view.transform.set_position(range.0 - lead, total);
        }
        Some(range)
    }

    fn step_auto_edit_cut(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(panel) = self.auto_edit.as_ref() else {
            return;
        };
        let count = panel.cuts.len();
        if count == 0 {
            return;
        }
        let next = match panel.focus {
            Some(focus) if forward => (focus + 1).min(count - 1),
            Some(focus) => focus.saturating_sub(1),
            None => {
                let playhead = self.playhead;
                let starts = self
                    .project
                    .timeline
                    .as_ref()
                    .map_or_else(Vec::new, |timeline| {
                        panel
                            .cuts
                            .iter()
                            .map(|cut| {
                                auto_edit::cut_output_range(timeline, cut)
                                    .map_or(f64::INFINITY, |range| range.0)
                            })
                            .collect()
                    });
                if forward {
                    starts
                        .iter()
                        .position(|start| *start >= playhead - 0.05)
                        .unwrap_or(0)
                } else {
                    starts
                        .iter()
                        .rposition(|start| *start < playhead - 0.05)
                        .unwrap_or(count - 1)
                }
            }
        };
        if self.playing {
            self.stop_playback(cx);
        }
        if let Some((start, _)) = self.focus_auto_edit_cut(next) {
            self.seek_to_time(start, cx);
        }
        cx.notify();
    }

    /// Plays a cut with a second either side, so it is heard in context.
    fn play_auto_edit_cut(&mut self, index: usize, play_all: bool, cx: &mut Context<Self>) {
        let Some((start, end)) = self.focus_auto_edit_cut(index) else {
            return;
        };
        if let Some(panel) = self.auto_edit.as_mut() {
            panel.play_all = play_all;
        }
        if self.playing {
            self.stop_playback(cx);
        }
        self.start_playback((start - PREVIEW_LEAD_SECS).max(0.0), cx);
        self.play_until = Some(end + PREVIEW_TAIL_SECS);
        cx.notify();
    }

    /// Called when a preview reaches its end: moves on to the next cut while
    /// playing them all, and reports whether it did.
    pub(super) fn advance_auto_edit_preview(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(panel) = self.auto_edit.as_mut() else {
            return false;
        };
        if !panel.play_all {
            return false;
        }
        let next = panel.focus.map_or(0, |focus| focus + 1);
        if next >= panel.cuts.len() {
            panel.play_all = false;
            return false;
        }
        self.play_auto_edit_cut(next, true, cx);
        true
    }

    fn apply_auto_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto_edit_waiting().is_some() {
            return;
        }
        self.refresh_auto_edit_preview();
        let Some(panel) = self.auto_edit.take() else {
            return;
        };
        if self.playing {
            self.stop_playback(cx);
        }
        if panel.cuts.is_empty() {
            cx.notify();
            return;
        }
        let count = panel.cuts.len();
        if self.edit(
            |timeline| auto_edit::apply_cuts(timeline, &panel.cuts, panel.settings.transition),
            window,
            cx,
        ) {
            tracing::info!(count, "auto edit applied");
            self.note_edit("auto-edit", Some(TrackKind::Clip));
        }
        cx.notify();
    }

    pub(super) fn render_auto_edit_overlays(
        &self,
        model: &timeline::TimelineModel,
        viewport_width: f32,
    ) -> Option<gpui::AnyElement> {
        let panel = self.auto_edit.as_ref()?;
        let timeline = self.project.timeline.as_ref()?;
        let lane_width = timeline::content_width(viewport_width);
        let secs_per_pixel = self.view.transform.secs_per_pixel(lane_width);
        let mut layer = div()
            .absolute()
            .left(px(timeline::TRACK_GUTTER))
            .right_0()
            .top_0()
            .bottom_0()
            .overflow_hidden();
        for (index, cut) in panel.cuts.iter().enumerate() {
            let (Some(clip), Some(segment)) = (
                model.clips.get(cut.clip_index),
                timeline.segments.get(cut.clip_index),
            ) else {
                continue;
            };
            let start = clip.start + (cut.start - segment.start) / segment.timescale;
            let end = clip.start + (cut.end - segment.start) / segment.timescale;
            if !self.view.transform.segment_visible(start, end) {
                continue;
            }
            let x = ((start - self.view.transform.position) / secs_per_pixel) as f32;
            let width = (((end - start) / secs_per_pixel) as f32).max(2.);
            let color = reason_color(&self.theme, cut.reason);
            let focused = panel.focus == Some(index);
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x))
                    .w(px(width))
                    .top(px(2.))
                    .bottom(px(2.))
                    .rounded(px(4.))
                    .when(focused, |this| this.border_2())
                    .when(!focused, |this| this.border_1())
                    .border_color(color.opacity(if focused { 1.0 } else { 0.9 }))
                    .bg(color.opacity(if focused { 0.45 } else { 0.28 })),
            );
        }
        Some(layer.into_any_element())
    }

    pub(super) fn render_auto_edit_popover(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let panel = self.auto_edit.as_ref()?;
        let theme = self.theme;
        let settings = panel.settings;
        let waiting = self.auto_edit_waiting();
        let count = panel.cuts.len();
        let removed = self.project.timeline.as_ref().map_or(0.0, |timeline| {
            auto_edit::removed_output_secs(timeline, &panel.cuts)
        });
        let summary = match waiting {
            Some(message) => message.to_string(),
            None if count == 0 => "Nothing to cut with these settings".to_string(),
            None => format!(
                "{} · {} removed",
                describe_cuts(&panel.cuts),
                format_removed(removed)
            ),
        };

        let chip = |id: SharedString, selected: bool, label: String| {
            div()
                .id(id)
                .flex_1()
                .rounded(px(6.))
                .px(px(4.))
                .py(px(4.))
                .text_size(px(11.5))
                .text_center()
                .whitespace_nowrap()
                .cursor_pointer()
                .bg(if selected {
                    Hsla::from(theme.editor.ctl_active)
                } else {
                    gpui::transparent_black()
                })
                .text_color(Hsla::from(if selected {
                    theme.editor.text_1
                } else {
                    theme.editor.text_2
                }))
                .hover(|this| this.text_color(Hsla::from(theme.editor.text_1)))
                .child(label)
        };
        let chip_row = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.))
                .rounded(px(8.))
                .bg(Hsla::from(theme.editor.ctl))
                .p(px(2.))
        };
        let hint = |text: String| {
            div()
                .px(px(8.))
                .text_size(px(10.5))
                .text_color(Hsla::from(theme.editor.text_3))
                .child(text)
        };
        let section = |label: &'static str, row: gpui::Div| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(clip_menu_section(&theme, label))
                .child(row)
        };

        let thresholds = chip_row().children(THRESHOLD_OPTIONS.into_iter().map(|threshold| {
            chip(
                SharedString::from(format!("auto-edit-threshold-{threshold:?}")),
                settings.threshold_db == threshold,
                threshold.map_or_else(|| "Auto".to_string(), |db| format!("{db:.0}")),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.threshold_db = threshold, cx);
            }))
        }));
        let threshold_in_use = auto_edit::resolve_threshold(settings, panel.levels);
        let level_hint = match panel.levels {
            Some(levels) => format!(
                "Your speech sits near {:.0} dB and the room near {:.0} dB. Cutting below {:.0} dB.",
                levels.speech_db, levels.room_db, threshold_in_use
            ),
            None => format!("Cutting below {threshold_in_use:.0} dB."),
        };
        let thresholds = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(thresholds)
            .child(hint(level_hint));
        let gaps = chip_row().children(MIN_GAP_OPTIONS.into_iter().map(|gap| {
            chip(
                SharedString::from(format!("auto-edit-gap-{gap}")),
                (settings.min_gap_secs - gap).abs() < f64::EPSILON,
                format_seconds(gap),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.min_gap_secs = gap, cx);
            }))
        }));
        let paddings = chip_row().children(PADDING_OPTIONS.into_iter().map(|padding| {
            chip(
                SharedString::from(format!("auto-edit-padding-{padding}")),
                (settings.padding_secs - padding).abs() < f64::EPSILON,
                format_seconds(padding),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.padding_secs = padding, cx);
            }))
        }));
        let activity =
            chip_row().children([(true, "Keep it"), (false, "Ignore it")].into_iter().map(
                |(keep, label)| {
                    chip(
                        SharedString::from(format!("auto-edit-activity-{keep}")),
                        settings.keep_screen_activity == keep,
                        label.to_string(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_auto_edit_settings(
                            |settings| settings.keep_screen_activity = keep,
                            cx,
                        );
                    }))
                },
            ));
        let noises =
            chip_row().children([(true, "Cut them"), (false, "Keep them")].into_iter().map(
                |(cut, label)| {
                    chip(
                        SharedString::from(format!("auto-edit-noises-{cut}")),
                        settings.cut_noises == cut,
                        label.to_string(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_auto_edit_settings(|settings| settings.cut_noises = cut, cx);
                    }))
                },
            ));
        let retakes = chip_row().children(RETAKE_OPTIONS.into_iter().map(|mode| {
            chip(
                SharedString::from(format!("auto-edit-retakes-{mode:?}")),
                settings.retakes == mode,
                mode.label().to_string(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.retakes = mode, cx);
            }))
        }));
        let cue = "Say \u{201c}go back and start that again\u{201d} to mark a retake.";
        let retake_hint = if self.loading_auto_edit_transcript {
            "Transcribing the recording…".to_string()
        } else if self.auto_edit_caption_segments().is_some() {
            cue.to_string()
        } else {
            self.auto_edit_transcript_error.clone().unwrap_or_else(|| {
                "Retakes need a transcript, which could not be made.".to_string()
            })
        };
        let retakes = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(retakes)
            .child(hint(retake_hint));
        let transitions = chip_row().children(TRANSITION_OPTIONS.into_iter().map(|transition| {
            chip(
                SharedString::from(format!("auto-edit-transition-{transition:?}")),
                settings.transition == transition,
                transition.label().to_string(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.transition = transition, cx);
            }))
        }));

        let can_apply = waiting.is_none() && count > 0;
        let apply_label = if count == 0 {
            "Cut sections".to_string()
        } else {
            format!("Cut {count} section{}", if count == 1 { "" } else { "s" })
        };

        let focused = panel
            .focus
            .and_then(|focus| panel.cuts.get(focus).map(|cut| (focus, cut)));
        let step_label = match focused {
            Some((focus, cut)) => {
                let secs = self
                    .project
                    .timeline
                    .as_ref()
                    .and_then(|timeline| auto_edit::cut_output_range(timeline, cut))
                    .map_or(0.0, |(start, end)| end - start);
                format!(
                    "{} of {count} · {} · {}",
                    focus + 1,
                    reason_label(cut.reason),
                    format_removed(secs)
                )
            }
            None => format!(
                "Step through {count} cut{}",
                if count == 1 { "" } else { "s" }
            ),
        };
        let can_step = count > 0;
        let playing_preview = self.playing && self.play_until.is_some();
        let navigator = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(
                ui::IconButton::body(&theme, "auto-edit-prev", "icons/chevron-left.svg")
                    .size(px(26.))
                    .icon_size(px(14.))
                    .rounded(px(7.))
                    .disabled(!can_step)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.step_auto_edit_cut(false, cx);
                        window.refresh();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .text_center()
                    .text_size(px(11.5))
                    .whitespace_nowrap()
                    .text_color(Hsla::from(theme.editor.text_2))
                    .child(step_label),
            )
            .child(
                ui::IconButton::body(&theme, "auto-edit-next", "icons/chevron-right.svg")
                    .size(px(26.))
                    .icon_size(px(14.))
                    .rounded(px(7.))
                    .disabled(!can_step)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.step_auto_edit_cut(true, cx);
                        window.refresh();
                    })),
            )
            .child(
                ui::Button::plain(
                    &theme,
                    "auto-edit-play-cut",
                    ui::ButtonVariant::Gray,
                    ui::ButtonSize::Sm,
                )
                .icon(if playing_preview {
                    "icons/pause.svg"
                } else {
                    "icons/play.svg"
                })
                .label(if playing_preview { "Stop" } else { "Play" })
                .radius(px(7.))
                .disabled(!can_step)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if playing_preview {
                        if let Some(panel) = this.auto_edit.as_mut() {
                            panel.play_all = false;
                        }
                        this.stop_playback(cx);
                    } else {
                        let index = this
                            .auto_edit
                            .as_ref()
                            .and_then(|panel| panel.focus)
                            .unwrap_or(0);
                        this.play_auto_edit_cut(index, false, cx);
                    }
                    window.refresh();
                })),
            )
            .child(
                ui::Button::plain(
                    &theme,
                    "auto-edit-play-all",
                    ui::ButtonVariant::Gray,
                    ui::ButtonSize::Sm,
                )
                .label("Play all")
                .radius(px(7.))
                .disabled(!can_step)
                .on_click(cx.listener(|this, _, window, cx| {
                    let index = this
                        .auto_edit
                        .as_ref()
                        .and_then(|panel| panel.focus)
                        .unwrap_or(0);
                    this.play_auto_edit_cut(index, true, cx);
                    window.refresh();
                })),
            );

        let mut card = div()
            .id("auto-edit-popover")
            .absolute()
            .left(panel.left)
            .w(px(AUTO_EDIT_MENU_WIDTH))
            .max_h(panel.max_height)
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(12.))
            .border_1()
            .border_color(Hsla::from(theme.editor.line))
            .bg(Hsla::from(theme.editor.card))
            .p(px(8.))
            .shadow(theme.editor.pop_shadow())
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            );
        card = match panel.anchor {
            ClipMenuAnchor::Above { bottom } => card.bottom(bottom),
            ClipMenuAnchor::Below { top } => card.top(top),
        };

        let card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(8.))
                    .pt(px(4.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Hsla::from(theme.editor.text_1))
                            .child("Auto edit"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(Hsla::from(theme.editor.text_3))
                            .child(
                                "Highlighted sections will be removed: red for silence, \
                                 amber for noises, blue for retakes.",
                            ),
                    ),
            )
            .child(section("SILENCE BELOW (DB)", thresholds))
            .child(section("SHORTEST GAP TO CUT", gaps))
            .child(section("KEEP EITHER SIDE", paddings))
            .child(section("WHEN THE SCREEN IS ACTIVE", activity))
            .child(section("COUGHS AND NOISES", noises))
            .child(section("RETAKES", retakes))
            .child(section("TRANSITION AT EACH CUT", transitions))
            .child(div().h(px(1.)).bg(Hsla::from(theme.editor.line)))
            .child(section("PREVIEW THE CUTS", navigator))
            .child(
                div()
                    .px(px(8.))
                    .text_size(px(11.5))
                    .text_color(Hsla::from(theme.editor.text_2))
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(6.))
                    .child(
                        ui::Button::plain(
                            &theme,
                            "auto-edit-cancel",
                            ui::ButtonVariant::Gray,
                            ui::ButtonSize::Sm,
                        )
                        .label("Cancel")
                        .radius(px(7.))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.auto_edit = None;
                            cx.notify();
                            window.refresh();
                        })),
                    )
                    .child(
                        ui::Button::plain(
                            &theme,
                            "auto-edit-apply",
                            ui::ButtonVariant::Primary,
                            ui::ButtonSize::Sm,
                        )
                        .icon("icons/scissors.svg")
                        .label(apply_label)
                        .radius(px(7.))
                        .disabled(!can_apply)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_auto_edit(window, cx);
                            window.refresh();
                        })),
                    ),
            );
        Some(card.into_any_element())
    }
}

fn reason_color(theme: &Theme, reason: CutReason) -> Hsla {
    Hsla::from(match reason {
        CutReason::Silence => theme.red_9,
        CutReason::Noise => theme.amber_11,
        CutReason::Retake => theme.blue_9,
    })
}

fn reason_label(reason: CutReason) -> &'static str {
    match reason {
        CutReason::Silence => "silence",
        CutReason::Noise => "noise",
        CutReason::Retake => "retake",
    }
}

fn describe_cuts(cuts: &[AutoEditCut]) -> String {
    [
        (CutReason::Silence, "silence", "silences"),
        (CutReason::Noise, "noise", "noises"),
        (CutReason::Retake, "retake", "retakes"),
    ]
    .into_iter()
    .filter_map(|(reason, one, many)| {
        let count = auto_edit::count_by_reason(cuts, reason);
        (count > 0).then(|| format!("{count} {}", if count == 1 { one } else { many }))
    })
    .collect::<Vec<_>>()
    .join(", ")
}

fn format_seconds(seconds: f64) -> String {
    if seconds.fract().abs() < f64::EPSILON {
        format!("{seconds:.0}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_removed(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        crate::editor_clips::format_clip_duration(seconds)
    }
}
