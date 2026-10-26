//! Auto edit: preview and cut the silent, idle stretches of a recording.
//!
//! Detection and the cut itself are pure functions in
//! [`crate::editor_auto_edit`]; this module owns the panel state, the
//! timeline highlights and the single history entry the cut lands as.

use super::*;
use crate::editor_auto_edit::{
    self as auto_edit, AutoEditCut, AutoEditSettings, MIN_GAP_OPTIONS, PADDING_OPTIONS,
    THRESHOLD_OPTIONS, TRANSITION_OPTIONS,
};

pub(super) const AUTO_EDIT_MENU_WIDTH: f32 = 320.;

pub(super) struct AutoEditPanel {
    settings: AutoEditSettings,
    cuts: Vec<AutoEditCut>,
    left: Pixels,
    anchor: ClipMenuAnchor,
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
        let placement = clip_menu_placement(
            0,
            window.mouse_position(),
            window.viewport_size(),
            AUTO_EDIT_MENU_WIDTH,
        );
        self.auto_edit = Some(AutoEditPanel {
            settings: self.auto_edit_settings,
            cuts: Vec::new(),
            left: placement.left,
            anchor: placement.anchor,
        });
        self.load_auto_edit_activity(window, cx);
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

    pub(super) fn refresh_auto_edit_preview(&mut self) {
        let Some(panel) = self.auto_edit.as_mut() else {
            return;
        };
        let Some(timeline) = self.project.timeline.as_ref() else {
            panel.cuts.clear();
            return;
        };
        let activity = self
            .auto_edit_activity
            .as_deref()
            .map_or(&[][..], |activity| activity.as_slice());
        panel.cuts = auto_edit::detect_empty_sections(
            timeline,
            &self.timeline.mic_waveforms,
            &self.timeline.system_waveforms,
            activity,
            panel.settings,
        );
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
        (panel.settings.keep_screen_activity && self.auto_edit_activity.is_none())
            .then_some("Reading screen activity…")
    }

    fn apply_auto_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto_edit_waiting().is_some() {
            return;
        }
        self.refresh_auto_edit_preview();
        let Some(panel) = self.auto_edit.take() else {
            return;
        };
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
        let red = Hsla::from(self.theme.red_9);
        let mut layer = div()
            .absolute()
            .left(px(timeline::TRACK_GUTTER))
            .right_0()
            .top_0()
            .bottom_0()
            .overflow_hidden();
        for cut in &panel.cuts {
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
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x))
                    .w(px(width))
                    .top(px(2.))
                    .bottom(px(2.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(red.opacity(0.9))
                    .bg(red.opacity(0.28)),
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
                "{count} section{} · {} removed",
                if count == 1 { "" } else { "s" },
                format_removed(removed)
            ),
        };

        let chip = |id: SharedString, selected: bool, label: String| {
            div()
                .id(id)
                .flex_1()
                .rounded(px(6.))
                .px(px(6.))
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
                SharedString::from(format!("auto-edit-threshold-{threshold}")),
                (settings.threshold_db - threshold).abs() < f32::EPSILON,
                format!("{threshold:.0} dB"),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_auto_edit_settings(|settings| settings.threshold_db = threshold, cx);
            }))
        }));
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

        let mut card = div()
            .id("auto-edit-popover")
            .absolute()
            .left(panel.left)
            .w(px(AUTO_EDIT_MENU_WIDTH))
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
                            .child("Cuts quiet stretches. Red sections will be removed."),
                    ),
            )
            .child(section("SILENCE BELOW", thresholds))
            .child(section("SHORTEST GAP TO CUT", gaps))
            .child(section("KEEP EITHER SIDE", paddings))
            .child(section("WHEN THE SCREEN IS ACTIVE", activity))
            .child(section("TRANSITION AT EACH CUT", transitions))
            .child(div().h(px(1.)).bg(Hsla::from(theme.editor.line)))
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
