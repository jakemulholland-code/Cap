//! The Paramount Digital video template: branded section cards, an intro and
//! outro, a logo watermark, and the brand frame and caption styling.
//!
//! Section cards are fullscreen text holds, so placing one pauses the
//! recording and inserts time; every output-timed track past the card is
//! pushed along by the card's length here, because nothing else does.

use std::path::Path;

use cap_project::{
    BackgroundSource, BorderConfiguration, ImageSegment, ProjectConfiguration, TextAnimation,
    TextLayout, TextSegment, TimelineConfiguration, XY,
};

use crate::editor_clips::{ripple_keyboard_track, ripple_track};
use crate::editor_edits;

pub const KICKER_FONT: &str = "Sora";
pub const HEADING_FONT: &str = "Fraunces";
const INK: &str = "#1a1a1a";
const TEAL_DARK: &str = "#008475";
const SURFACE_3: [u16; 3] = [251, 251, 252];
const LINE_2: [u16; 3] = [222, 222, 222];

pub const SECTION_CARD_SECS: f64 = 2.5;
pub const INTRO_CARD_SECS: f64 = 3.0;
pub const OUTRO_CARD_SECS: f64 = 3.5;

pub const INTRO_KICKER: &str = "PARAMOUNT DIGITAL";
pub const OUTRO_URL: &str = "paramountdigital.co.uk";
pub const WATERMARK_NAME: &str = "Paramount Digital logo";
pub const OUTRO_LOGO_NAME: &str = "Paramount Digital logo (outro)";
pub const LOGO_PROJECT_PATH: &str = "content/images/paramount-digital-logo.png";

const LOGO_SVG: &[u8] = include_bytes!("../assets/brand/paramount-logo.svg");
const LOGO_PNG_WIDTH: u32 = 1200;
const LOGO_ASPECT: f64 = 139.0 / 47.0;
const CARD_EPSILON: f64 = 0.01;

fn card_text(start: f64, end: f64, track: u32) -> TextSegment {
    TextSegment {
        layout: TextLayout::Fullscreen,
        layout_transition: 0.5,
        animation_in: TextAnimation::SlideUp,
        animation_in_duration: 0.5,
        animation_out: TextAnimation::Fade,
        animation_out_duration: 0.3,
        fade_duration: 0.3,
        background_color: None,
        stroke_width: 0.0,
        glow: 0.0,
        shadow: 0.0,
        gradient_color: None,
        ..editor_edits::default_text_segment(start, end, track)
    }
}

pub fn style_kicker(segment: &mut TextSegment) {
    segment.layout = TextLayout::Fullscreen;
    segment.center = XY::new(0.5, 0.415);
    segment.size = XY::new(0.6, 0.05);
    segment.font_family = KICKER_FONT.to_string();
    segment.font_size = 22.0;
    segment.font_weight = 600.0;
    segment.italic = false;
    segment.uppercase = true;
    segment.letter_spacing = 5.0;
    segment.color = TEAL_DARK.to_string();
}

pub fn style_heading(segment: &mut TextSegment) {
    segment.layout = TextLayout::Fullscreen;
    segment.center = XY::new(0.5, 0.52);
    segment.size = XY::new(0.8, 0.16);
    segment.font_family = HEADING_FONT.to_string();
    segment.font_size = 92.0;
    segment.font_weight = 900.0;
    segment.italic = false;
    segment.uppercase = false;
    segment.letter_spacing = -2.0;
    segment.line_height = 1.1;
    segment.color = INK.to_string();
}

fn style_caption_line(segment: &mut TextSegment) {
    segment.layout = TextLayout::Fullscreen;
    segment.center = XY::new(0.5, 0.64);
    segment.size = XY::new(0.6, 0.05);
    segment.font_family = KICKER_FONT.to_string();
    segment.font_size = 26.0;
    segment.font_weight = 500.0;
    segment.uppercase = false;
    segment.letter_spacing = 0.5;
    segment.color = TEAL_DARK.to_string();
}

fn is_kicker(segment: &TextSegment) -> bool {
    segment.font_family == KICKER_FONT && segment.uppercase
}

fn section_label(number: usize) -> String {
    format!("Section {number:02}")
}

