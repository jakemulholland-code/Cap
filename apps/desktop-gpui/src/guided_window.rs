//! Guided recording: a Claude Motion video and its transcript, split into the
//! sections you record one at a time. The split itself is pure logic in
//! [`crate::guided_recording`]; this window picks the files, runs it off the
//! main thread, and lets you check and move each section's start.

use std::{path::PathBuf, sync::Arc};

use gpui::{
    Context, FocusHandle, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    RenderImage, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, StyledImage as _,
    Window, div, prelude::FluentBuilder, px,
};

use crate::guided_recording::{self, BoundarySource, GuidedSection, GuidedSession};
use crate::theme::{LocaleFont as _, Theme};
use crate::ui::{Button, ButtonSize, ButtonVariant};

mod recorder;

pub const GUIDED_WIDTH: f32 = 900.;
pub const GUIDED_HEIGHT: f32 = 680.;
pub const GUIDED_MIN_WIDTH: f32 = 680.;
pub const GUIDED_MIN_HEIGHT: f32 = 440.;

const THUMB_WIDTH: f32 = 128.;
const THUMB_HEIGHT: f32 = 72.;
/// Frames are taken this far into a section, past the fade into its slide.
const THUMB_OFFSET_SECS: f64 = 0.6;
const NUDGE_SECS: f64 = 0.5;
/// A read this much longer than its slide is flagged.
const OVERRUN_SECS: f64 = 2.0;

enum Stage {
    Choose,
    Preparing,
    Ready,
    Record,
    Failed(String),
}

pub struct GuidedRecordingWindow {
    theme: Theme,
    focus: FocusHandle,
    stage: Stage,
    video: Option<PathBuf>,
    script: Option<PathBuf>,
    session: Option<GuidedSession>,
    thumbnails: Vec<Option<Arc<RenderImage>>>,
    recent: Vec<(String, PathBuf)>,
    scroll: ScrollHandle,
    recorder: recorder::Recorder,
}

