use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_project::{
    ClipTransitionType, RecordingMeta, RecordingMetaInner, StudioRecordingMeta,
    TimelineConfiguration, TimelineSegment,
};

use crate::editor_edits::{self, SPLIT_EDGE_EPSILON};
use crate::{editor_clips, editor_timeline};

const BINS_PER_SECOND: f64 = 10.0;
const MIN_CUT_SECS: f64 = 0.2;
const CURSOR_MOVE_THRESHOLD_PX: f64 = 2.0;

pub const THRESHOLD_OPTIONS: [f32; 4] = [-50.0, -45.0, -40.0, -35.0];
pub const MIN_GAP_OPTIONS: [f64; 4] = [0.5, 1.0, 2.0, 3.0];
pub const PADDING_OPTIONS: [f64; 3] = [0.1, 0.25, 0.5];
pub const TRANSITION_OPTIONS: [AutoEditTransition; 4] = [
    AutoEditTransition::None,
    AutoEditTransition::Quick,
    AutoEditTransition::Smooth,
    AutoEditTransition::FadeThroughBlack,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoEditTransition {
    None,
    Quick,
    Smooth,
    FadeThroughBlack,
}

impl AutoEditTransition {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Hard cut",
            Self::Quick => "Quick fade",
            Self::Smooth => "Smooth",
            Self::FadeThroughBlack => "Dip to black",
        }
    }

    fn clip_transition(self) -> Option<(ClipTransitionType, f64)> {
        match self {
            Self::None => None,
            Self::Quick => Some((ClipTransitionType::CrossFade, 0.15)),
            Self::Smooth => Some((ClipTransitionType::CrossFade, 0.35)),
            Self::FadeThroughBlack => Some((ClipTransitionType::FadeThroughBlack, 0.5)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoEditSettings {
    pub threshold_db: f32,
    pub min_gap_secs: f64,
    pub padding_secs: f64,
    pub keep_screen_activity: bool,
    pub transition: AutoEditTransition,
}

impl Default for AutoEditSettings {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            min_gap_secs: 1.0,
            padding_secs: 0.25,
            keep_screen_activity: true,
            transition: AutoEditTransition::Quick,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoEditCut {
    pub clip_index: usize,
    pub recording_clip: u32,
    pub start: f64,
    pub end: f64,
}

pub fn removed_output_secs(timeline: &TimelineConfiguration, cuts: &[AutoEditCut]) -> f64 {
    cuts.iter()
        .map(|cut| {
            let timescale = timeline
                .segments
                .get(cut.clip_index)
                .map_or(1.0, |segment| segment.timescale);
            (cut.end - cut.start) / timescale
        })
        .sum()
}

/// Source-time ranges, per clip, where every audio track sits below the
/// threshold and (optionally) nothing moved, clicked or typed.
pub fn detect_empty_sections(
    timeline: &TimelineConfiguration,
    mic: &[Arc<Vec<f32>>],
    system: &[Arc<Vec<f32>>],
    activity: &[Vec<f64>],
    settings: AutoEditSettings,
) -> Vec<AutoEditCut> {
    let mut cuts = Vec::new();
    for (clip_index, segment) in timeline.segments.iter().enumerate() {
        let recording = segment.recording_clip as usize;
        let mic = mic.get(recording).map_or(&[][..], |table| table.as_slice());
        let system = system
            .get(recording)
            .map_or(&[][..], |table| table.as_slice());
        if mic.is_empty() && system.is_empty() && !settings.keep_screen_activity {
            continue;
        }
        let active_bins = activity
            .get(recording)
            .filter(|_| settings.keep_screen_activity)
            .map(|times| activity_bins(times))
            .unwrap_or_default();
        let quiet = |table: &[f32], bin: usize| {
            table
                .get(bin)
                .is_none_or(|level| *level < settings.threshold_db)
        };
        let is_empty = |bin: usize| {
            quiet(mic, bin) && quiet(system, bin) && !active_bins.get(bin).copied().unwrap_or(false)
        };

        let first_bin = (segment.start * BINS_PER_SECOND).ceil().max(0.0) as usize;
        let last_bin = (segment.end * BINS_PER_SECOND).floor().max(0.0) as usize;
        let mut bin = first_bin;
        while bin < last_bin {
            if !is_empty(bin) {
                bin += 1;
                continue;
            }
            let run_first = bin;
            while bin < last_bin && is_empty(bin) {
                bin += 1;
            }
            let touches_start = run_first == first_bin;
            let touches_end = bin == last_bin;
            let run_start = if touches_start {
                segment.start
            } else {
                run_first as f64 / BINS_PER_SECOND
            };
            let run_end = if touches_end {
                segment.end
            } else {
                bin as f64 / BINS_PER_SECOND
            };
            if (run_end - run_start) / segment.timescale < settings.min_gap_secs {
                continue;
            }
            let padding = settings.padding_secs * segment.timescale;
            let start = if touches_start {
                run_start
            } else {
                run_start + padding
            };
            let end = if touches_end {
                run_end
            } else {
                run_end - padding
            };
            if (end - start) / segment.timescale >= MIN_CUT_SECS {
                cuts.push(AutoEditCut {
                    clip_index,
                    recording_clip: segment.recording_clip,
                    start,
                    end,
                });
            }
        }
    }
    cuts
}

fn activity_bins(times: &[f64]) -> Vec<bool> {
    let Some(last) = times.iter().copied().fold(None, |max: Option<f64>, time| {
        Some(max.map_or(time, |max| max.max(time)))
    }) else {
        return Vec::new();
    };
    let mut bins = vec![false; (last * BINS_PER_SECOND).max(0.0) as usize + 1];
    for time in times {
        if *time >= 0.0 {
            bins[(time * BINS_PER_SECOND) as usize] = true;
        }
    }
    bins
}

/// Removes every cut from the clip track as one change, last first so the
/// clip indices the earlier cuts carry stay valid.
pub fn apply_cuts(
    timeline: &mut TimelineConfiguration,
    cuts: &[AutoEditCut],
    transition: AutoEditTransition,
) -> bool {
    let boundaries_before = clip_boundaries(&timeline.segments);
    let mut ordered = cuts.to_vec();
    ordered.sort_by(|a, b| {
        b.clip_index
            .cmp(&a.clip_index)
            .then(b.start.total_cmp(&a.start))
    });
    let mut changed = false;
    for cut in ordered {
        changed |= apply_cut(timeline, cut);
    }
    if changed && let Some((kind, duration)) = transition.clip_transition() {
        let created = clip_boundaries(&timeline.segments)
            .into_iter()
            .enumerate()
            .filter(|(_, boundary)| !boundaries_before.contains(boundary))
            .map(|(position, _)| position + 1)
            .collect::<Vec<_>>();
        for index in created {
            editor_clips::add_clip_transition(timeline, index, kind, duration);
        }
    }
    changed
}

type ClipBoundary = (u32, u64, u32, u64);

/// Each join between neighbouring clips, by what meets there, so the joins a
/// cut made can be told apart from the ones that were already on the track.
fn clip_boundaries(segments: &[TimelineSegment]) -> Vec<ClipBoundary> {
    segments
        .windows(2)
        .map(|pair| {
            (
                pair[0].recording_clip,
                pair[0].end.to_bits(),
                pair[1].recording_clip,
                pair[1].start.to_bits(),
            )
        })
        .collect()
}

fn apply_cut(timeline: &mut TimelineConfiguration, cut: AutoEditCut) -> bool {
    let index = cut.clip_index;
    let Some(segment) = timeline.segments.get(index).cloned() else {
        return false;
    };
    if segment.recording_clip != cut.recording_clip
        || cut.start < segment.start - SPLIT_EDGE_EPSILON
        || cut.end > segment.end + SPLIT_EDGE_EPSILON
    {
        return false;
    }
    let at_start = cut.start - segment.start <= SPLIT_EDGE_EPSILON;
    let at_end = segment.end - cut.end <= SPLIT_EDGE_EPSILON;
    let output_time = |timeline: &TimelineConfiguration, source: f64| {
        let offset = editor_timeline::clip_timeline_offsets(timeline)
            .get(index)
            .copied()
            .unwrap_or(0.0);
        editor_edits::effective_to_output(
            &timeline.hold_windows(),
            offset + (source - segment.start) / segment.timescale,
        )
    };

    match (at_start, at_end) {
        (true, true) => editor_edits::delete_clip_segments(timeline, &[index]),
        (true, false) => {
            let time = output_time(timeline, cut.end);
            editor_edits::split_clip_segment(timeline, time, Some(index))
                && editor_edits::delete_clip_segments(timeline, &[index])
        }
        (false, true) => {
            let time = output_time(timeline, cut.start);
            editor_edits::split_clip_segment(timeline, time, Some(index))
                && editor_edits::delete_clip_segments(timeline, &[index + 1])
        }
        (false, false) => {
            let end = output_time(timeline, cut.end);
            if !editor_edits::split_clip_segment(timeline, end, Some(index)) {
                return false;
            }
            let start = output_time(timeline, cut.start);
            editor_edits::split_clip_segment(timeline, start, Some(index))
                && editor_edits::delete_clip_segments(timeline, &[index + 1])
        }
    }
}

/// Seconds, per recording segment, at which the cursor moved, a button went
/// down or a key was pressed.
pub fn load_recording_activity(project_path: &Path) -> Vec<Vec<f64>> {
    let Ok(meta) = RecordingMeta::load_for_project(project_path) else {
        return Vec::new();
    };
    let RecordingMetaInner::Studio(studio) = &meta.inner else {
        return Vec::new();
    };
    match &**studio {
        StudioRecordingMeta::SingleSegment { segment } => {
            vec![cursor_activity(
                segment.cursor.as_ref().map(|cursor| meta.path(cursor)),
            )]
        }
        StudioRecordingMeta::MultipleSegments { inner } => inner
            .segments
            .iter()
            .map(|segment| {
                let mut times =
                    cursor_activity(segment.cursor.as_ref().map(|cursor| meta.path(cursor)));
                times.extend(
                    segment
                        .keyboard_events(&meta)
                        .presses
                        .iter()
                        .filter(|press| press.down)
                        .map(|press| press.time_ms / 1000.0),
                );
                times
            })
            .collect(),
    }
}

fn cursor_activity(path: Option<PathBuf>) -> Vec<f64> {
    let Some(events) = path.and_then(|path| cap_project::CursorEvents::load_from_file(&path).ok())
    else {
        return Vec::new();
    };
    let mut times = events
        .clicks
        .iter()
        .filter(|click| click.down)
        .map(|click| click.time_ms / 1000.0)
        .collect::<Vec<_>>();
    let mut previous: Option<(f64, f64)> = None;
    for event in &events.moves {
        let moved = previous.is_none_or(|(x, y)| {
            (event.x - x).abs() >= CURSOR_MOVE_THRESHOLD_PX
                || (event.y - y).abs() >= CURSOR_MOVE_THRESHOLD_PX
        });
        if moved {
            if previous.is_some() {
                times.push(event.time_ms / 1000.0);
            }
            previous = Some((event.x, event.y));
        }
    }
    times
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_project::ProjectConfiguration;

    fn timeline(mut json: serde_json::Value) -> TimelineConfiguration {
        if json.get("zoomSegments").is_none() {
            json["zoomSegments"] = serde_json::json!([]);
        }
        serde_json::from_value::<ProjectConfiguration>(serde_json::json!({ "timeline": json }))
            .expect("fixture parses")
            .timeline
            .expect("fixture has a timeline")
    }

    fn single_clip(end: f64) -> TimelineConfiguration {
        timeline(serde_json::json!({
            "segments": [{ "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": end }]
        }))
    }

    fn levels(seconds: f64, loud: &[(f64, f64)]) -> Arc<Vec<f32>> {
        let bins = (seconds * BINS_PER_SECOND) as usize;
        Arc::new(
            (0..bins)
                .map(|bin| {
                    let time = bin as f64 / BINS_PER_SECOND;
                    if loud
                        .iter()
                        .any(|(start, end)| time >= *start && time < *end)
                    {
                        -20.0
                    } else {
                        -60.0
                    }
                })
                .collect(),
        )
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_long_silence_between_speech_is_cut_with_padding() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (6.0, 10.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert!(approx(cuts[0].start, 3.25));
        assert!(approx(cuts[0].end, 5.75));
    }

    #[test]
    fn silence_shorter_than_the_minimum_gap_is_kept() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (3.5, 10.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert!(cuts.is_empty());
    }

    #[test]
    fn leading_and_trailing_silence_is_trimmed_without_padding() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(2.0, 8.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 2);
        assert!(approx(cuts[0].start, 0.0));
        assert!(approx(cuts[0].end, 1.75));
        assert!(approx(cuts[1].start, 8.25));
        assert!(approx(cuts[1].end, 10.0));
    }

    #[test]
    fn screen_activity_keeps_a_silent_section() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (6.0, 10.0)])];
        let activity = vec![vec![4.5]];
        let kept =
            detect_empty_sections(&timeline, &mic, &[], &activity, AutoEditSettings::default());
        assert!(kept.iter().all(|cut| cut.end <= 4.5 || cut.start > 4.6));

        let ignored = detect_empty_sections(
            &timeline,
            &mic,
            &[],
            &activity,
            AutoEditSettings {
                keep_screen_activity: false,
                ..AutoEditSettings::default()
            },
        );
        assert_eq!(ignored.len(), 1);
        assert!(approx(ignored[0].start, 3.25));
    }

    #[test]
    fn system_audio_counts_as_sound() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (6.0, 10.0)])];
        let system = vec![levels(10.0, &[(0.0, 10.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &system, &[], AutoEditSettings::default());
        assert!(cuts.is_empty());
    }

    #[test]
    fn applying_cuts_removes_the_empty_ranges_from_the_clip_track() {
        let mut timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(2.0, 4.0), (7.0, 9.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 3);
        let removed = removed_output_secs(&timeline, &cuts);

        assert!(apply_cuts(&mut timeline, &cuts, AutoEditTransition::None));

        let kept = timeline
            .segments
            .iter()
            .map(|segment| (segment.start, segment.end))
            .collect::<Vec<_>>();
        assert_eq!(kept.len(), 2);
        assert!(approx(kept[0].0, 1.75) && approx(kept[0].1, 4.25));
        assert!(approx(kept[1].0, 6.75) && approx(kept[1].1, 9.25));
        let total: f64 = timeline.segments.iter().map(|s| s.duration()).sum();
        assert!(approx(total, 10.0 - removed));
    }

    #[test]
    fn cuts_follow_the_clip_they_were_detected_in() {
        let mut timeline = timeline(serde_json::json!({
            "segments": [
                { "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 5.0 },
                { "recordingSegment": 0, "timescale": 2.0, "start": 5.0, "end": 15.0 }
            ]
        }));
        let mic = vec![levels(15.0, &[(0.0, 5.0), (5.0, 8.0), (12.0, 15.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].clip_index, 1);
        assert!(approx(cuts[0].start, 8.5));
        assert!(approx(cuts[0].end, 11.5));

        assert!(apply_cuts(&mut timeline, &cuts, AutoEditTransition::None));
        let ranges = timeline
            .segments
            .iter()
            .map(|segment| (segment.start, segment.end))
            .collect::<Vec<_>>();
        assert_eq!(ranges.len(), 3);
        assert!(approx(ranges[1].0, 5.0) && approx(ranges[1].1, 8.5));
        assert!(approx(ranges[2].0, 11.5) && approx(ranges[2].1, 15.0));
    }

    #[test]
    fn a_fully_silent_single_clip_is_never_deleted() {
        let mut timeline = single_clip(5.0);
        let mic = vec![levels(5.0, &[])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert!(!apply_cuts(&mut timeline, &cuts, AutoEditTransition::None));
        assert_eq!(timeline.segments.len(), 1);
    }

    #[test]
    fn transitions_land_only_on_the_joins_a_cut_made() {
        let mut timeline = timeline(serde_json::json!({
            "segments": [
                { "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 5.0 },
                { "recordingSegment": 0, "timescale": 1.0, "start": 5.0, "end": 15.0 }
            ]
        }));
        let mic = vec![levels(15.0, &[(0.0, 8.0), (12.0, 15.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);

        assert!(apply_cuts(&mut timeline, &cuts, AutoEditTransition::Quick));

        assert_eq!(timeline.segments.len(), 3);
        assert_eq!(timeline.transitions.len(), 1);
        let transition = timeline.effective_transition(2).expect("cut join fades");
        assert_eq!(transition.kind, ClipTransitionType::CrossFade);
        assert!(approx(transition.duration, 0.15));
        assert!(timeline.effective_transition(1).is_none());
    }

    #[test]
    fn a_transition_pulls_later_tracks_back_by_its_overlap() {
        let mut timeline = timeline(serde_json::json!({
            "segments": [{ "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 10.0 }],
            "zoomSegments": [{ "start": 8.0, "end": 9.0, "amount": 1.5, "mode": "auto" }]
        }));
        let mic = vec![levels(10.0, &[(0.0, 3.0), (6.0, 10.0)])];
        let cuts = detect_empty_sections(&timeline, &mic, &[], &[], AutoEditSettings::default());

        assert!(apply_cuts(
            &mut timeline,
            &cuts,
            AutoEditTransition::FadeThroughBlack
        ));

        let zoom = &timeline.zoom_segments[0];
        assert!(approx(zoom.start, 8.0 - 2.5 - 0.5), "{}", zoom.start);
        assert!(approx(zoom.end, 9.0 - 2.5 - 0.5), "{}", zoom.end);
    }
}