/// Pushes every output-timed track at or past `at` along by `duration`, the
/// way a new fullscreen hold pushes the recording; the same set
/// `drop_clip_transition` ripples.
fn make_room(timeline: &mut TimelineConfiguration, at: f64, duration: f64) {
    ripple_track(&mut timeline.style_segments, at, duration);
    ripple_track(&mut timeline.image_segments, at, duration);
    ripple_track(&mut timeline.zoom_segments, at, duration);
    ripple_track(&mut timeline.scene_segments, at, duration);
    ripple_track(&mut timeline.mask_segments, at, duration);
    ripple_track(&mut timeline.text_segments, at, duration);
    ripple_track(&mut timeline.caption_segments, at, duration);
    ripple_track(&mut timeline.camera3d_segments, at, duration);
    ripple_track(&mut timeline.audio_segments, at, duration);
    ripple_keyboard_track(&mut timeline.keyboard_segments, at, duration);
}

/// The lowest text lane with nothing between `start` and `end`, skipping
/// `taken`.
fn free_text_lane(timeline: &TimelineConfiguration, start: f64, end: f64, taken: &[u32]) -> u32 {
    (0..)
        .find(|lane| {
            !taken.contains(lane)
                && !timeline.text_segments.iter().any(|segment| {
                    segment.track == *lane && segment.start < end && segment.end > start
                })
        })
        .unwrap_or(0)
}

fn free_image_lane(timeline: &TimelineConfiguration, start: f64, end: f64) -> u32 {
    (0..)
        .find(|lane| {
            !timeline
                .image_segments
                .iter()
                .any(|segment| segment.track == *lane && segment.start < end && segment.end > start)
        })
        .unwrap_or(0)
}

/// Where a card asked for at `time` actually lands: never inside another
/// card, and never past the end.
fn card_start(timeline: &TimelineConfiguration, time: f64) -> f64 {
    let mut at = time.clamp(0.0, timeline.duration());
    for (start, end) in timeline.hold_windows() {
        if at > start + CARD_EPSILON && at < end {
            at = end;
        }
    }
    at
}

/// Inserts a two-line card (kicker over heading) at `time` and returns the
/// heading's index in `text_segments` and the card's start.
fn insert_card(
    timeline: &mut TimelineConfiguration,
    time: f64,
    duration: f64,
    kicker: &str,
    heading: &str,
) -> (usize, f64) {
    let start = card_start(timeline, time);
    let end = start + duration;
    make_room(timeline, start, duration);
    let kicker_lane = free_text_lane(timeline, start, end, &[]);
    let heading_lane = free_text_lane(timeline, start, end, &[kicker_lane]);

    let mut kicker_segment = card_text(start, end, kicker_lane);
    kicker_segment.content = kicker.to_string();
    style_kicker(&mut kicker_segment);
    editor_edits::insert_text_segment(timeline, kicker_segment);

    let mut heading_segment = card_text(start, end, heading_lane);
    heading_segment.content = heading.to_string();
    heading_segment.animation_in_duration = 0.6;
    style_heading(&mut heading_segment);
    let index = editor_edits::insert_text_segment(timeline, heading_segment);
    (index, start)
}

/// "Add section": a branded card at `time`. Returns the heading's index so
/// the editor can select it for the user to type the section's name.
pub fn add_section_card(timeline: &mut TimelineConfiguration, time: f64) -> usize {
    let number = section_windows(timeline)
        .iter()
        .filter(|(start, _)| *start < time)
        .count()
        + 1;
    let (index, start) = insert_card(
        timeline,
        time,
        SECTION_CARD_SECS,
        &section_label(number),
        "Section title",
    );
    renumber_sections(timeline);
    timeline
        .text_segments
        .iter()
        .position(|segment| {
            (segment.start - start).abs() < CARD_EPSILON
                && segment.font_family == HEADING_FONT
                && segment.layout == TextLayout::Fullscreen
        })
        .unwrap_or(index)
}

fn card_segments(
    timeline: &TimelineConfiguration,
    start: f64,
    end: f64,
) -> impl Iterator<Item = (usize, &TextSegment)> {
    timeline
        .text_segments
        .iter()
        .enumerate()
        .filter(move |(_, segment)| {
            segment.enabled
                && segment.layout == TextLayout::Fullscreen
                && segment.start >= start - CARD_EPSILON
                && segment.end <= end + CARD_EPSILON
        })
}

fn is_intro(timeline: &TimelineConfiguration, (start, end): (f64, f64)) -> bool {
    start <= CARD_EPSILON
        && card_segments(timeline, start, end).any(|(_, segment)| segment.content == INTRO_KICKER)
}