impl GuidedRecordingWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.on_window_should_close(cx, |_window, cx| {
            cx.defer(crate::app_windows::guided_recording_closed);
            true
        });
        crate::theme::bind_window(window, cx);
        Self {
            theme: Theme::for_window(window, cx, false),
            focus: cx.focus_handle(),
            stage: Stage::Choose,
            video: None,
            script: None,
            session: None,
            thumbnails: Vec::new(),
            recent: recent_sessions(),
            scroll: ScrollHandle::new(),
            recorder: recorder::Recorder::default(),
        }
    }

    pub fn focus_root(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    fn pick_video(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) = crate::import::pick_import_file(
                &[("Video Files", crate::import::VIDEO_IMPORT_EXTENSIONS)],
                cx,
            )
            .await
            else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.video = Some(path);
                this.prepare_if_ready(window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn pick_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) =
                crate::import::pick_import_file(&[("Transcript", &["md", "markdown", "txt"])], cx)
                    .await
            else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.script = Some(path);
                this.prepare_if_ready(window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn prepare_if_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(video), Some(script)) = (self.video.clone(), self.script.clone()) else {
            cx.notify();
            return;
        };
        self.stage = Stage::Preparing;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { guided_recording::prepare_session(&video, &script) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(session) => {
                    if let Err(error) = session.save() {
                        tracing::warn!(%error, "could not save the guided session");
                    }
                    this.show_session(session, window, cx);
                }
                Err(error) => {
                    this.stage = Stage::Failed(error);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn open_saved(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let session = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|json| {
                serde_json::from_str::<GuidedSession>(&json).map_err(|e| e.to_string())
            });
        match session {
            Ok(session) => {
                self.video = Some(session.video_path.clone());
                self.script = Some(session.script_path.clone());
                self.show_session(session, window, cx);
            }
            Err(error) => {
                self.stage = Stage::Failed(format!("Could not open {}: {error}", path.display()));
                cx.notify();
            }
        }
    }

    fn show_session(
        &mut self,
        session: GuidedSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.thumbnails = vec![None; session.sections.len()];
        self.session = Some(session);
        self.stage = Stage::Ready;
        self.recent = recent_sessions();
        for index in 0..self.thumbnails.len() {
            self.load_thumbnail(index, window, cx);
        }
        cx.notify();
    }

    fn start_over(&mut self, cx: &mut Context<Self>) {
        self.stage = Stage::Choose;
        self.session = None;
        self.video = None;
        self.script = None;
        self.thumbnails.clear();
        self.recent = recent_sessions();
        cx.notify();
    }

    fn load_thumbnail(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(section) = session.sections.get(index) else {
            return;
        };
        let time = (section.start + THUMB_OFFSET_SECS).min(section.end);
        let path = session.video_path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let image = cx
                .background_executor()
                .spawn(async move { crate::editor_clips::decode_thumbnail_frame(&path, time) })
                .await;
            this.update_in(cx, |this, _window, cx| {
                match image {
                    Ok(image) => {
                        if let Some(slot) = this.thumbnails.get_mut(index) {
                            *slot = Some(image);
                        }
                    }
                    Err(error) => tracing::warn!(%error, index, "guided thumbnail failed"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn move_start(
        &mut self,
        index: usize,
        start: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &mut self.session else {
            return;
        };
        session.move_boundary(index, start);
        if let Err(error) = session.save() {
            tracing::warn!(%error, "could not save the guided session");
        }
        self.load_thumbnail(index, window, cx);
        cx.notify();
    }

    fn nudge(&mut self, index: usize, delta: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(start) = self
            .session
            .as_ref()
            .and_then(|s| s.sections.get(index))
            .map(|s| s.start)
        else {
            return;
        };
        self.move_start(index, start + delta, window, cx);
    }

    fn snap(&mut self, index: usize, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self
            .session
            .as_ref()
            .and_then(|session| session.neighbouring_cut(index, forward))
        else {
            return;
        };
        self.move_start(index, cut, window, cx);
    }

    fn button(&self, id: impl Into<SharedString>, label: &str, variant: ButtonVariant) -> Button {
        Button::plain(
            &self.theme,
            gpui::ElementId::Name(id.into()),
            variant,
            ButtonSize::Sm,
        )
        .label(label.to_string())
    }

    fn render_choose(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let file_row = |label: &'static str, path: &Option<PathBuf>, button: Button| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(div().w(px(90.)).text_size(px(13.)).child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(Hsla::from(theme.gray_11))
                        .child(path.as_ref().map_or("Not chosen".to_string(), |path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        })),
                )
                .child(button)
        };

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(24.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(Hsla::from(theme.gray_11))
                    .child(
                        "Pick the Claude Motion video and its transcript. Each `## 0:14 Heading` \
                         becomes a section, lined up with the slide change in the video.",
                    ),
            )
            .child(file_row(
                "Video",
                &self.video,
                self.button("guided-pick-video", "Choose video", ButtonVariant::Gray)
                    .on_click(cx.listener(|this, _, window, cx| this.pick_video(window, cx))),
            ))
            .child(file_row(
                "Transcript",
                &self.script,
                self.button(
                    "guided-pick-script",
                    "Choose transcript",
                    ButtonVariant::Gray,
                )
                .on_click(cx.listener(|this, _, window, cx| this.pick_script(window, cx))),
            ))
            .when(!self.recent.is_empty(), |this| {
                this.child(
                    div()
                        .mt(px(12.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(Hsla::from(theme.gray_11))
                        .child("Recent"),
                )
                .children(
                    self.recent
                        .iter()
                        .enumerate()
                        .map(|(index, (title, path))| {
                            let path = path.clone();
                            div()
                                .id(("guided-recent", index))
                                .px(px(10.))
                                .py(px(6.))
                                .rounded(px(6.))
                                .text_size(px(13.))
                                .hover(move |style| style.bg(Hsla::from(theme.gray_3)))
                                .child(title.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_saved(path.clone(), window, cx)
                                }))
                        }),
                )
            })
    }

    fn render_section(
        &self,
        index: usize,
        section: &GuidedSection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let read = section.read_secs();
        let overrun = read - section.duration() > OVERRUN_SECS;
        let source = match section.source {
            BoundarySource::Start => "start",
            BoundarySource::SlideChange => "slide change",
            BoundarySource::Script => "script time",
            BoundarySource::Estimate => "estimated",
            BoundarySource::Manual => "moved",
        };
        let thumbnail = self.thumbnails.get(index).cloned().flatten();
        let first = index == 0;

        div()
            .flex()
            .gap(px(14.))
            .p(px(12.))
            .border_b_1()
            .border_color(Hsla::from(theme.gray_4))
            .child(
                div()
                    .flex_none()
                    .w(px(THUMB_WIDTH))
                    .h(px(THUMB_HEIGHT))
                    .rounded(px(6.))
                    .overflow_hidden()
                    .bg(Hsla::from(theme.gray_3))
                    .when_some(thumbnail, |this, image| {
                        this.child(
                            gpui::img(image)
                                .size_full()
                                .object_fit(gpui::ObjectFit::Cover),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(format!("{}. {}", index + 1, section.title)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(Hsla::from(theme.gray_11))
                                    .child(format!(
                                        "{} – {} · {:.1}s · {source}",
                                        clock(section.start),
                                        clock(section.end),
                                        section.duration()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if overrun {
                                Hsla::from(theme.amber_11)
                            } else {
                                Hsla::from(theme.gray_11)
                            })
                            .child(format!(
                                "{} words · about {:.0}s to read{}",
                                section.word_count(),
                                read,
                                if overrun {
                                    " · longer than the slide, which will hold"
                                } else {
                                    ""
                                }
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_clamp(2)
                            .text_color(Hsla::from(theme.gray_12))
                            .child(section.text.replace("\n\n", " ")),
                    ),
            )
            .when(!first, |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex()
                                .gap(px(4.))
                                .child(
                                    self.button(
                                        format!("guided-prev-cut-{index}"),
                                        "◀ Slide",
                                        ButtonVariant::Gray,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.snap(index, false, window, cx)
                                        },
                                    )),
                                )
                                .child(
                                    self.button(
                                        format!("guided-next-cut-{index}"),
                                        "Slide ▶",
                                        ButtonVariant::Gray,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.snap(index, true, window, cx)
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(4.))
                                .child(
                                    self.button(
                                        format!("guided-earlier-{index}"),
                                        "−0.5s",
                                        ButtonVariant::Outline,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.nudge(index, -NUDGE_SECS, window, cx)
                                        },
                                    )),
                                )
                                .child(
                                    self.button(
                                        format!("guided-later-{index}"),
                                        "+0.5s",
                                        ButtonVariant::Outline,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.nudge(index, NUDGE_SECS, window, cx)
                                        },
                                    )),
                                ),
                        ),
                )
            })
    }

    fn render_ready(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let Some(session) = &self.session else {
            return div();
        };
        let overruns = session
            .sections
            .iter()
            .filter(|section| section.read_secs() - section.duration() > OVERRUN_SECS)
            .count();

        let rows: Vec<_> = session
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| self.render_section(index, section, cx).into_any_element())
            .collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(Hsla::from(theme.gray_4))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(Hsla::from(theme.gray_11))
                            .child(format!(
                                "{} sections over {} · {overruns} read longer than their slide",
                                session.sections.len(),
                                clock(session.video_duration)
                            )),
                    )
                    .child(
                        self.button("guided-start-over", "Start over", ButtonVariant::Outline)
                            .on_click(cx.listener(|this, _, _window, cx| this.start_over(cx))),
                    )
                    .child(
                        self.button(
                            "guided-open-recorder",
                            "Record sections",
                            ButtonVariant::Primary,
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.open_recorder(cx))),
                    ),
            )
            .child(
                div()
                    .id("guided-sections")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows),
            )
    }
}

