//! The Paramount Digital template's two entry points: "Add section" at the
//! playhead, and "Template", which brands the whole video. The edits
//! themselves are [`crate::editor_template`]'s pure functions.

use super::*;
use crate::editor_template;

impl EditorWindow {
    pub(super) fn add_section_at_playhead(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_ready()
            || !edits::ensure_timeline(&mut self.project, &self.clip_display_durations)
        {
            return;
        }
        let time = self.view.preview_time.unwrap_or(self.playhead);
        let mut index = 0;
        if self.edit(
            |timeline| {
                index = editor_template::add_section_card(timeline, time);
                true
            },
            window,
            cx,
        ) {
            self.sync_template_lanes();
            self.set_selection(Some(Selection::single(TrackKind::Text, index)), cx);
            if let Some(start) = self
                .project
                .timeline
                .as_ref()
                .and_then(|timeline| timeline.text_segments.get(index))
                .map(|segment| segment.start)
            {
                self.seek_to_time(start + 0.8, cx);
            }
            self.note_edit("add-section", Some(TrackKind::Text));
        }
        cx.notify();
    }

    pub(super) fn apply_paramount_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_ready()
            || !edits::ensure_timeline(&mut self.project, &self.clip_display_durations)
        {
            return;
        }
        let logo = match editor_template::write_logo_png(&self.project_path) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not write the template logo");
                false
            }
        };
        let title = self
            .summary()
            .map(|summary| summary.pretty_name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Untitled video".to_string());
        let output = self
            .frame_layout
            .map_or([1920, 1080], |layout| layout.output_size);
        let aspect = f64::from(output[0]) / f64::from(output[1]).max(1.0);
        if self.edit_timeline_project(
            |project| editor_template::apply_template(project, &title, aspect, logo),
            window,
            cx,
        ) {
            self.sync_template_lanes();
            self.note_edit("apply-template", Some(TrackKind::Text));
        }
        cx.notify();
    }

    /// The template writes onto lanes the window has not opened yet; open
    /// them so every card and logo is drawn.
    fn sync_template_lanes(&mut self) {
        let Some(timeline) = self.project.timeline.as_ref() else {
            return;
        };
        let text = edits::used_lane_count(&timeline.text_segments);
        let image = edits::used_lane_count(&timeline.image_segments);
        if text > self.tracks.text || image > self.tracks.image {
            self.tracks.text = self.tracks.text.max(text);
            self.tracks.image = self.tracks.image.max(image);
            self.rebuild_timeline();
        }
    }
}