fn is_outro(timeline: &TimelineConfiguration, (start, end): (f64, f64)) -> bool {
    card_segments(timeline, start, end).any(|(_, segment)| segment.content == OUTRO_URL)
}

/// Each card's span, grouped by the start its lines share. Back-to-back
/// cards merge into one hold window, so holds cannot tell cards apart.
pub fn card_windows(timeline: &TimelineConfiguration) -> Vec<(f64, f64)> {
    let mut spans = timeline
        .text_segments
        .iter()
        .filter(|segment| {
            segment.enabled
                && segment.layout == TextLayout::Fullscreen
                && segment.end > segment.start
        })
        .map(|segment| (segment.start, segment.end))
        .collect::<Vec<_>>();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut windows: Vec<(f64, f64)> = Vec::new();
    for (start, end) in spans {
        match windows.last_mut() {
            Some(window) if (start - window.0).abs() < CARD_EPSILON => window.1 = window.1.max(end),
            _ => windows.push((start, end)),
        }
    }
    windows
}

/// Every card that is a section rather than the intro or outro.
pub fn section_windows(timeline: &TimelineConfiguration) -> Vec<(f64, f64)> {
    card_windows(timeline)
        .into_iter()
        .filter(|window| !is_intro(timeline, *window) && !is_outro(timeline, *window))
        .collect()
}

/// Brands every section card and numbers the kickers in timeline order. A
/// fullscreen text the user made by hand gets the heading style and a kicker.
fn renumber_sections(timeline: &mut TimelineConfiguration) {
    for (number, (start, end)) in section_windows(timeline).into_iter().enumerate() {
        let members = card_segments(timeline, start, end)
            .map(|(index, segment)| (index, is_kicker(segment)))
            .collect::<Vec<_>>();
        let mut has_kicker = false;
        for (index, kicker) in members {
            let segment = &mut timeline.text_segments[index];
            if kicker && !has_kicker {
                segment.content = section_label(number + 1);
                style_kicker(segment);
                has_kicker = true;
            } else {
                style_heading(segment);
            }
        }
        if !has_kicker {
            let taken = card_segments(timeline, start, end)
                .map(|(_, segment)| segment.track)
                .collect::<Vec<_>>();
            let lane = free_text_lane(timeline, start, end, &taken);
            let mut kicker = card_text(start, end, lane);
            kicker.content = section_label(number + 1);
            style_kicker(&mut kicker);
            editor_edits::insert_text_segment(timeline, kicker);
        }
    }
}

fn logo_segment(start: f64, end: f64, track: u32, width: f64, frame_aspect: f64) -> ImageSegment {
    ImageSegment {
        start,
        end,
        track,
        path: LOGO_PROJECT_PATH.to_string(),
        size: XY::new(width, width * frame_aspect / LOGO_ASPECT),
        lock_aspect: true,
        ..Default::default()
    }
}

/// Applies the whole template as one change. `frame_aspect` is the output
/// width over height; `logo` says whether the logo PNG is in the project.
pub fn apply_template(
    project: &mut ProjectConfiguration,
    title: &str,
    frame_aspect: f64,
    logo: bool,
) -> bool {
    style_frame(project);
    style_captions(project);
    let Some(timeline) = project.timeline.as_mut() else {
        return true;
    };

    if !card_windows(timeline)
        .into_iter()
        .any(|window| is_intro(timeline, window))
    {
        insert_card(timeline, 0.0, INTRO_CARD_SECS, INTRO_KICKER, title);
    }
    if !card_windows(timeline)
        .into_iter()
        .any(|window| is_outro(timeline, window))
    {
        add_outro(timeline, frame_aspect, logo);
    }
    renumber_sections(timeline);
    if logo {
        add_watermark(timeline, frame_aspect);
    }
    true
}

fn add_outro(timeline: &mut TimelineConfiguration, frame_aspect: f64, logo: bool) {
    let start = timeline.duration();
    let end = start + OUTRO_CARD_SECS;
    let line_lane = free_text_lane(timeline, start, end, &[]);
    let mut line = card_text(start, end, line_lane);
    line.content = OUTRO_URL.to_string();
    style_caption_line(&mut line);
    editor_edits::insert_text_segment(timeline, line);
    if logo {
        let lane = free_image_lane(timeline, start, end);
        let mut image = logo_segment(start, end, lane, 0.34, frame_aspect);
        image.name = OUTRO_LOGO_NAME.to_string();
        image.center = XY::new(0.5, 0.47);
        editor_edits::insert_image_segment(timeline, image);
    }
}