impl Render for GuidedRecordingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.theme.refresh(window, cx, false);
        let theme = self.theme;
        let title = self
            .session
            .as_ref()
            .map_or("Guided recording".to_string(), |session| {
                session.title.clone()
            });

        let body = match &self.stage {
            Stage::Choose => self.render_choose(cx).into_any_element(),
            Stage::Preparing => div()
                .p(px(24.))
                .text_size(px(13.))
                .text_color(Hsla::from(theme.gray_11))
                .child("Reading the transcript and finding the slide changes…")
                .into_any_element(),
            Stage::Ready => self.render_ready(cx).into_any_element(),
            Stage::Record => self.render_recorder(window, cx),
            Stage::Failed(error) => div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(24.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(Hsla::from(theme.red_9))
                        .child(error.clone()),
                )
                .child(
                    self.button("guided-retry", "Start over", ButtonVariant::Outline)
                        .on_click(cx.listener(|this, _, _window, cx| this.start_over(cx))),
                )
                .into_any_element(),
        };

        let shell = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(this.stage, Stage::Record)
                    && this.recorder_key(event.keystroke.key.as_str(), window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(Hsla::from(theme.gray_1))
            .font_family(crate::theme::UI_FONT)
            .locale_font()
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_primary)
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(16.))
                    .border_b_1()
                    .border_color(Hsla::from(theme.gray_4))
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    ),
            )
            .child(body);

        #[cfg(target_os = "windows")]
        let shell = shell.child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .h(px(40.))
                .flex()
                .justify_end()
                .child(crate::ui::windows_caption_controls(
                    theme,
                    window.is_window_active(),
                    window.is_maximized(),
                    true,
                    true,
                )),
        );

        shell
    }
}

fn clock(secs: f64) -> String {
    let secs = secs.max(0.0);
    let minutes = (secs / 60.0).floor();
    format!("{}:{:04.1}", minutes as u64, secs - minutes * 60.0)
}

/// Saved sessions, newest first.
fn recent_sessions() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(guided_recording::sessions_dir()) else {
        return Vec::new();
    };
    let mut sessions: Vec<(std::time::SystemTime, String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path().join("session.json");
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let json = std::fs::read_to_string(&path).ok()?;
            let session: GuidedSession = serde_json::from_str(&json).ok()?;
            Some((modified, session.title, path))
        })
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.0));
    sessions
        .into_iter()
        .take(8)
        .map(|(_, title, path)| (title, path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn clock_formats_minutes_and_tenths() {
        assert_eq!(clock(0.0), "0:00.0");
        assert_eq!(clock(13.85), "0:13.8");
        assert_eq!(clock(294.02), "4:54.0");
    }
}