/// A small logo bottom-right from the end of the intro to the start of the
/// outro, replacing any earlier watermark.
fn add_watermark(timeline: &mut TimelineConfiguration, frame_aspect: f64) {
    timeline
        .image_segments
        .retain(|segment| segment.name != WATERMARK_NAME);
    let holds = card_windows(timeline);
    let start = holds
        .iter()
        .find(|window| is_intro(timeline, **window))
        .map_or(0.0, |(_, end)| *end);
    let end = holds
        .iter()
        .find(|window| is_outro(timeline, **window))
        .map_or_else(|| timeline.duration(), |(start, _)| *start);
    if end - start <= CARD_EPSILON {
        return;
    }
    let lane = free_image_lane(timeline, start, end);
    let width = 0.1;
    let mut image = logo_segment(start, end, lane, width, frame_aspect);
    image.name = WATERMARK_NAME.to_string();
    image.opacity = 0.9;
    image.center = XY::new(
        1.0 - 0.03 - image.size.x / 2.0,
        1.0 - 0.045 - image.size.y / 2.0,
    );
    editor_edits::insert_image_segment(timeline, image);
}

fn style_frame(project: &mut ProjectConfiguration) {
    let background = &mut project.background;
    background.source = BackgroundSource::Color {
        value: SURFACE_3,
        alpha: 255,
    };
    background.blur = 0.0;
    background.padding = 6.0;
    background.rounding = 4.0;
    background.shadow = 24.0;
    background.border = Some(BorderConfiguration {
        enabled: true,
        width: 2.0,
        color: LINE_2,
        opacity: 100.0,
    });
}

fn style_captions(project: &mut ProjectConfiguration) {
    if let Some(captions) = project.captions.as_mut() {
        let settings = &mut captions.settings;
        settings.font = KICKER_FONT.to_string();
        settings.font_weight = 500;
        settings.color = INK.to_string();
        settings.background_color = "#FFFFFF".to_string();
        settings.background_opacity = 92;
        settings.highlight_color = TEAL_DARK.to_string();
        settings.outline = false;
    }
    if let Some(keyboard) = project.keyboard.as_mut() {
        let settings = &mut keyboard.settings;
        settings.font = KICKER_FONT.to_string();
        settings.font_weight = 600;
        settings.color = INK.to_string();
        settings.background_color = "#FFFFFF".to_string();
        settings.background_opacity = 92;
    }
}

/// Rasterises the bundled logo into the project, where image segments
/// resolve their paths.
pub fn write_logo_png(project_path: &Path) -> Result<(), String> {
    let tree = resvg::usvg::Tree::from_data(LOGO_SVG, &resvg::usvg::Options::default())
        .map_err(|error| format!("logo svg: {error}"))?;
    let size = tree.size();
    let scale = LOGO_PNG_WIDTH as f32 / size.width();
    let height = (size.height() * scale).ceil() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(LOGO_PNG_WIDTH, height)
        .ok_or_else(|| "logo pixmap".to_string())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let path = project_path.join(LOGO_PROJECT_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    pixmap
        .save_png(&path)
        .map_err(|error| format!("logo png: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(json: serde_json::Value) -> ProjectConfiguration {
        let mut timeline = json;
        if timeline.get("zoomSegments").is_none() {
            timeline["zoomSegments"] = serde_json::json!([]);
        }
        serde_json::from_value(serde_json::json!({ "timeline": timeline })).expect("fixture parses")
    }

    fn twenty_seconds() -> ProjectConfiguration {
        project(serde_json::json!({
            "segments": [{ "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 20.0 }]
        }))
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_section_card_inserts_time_and_pushes_later_tracks() {
        let mut project = project(serde_json::json!({
            "segments": [{ "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 20.0 }],
            "zoomSegments": [{ "start": 12.0, "end": 14.0, "amount": 1.5, "mode": "auto" }]
        }));
        let timeline = project.timeline.as_mut().unwrap();

        let heading = add_section_card(timeline, 10.0);

        assert!(approx(timeline.duration(), 20.0 + SECTION_CARD_SECS));
        assert_eq!(
            timeline.hold_windows(),
            vec![(10.0, 10.0 + SECTION_CARD_SECS)]
        );
        assert!(approx(
            timeline.zoom_segments[0].start,
            12.0 + SECTION_CARD_SECS
        ));
        let heading = &timeline.text_segments[heading];
        assert_eq!(heading.font_family, HEADING_FONT);
        let kicker = timeline
            .text_segments
            .iter()
            .find(|segment| is_kicker(segment))
            .unwrap();
        assert_eq!(kicker.content, "Section 01");
        assert_ne!(kicker.track, heading.track);
    }

    #[test]
    fn sections_are_numbered_in_timeline_order() {
        let mut project = twenty_seconds();
        let timeline = project.timeline.as_mut().unwrap();
        add_section_card(timeline, 15.0);
        add_section_card(timeline, 5.0);

        let mut kickers = timeline
            .text_segments
            .iter()
            .filter(|segment| is_kicker(segment))
            .map(|segment| (segment.start, segment.content.clone()))
            .collect::<Vec<_>>();
        kickers.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(kickers[0].1, "Section 01");
        assert!(approx(kickers[0].0, 5.0));
        assert_eq!(kickers[1].1, "Section 02");
        assert!(approx(kickers[1].0, 15.0 + SECTION_CARD_SECS));
    }

    #[test]
    fn a_card_asked_for_inside_another_lands_after_it() {
        let mut project = twenty_seconds();
        let timeline = project.timeline.as_mut().unwrap();
        add_section_card(timeline, 5.0);
        add_section_card(timeline, 6.0);
        assert_eq!(
            card_windows(timeline),
            vec![
                (5.0, 5.0 + SECTION_CARD_SECS),
                (5.0 + SECTION_CARD_SECS, 5.0 + 2.0 * SECTION_CARD_SECS)
            ]
        );
        let mut kickers = timeline
            .text_segments
            .iter()
            .filter(|segment| is_kicker(segment))
            .map(|segment| segment.content.clone())
            .collect::<Vec<_>>();
        kickers.sort();
        assert_eq!(kickers, vec!["Section 01", "Section 02"]);
    }

    #[test]
    fn the_template_adds_intro_outro_and_watermark_once() {
        let mut project = twenty_seconds();
        add_section_card(project.timeline.as_mut().unwrap(), 10.0);

        assert!(apply_template(&mut project, "My video", 16.0 / 9.0, true));
        assert!(apply_template(&mut project, "My video", 16.0 / 9.0, true));

        let timeline = project.timeline.as_ref().unwrap();
        let expected = 20.0 + SECTION_CARD_SECS + INTRO_CARD_SECS + OUTRO_CARD_SECS;
        assert!(
            approx(timeline.duration(), expected),
            "{}",
            timeline.duration()
        );
        assert_eq!(card_windows(timeline).len(), 3);
        assert!(
            timeline
                .text_segments
                .iter()
                .any(|s| s.content == "My video")
        );
        assert_eq!(
            timeline
                .text_segments
                .iter()
                .filter(|s| s.content == "Section 01")
                .count(),
            1
        );
        let watermarks = timeline
            .image_segments
            .iter()
            .filter(|segment| segment.name == WATERMARK_NAME)
            .collect::<Vec<_>>();
        assert_eq!(watermarks.len(), 1);
        assert!(approx(watermarks[0].start, INTRO_CARD_SECS));
        assert!(approx(watermarks[0].end, expected - OUTRO_CARD_SECS));
        assert_eq!(
            timeline
                .image_segments
                .iter()
                .filter(|segment| segment.name == OUTRO_LOGO_NAME)
                .count(),
            1
        );
        assert!(matches!(
            project.background.source,
            BackgroundSource::Color {
                value: SURFACE_3,
                ..
            }
        ));
    }

    #[test]
    fn a_hand_made_fullscreen_text_becomes_a_branded_section() {
        let mut project = project(serde_json::json!({
            "segments": [{ "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 20.0 }],
            "textSegments": [{
                "start": 8.0, "end": 10.0, "content": "Pricing", "layout": "fullscreen"
            }]
        }));

        apply_template(&mut project, "Video", 16.0 / 9.0, false);

        let timeline = project.timeline.as_ref().unwrap();
        let heading = timeline
            .text_segments
            .iter()
            .find(|segment| segment.content == "Pricing")
            .unwrap();
        assert_eq!(heading.font_family, HEADING_FONT);
        assert!(
            timeline
                .text_segments
                .iter()
                .any(|segment| segment.content == "Section 01" && is_kicker(segment))
        );
    }

    #[test]
    fn the_bundled_logo_rasterises() {
        let directory = tempfile::tempdir().unwrap();
        write_logo_png(directory.path()).unwrap();
        assert!(directory.path().join(LOGO_PROJECT_PATH).is_file());
    }
}
