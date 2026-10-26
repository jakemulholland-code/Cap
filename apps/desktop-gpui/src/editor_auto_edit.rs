use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_project::{
    CaptionSegment, ClipTransitionType, RecordingMeta, RecordingMetaInner, StudioRecordingMeta,
    TimelineConfiguration, TimelineSegment,
};

use crate::editor_edits::{self, SPLIT_EDGE_EPSILON};
use crate::{editor_clips, editor_timeline};

const BINS_PER_SECOND: f64 = 10.0;
const MIN_CUT_SECS: f64 = 0.2;
const CURSOR_MOVE_THRESHOLD_PX: f64 = 2.0;
/// What the waveform table holds for a window of exact digital silence, which
/// is quieter than any real room, not louder.
const DIGITAL_SILENCE_DB: f32 = -60.0;
const LEVEL_FLOOR_DB: f32 = -120.0;
const FALLBACK_THRESHOLD_DB: f32 = -65.0;
const MIN_LEVEL_SPREAD_DB: f32 = 15.0;
const THRESHOLD_BELOW_SPEECH_DB: f32 = 10.0;
const NOISE_QUIET_BINS: usize = 2;
const NOISE_MAX_SECS: f64 = 2.0;
const NOISE_MAX_SECS_WITHOUT_TRANSCRIPT: f64 = 1.2;
const NOISE_SHORT_SECS: f64 = 0.3;
const NOISE_ABOVE_SPEECH_DB: f32 = 8.0;
const NOISE_PADDING_SECS: f64 = 0.05;
const NOISE_WORD_MARGIN_SECS: f64 = 0.1;
/// Transcribers stretch a word over the silence after it, so a word is taken
/// to last no longer than this when checking what a burst of sound was.
const MAX_WORD_SECS: f64 = 1.0;
const CUT_MERGE_GAP_SECS: f64 = 0.1;
const RETAKE_LEAD_SECS: f64 = 0.05;
const RETAKE_KEY_WORDS: usize = 6;
const RETAKE_LOOKBACK_SECS: f64 = 60.0;
const RETAKE_CHAIN_SECS: f64 = 10.0;
const RETAKE_PAUSE_SECS: f64 = 0.6;
const RETAKE_FALLBACK_LOOKBACK_SECS: f64 = 30.0;
const REPEAT_WORDS: usize = 4;
const REPEAT_WINDOW_SECS: f64 = 15.0;
const REPEAT_PAUSE_SECS: f64 = 0.25;

const FILLER_WORDS: [&str; 8] = ["um", "uh", "uhm", "erm", "er", "ah", "hmm", "mm"];
const LEAD_IN_WORDS: [&str; 7] = ["so", "okay", "ok", "right", "alright", "and", "now"];
const RETAKE_CUES: [&str; 25] = [
    "restarting that section",
    "restart that section",
    "restarting this section",
    "restart this section",
    "go back and start that again",
    "let me start that again",
    "let me try that again",
    "let me say that again",
    "let me start again",
    "let me try again",
    "let me redo that",
    "start that again",
    "start this again",
    "starting that again",
    "start again",
    "start over",
    "starting over",
    "try that again",
    "say that again",
    "redo that",
    "restarting that",
    "restart that",
    "scratch that",
    "take two",
    "from the top",
];

pub const THRESHOLD_OPTIONS: [Option<f32>; 5] =
    [None, Some(-75.0), Some(-65.0), Some(-55.0), Some(-45.0)];
pub const MIN_GAP_OPTIONS: [f64; 4] = [0.5, 1.0, 2.0, 3.0];
pub const PADDING_OPTIONS: [f64; 3] = [0.1, 0.25, 0.5];
pub const RETAKE_OPTIONS: [RetakeMode; 3] =
    [RetakeMode::CueAndRepeats, RetakeMode::Cue, RetakeMode::Keep];
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

/// How retakes are found in the transcript: from a spoken cue such as
/// "restarting that section", and optionally from a line said twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetakeMode {
    Keep,
    Cue,
    CueAndRepeats,
}

impl RetakeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Keep => "Keep them",
            Self::Cue => "Spoken cue",
            Self::CueAndRepeats => "Cue + repeats",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoEditSettings {
    /// `None` measures the recording and picks the threshold itself.
    pub threshold_db: Option<f32>,
    pub min_gap_secs: f64,
    pub padding_secs: f64,
    pub keep_screen_activity: bool,
    pub cut_noises: bool,
    pub retakes: RetakeMode,
    pub transition: AutoEditTransition,
}

impl Default for AutoEditSettings {
    fn default() -> Self {
        Self {
            threshold_db: None,
            min_gap_secs: 1.0,
            padding_secs: 0.25,
            keep_screen_activity: true,
            cut_noises: true,
            retakes: RetakeMode::CueAndRepeats,
            transition: AutoEditTransition::Quick,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CutReason {
    Silence,
    Noise,
    Retake,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoEditCut {
    pub clip_index: usize,
    pub recording_clip: u32,
    pub start: f64,
    pub end: f64,
    pub reason: CutReason,
}

/// The two levels a recording's audio sits at, room tone and speech, and the
/// silence threshold between them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioLevels {
    pub room_db: f32,
    pub speech_db: f32,
    pub threshold_db: f32,
}

/// A transcript word in its recording segment's own time, lower-cased and
/// stripped of punctuation so lines can be compared.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptWord {
    pub recording: u32,
    pub start: f64,
    pub end: f64,
    pub token: String,
    pub ends_sentence: bool,
    pub ends_clause: bool,
}

impl TranscriptWord {
    fn is_filler(&self) -> bool {
        FILLER_WORDS.contains(&self.token.as_str())
    }

    fn is_lead_in(&self) -> bool {
        self.is_filler() || LEAD_IN_WORDS.contains(&self.token.as_str())
    }
}

/// Flattens source-timed caption segments into words, splitting the
/// concatenated source time back into recording segments.
pub fn transcript_words(
    segments: &[CaptionSegment],
    recording_durations: &[f64],
) -> Vec<TranscriptWord> {
    let mut offsets = Vec::with_capacity(recording_durations.len());
    let mut total = 0.0;
    for duration in recording_durations {
        offsets.push(total);
        total += duration;
    }
    let mut words = segments
        .iter()
        .flat_map(|segment| &segment.words)
        .filter_map(|word| {
            let token = normalize_token(&word.text);
            if token.is_empty() {
                return None;
            }
            let start = f64::from(word.start);
            let recording = offsets
                .iter()
                .rposition(|offset| *offset <= start + f64::EPSILON)
                .unwrap_or(0);
            let offset = offsets.get(recording).copied().unwrap_or(0.0);
            let text = word.text.trim_end();
            let ends_sentence = text.ends_with(['.', '?', '!']);
            Some(TranscriptWord {
                recording: recording as u32,
                start: start - offset,
                end: f64::from(word.end).max(start) - offset,
                token,
                ends_sentence,
                ends_clause: ends_sentence
                    || text.ends_with([',', ';', ':', '-', '\u{2014}', '\u{2026}']),
            })
        })
        .collect::<Vec<_>>();
    words.sort_by(|a, b| {
        a.recording
            .cmp(&b.recording)
            .then(a.start.total_cmp(&b.start))
    });
    words
}

fn normalize_token(text: &str) -> String {
    let token = text
        .chars()
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    token.trim_matches('\'').to_string()
}

/// Every cut auto edit would make: silence, coughs and other short noises,
/// and retakes found in the transcript, merged so no two overlap.
pub fn detect_cuts(
    timeline: &TimelineConfiguration,
    mic: &[Arc<Vec<f32>>],
    system: &[Arc<Vec<f32>>],
    activity: &[Vec<f64>],
    words: &[TranscriptWord],
    settings: AutoEditSettings,
) -> Vec<AutoEditCut> {
    let mut cuts = detect_empty_sections(timeline, mic, system, activity, words, settings);
    cuts.extend(retake_cuts(timeline, words, settings.retakes));
    merge_cuts(cuts)
}

pub fn count_by_reason(cuts: &[AutoEditCut], reason: CutReason) -> usize {
    cuts.iter().filter(|cut| cut.reason == reason).count()
}

fn merge_cuts(mut cuts: Vec<AutoEditCut>) -> Vec<AutoEditCut> {
    cuts.sort_by(|a, b| {
        a.clip_index
            .cmp(&b.clip_index)
            .then(a.start.total_cmp(&b.start))
    });
    let mut merged: Vec<AutoEditCut> = Vec::with_capacity(cuts.len());
    for cut in cuts {
        if let Some(last) = merged.last_mut()
            && last.clip_index == cut.clip_index
            && cut.start <= last.end + CUT_MERGE_GAP_SECS
        {
            last.end = last.end.max(cut.end);
            last.reason = last.reason.max(cut.reason);
            continue;
        }
        merged.push(cut);
    }
    merged
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

/// Where a cut sits on the output timeline, for moving the playhead to it.
pub fn cut_output_range(timeline: &TimelineConfiguration, cut: &AutoEditCut) -> Option<(f64, f64)> {
    let segment = timeline.segments.get(cut.clip_index)?;
    let offset = *editor_timeline::clip_timeline_offsets(timeline).get(cut.clip_index)?;
    let holds = timeline.hold_windows();
    let output = |source: f64| {
        editor_edits::effective_to_output(
            &holds,
            offset + (source - segment.start) / segment.timescale,
        )
    };
    Some((output(cut.start), output(cut.end)))
}

fn table(tables: &[Arc<Vec<f32>>], recording: u32) -> &[f32] {
    tables
        .get(recording as usize)
        .map_or(&[][..], |table| table.as_slice())
}

fn level_at(table: &[f32], bin: usize) -> f32 {
    match table.get(bin) {
        Some(level) if level.is_finite() && *level != DIGITAL_SILENCE_DB => {
            level.max(LEVEL_FLOOR_DB)
        }
        _ => LEVEL_FLOOR_DB,
    }
}

fn bin_range(segment: &TimelineSegment) -> (usize, usize) {
    (
        (segment.start * BINS_PER_SECOND).ceil().max(0.0) as usize,
        (segment.end * BINS_PER_SECOND).floor().max(0.0) as usize,
    )
}

/// Splits the levels on the timeline into room tone and speech (Otsu's
/// method over a 1 dB histogram) and puts the threshold midway between the
/// two, so it suits whatever microphone and gain the recording was made with.
pub fn measure_levels(
    timeline: &TimelineConfiguration,
    mic: &[Arc<Vec<f32>>],
    system: &[Arc<Vec<f32>>],
) -> Option<AudioLevels> {
    const BUCKETS: usize = 121;
    let mut histogram = [0.0_f64; BUCKETS];
    for segment in &timeline.segments {
        let mic = table(mic, segment.recording_clip);
        let system = table(system, segment.recording_clip);
        if mic.is_empty() && system.is_empty() {
            continue;
        }
        let (first_bin, last_bin) = bin_range(segment);
        for bin in first_bin..last_bin {
            let level = level_at(mic, bin).max(level_at(system, bin));
            histogram[(level - LEVEL_FLOOR_DB).round().clamp(0.0, 120.0) as usize] += 1.0;
        }
    }
    let total = histogram.iter().sum::<f64>();
    if total < 20.0 {
        return None;
    }
    let sum = histogram
        .iter()
        .enumerate()
        .map(|(bucket, count)| bucket as f64 * count)
        .sum::<f64>();
    let mut weight_low = 0.0;
    let mut sum_low = 0.0;
    let mut best: Option<(f64, f64, f64)> = None;
    for (split, count) in histogram.iter().enumerate().take(BUCKETS - 1) {
        weight_low += count;
        sum_low += split as f64 * count;
        let weight_high = total - weight_low;
        if weight_low < 0.5 || weight_high < 0.5 {
            continue;
        }
        let mean_low = sum_low / weight_low;
        let mean_high = (sum - sum_low) / weight_high;
        let variance = weight_low * weight_high * (mean_high - mean_low).powi(2);
        if best.is_none_or(|(best_variance, ..)| variance > best_variance) {
            best = Some((variance, mean_low, mean_high));
        }
    }
    let (_, mean_low, mean_high) = best?;
    let room_db = mean_low as f32 + LEVEL_FLOOR_DB;
    let speech_db = mean_high as f32 + LEVEL_FLOOR_DB;
    if speech_db - room_db < MIN_LEVEL_SPREAD_DB {
        return None;
    }
    Some(AudioLevels {
        room_db,
        speech_db,
        threshold_db: ((room_db + speech_db) / 2.0).min(speech_db - THRESHOLD_BELOW_SPEECH_DB),
    })
}

pub fn resolve_threshold(settings: AutoEditSettings, levels: Option<AudioLevels>) -> f32 {
    settings
        .threshold_db
        .or(levels.map(|levels| levels.threshold_db))
        .unwrap_or(FALLBACK_THRESHOLD_DB)
}

/// Source-time ranges, per clip, where every audio track sits below the
/// threshold (or only carries a cough-like burst) and, optionally, nothing
/// moved, clicked or typed.
pub fn detect_empty_sections(
    timeline: &TimelineConfiguration,
    mic: &[Arc<Vec<f32>>],
    system: &[Arc<Vec<f32>>],
    activity: &[Vec<f64>],
    words: &[TranscriptWord],
    settings: AutoEditSettings,
) -> Vec<AutoEditCut> {
    let levels = measure_levels(timeline, mic, system);
    let threshold = resolve_threshold(settings, levels);
    let mut cuts = Vec::new();
    for (clip_index, segment) in timeline.segments.iter().enumerate() {
        let mic = table(mic, segment.recording_clip);
        let system = table(system, segment.recording_clip);
        if mic.is_empty() && system.is_empty() && !settings.keep_screen_activity {
            continue;
        }
        let active_bins = activity
            .get(segment.recording_clip as usize)
            .filter(|_| settings.keep_screen_activity)
            .map(|times| activity_bins(times))
            .unwrap_or_default();
        let (first_bin, last_bin) = bin_range(segment);
        let bin_levels = (first_bin..last_bin)
            .map(|bin| level_at(mic, bin).max(level_at(system, bin)))
            .collect::<Vec<_>>();
        let noise = if settings.cut_noises {
            let spoken = (!words.is_empty()).then(|| {
                words
                    .iter()
                    .filter(|word| word.recording == segment.recording_clip)
                    .map(|word| (word.start, word.end.min(word.start + MAX_WORD_SECS)))
                    .collect::<Vec<_>>()
            });
            noise_mask(
                &bin_levels,
                threshold,
                first_bin,
                spoken.as_deref(),
                levels.map(|levels| levels.speech_db),
            )
        } else {
            vec![false; bin_levels.len()]
        };
        let is_noise = |bin: usize| noise.get(bin - first_bin).copied().unwrap_or(false);
        let is_empty = |bin: usize| {
            (bin_levels
                .get(bin - first_bin)
                .is_none_or(|level| *level < threshold)
                || is_noise(bin))
                && !active_bins.get(bin).copied().unwrap_or(false)
        };

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
            let noise_first = (run_first..bin).find(|bin| is_noise(*bin));
            let noise_last = (run_first..bin).rev().find(|bin| is_noise(*bin));
            let long_enough = (run_end - run_start) / segment.timescale >= settings.min_gap_secs;
            if !long_enough && noise_first.is_none() {
                continue;
            }
            // Padding keeps a breath of room tone, but never so much that it
            // leaves the edge of a cough behind.
            let padding = |room_secs: Option<f64>| {
                let padding = room_secs.map_or(settings.padding_secs, |room| {
                    settings
                        .padding_secs
                        .min(room.max(NOISE_PADDING_SECS) - NOISE_PADDING_SECS)
                        .max(0.0)
                });
                padding * segment.timescale
            };
            let start = if touches_start {
                run_start
            } else {
                run_start + padding(noise_first.map(|bin| bin as f64 / BINS_PER_SECOND - run_start))
            };
            let end = if touches_end {
                run_end
            } else {
                run_end
                    - padding(noise_last.map(|bin| run_end - (bin + 1) as f64 / BINS_PER_SECOND))
            };
            let has_noise = noise_first.is_some();
            if (end - start) / segment.timescale >= MIN_CUT_SECS {
                cuts.push(AutoEditCut {
                    clip_index,
                    recording_clip: segment.recording_clip,
                    start,
                    end,
                    reason: if has_noise {
                        CutReason::Noise
                    } else {
                        CutReason::Silence
                    },
                });
            }
        }
    }
    cuts
}

/// Marks short bursts of sound with quiet either side: a cough, a sniff, a
/// desk knock. With a transcript, a burst no word was heard in is noise; without
/// one, only a burst much louder than speech, or a very short one, counts.
fn noise_mask(
    levels: &[f32],
    threshold: f32,
    first_bin: usize,
    spoken: Option<&[(f64, f64)]>,
    speech_db: Option<f32>,
) -> Vec<bool> {
    let max_secs = if spoken.is_some() {
        NOISE_MAX_SECS
    } else {
        NOISE_MAX_SECS_WITHOUT_TRANSCRIPT
    };
    let max_bins = (max_secs * BINS_PER_SECOND).round() as usize;
    let short_bins = (NOISE_SHORT_SECS * BINS_PER_SECOND).round() as usize;
    let loud = levels
        .iter()
        .map(|level| *level >= threshold)
        .collect::<Vec<_>>();
    let mut mask = vec![false; loud.len()];
    let mut index = 0;
    while index < loud.len() {
        if !loud[index] {
            index += 1;
            continue;
        }
        let run_start = index;
        while index < loud.len() && loud[index] {
            index += 1;
        }
        let run_end = index;
        if run_end - run_start > max_bins || (run_start == 0 && run_end == loud.len()) {
            continue;
        }
        let quiet_before = loud[..run_start]
            .iter()
            .rev()
            .take_while(|loud| !**loud)
            .count();
        let quiet_after = loud[run_end..].iter().take_while(|loud| !**loud).count();
        let isolated = (quiet_before >= NOISE_QUIET_BINS || quiet_before == run_start)
            && (quiet_after >= NOISE_QUIET_BINS || quiet_after == loud.len() - run_end);
        if !isolated {
            continue;
        }
        let is_noise = match spoken {
            Some(spoken) => {
                let start =
                    (first_bin + run_start) as f64 / BINS_PER_SECOND - NOISE_WORD_MARGIN_SECS;
                let end = (first_bin + run_end) as f64 / BINS_PER_SECOND + NOISE_WORD_MARGIN_SECS;
                !spoken
                    .iter()
                    .any(|(word_start, word_end)| *word_start < end && *word_end > start)
            }
            None => {
                let peak = levels[run_start..run_end]
                    .iter()
                    .copied()
                    .fold(LEVEL_FLOOR_DB, f32::max);
                run_end - run_start <= short_bins
                    || speech_db.is_some_and(|speech| peak >= speech + NOISE_ABOVE_SPEECH_DB)
            }
        };
        if is_noise {
            mask[run_start..run_end].fill(true);
        }
    }
    mask
}

/// Retakes as per-clip cuts, each running from where the abandoned attempt
/// began to where the line is picked up again.
fn retake_cuts(
    timeline: &TimelineConfiguration,
    words: &[TranscriptWord],
    mode: RetakeMode,
) -> Vec<AutoEditCut> {
    let mut cuts = Vec::new();
    for (recording, start, end) in retake_ranges(words, mode) {
        for (clip_index, segment) in timeline.segments.iter().enumerate() {
            if segment.recording_clip != recording {
                continue;
            }
            let start = start.max(segment.start);
            let end = end.min(segment.end);
            if (end - start) / segment.timescale >= MIN_CUT_SECS {
                cuts.push(AutoEditCut {
                    clip_index,
                    recording_clip: recording,
                    start,
                    end,
                    reason: CutReason::Retake,
                });
            }
        }
    }
    cuts
}

/// Recording-time ranges to drop as retakes, found per recording segment.
fn retake_ranges(words: &[TranscriptWord], mode: RetakeMode) -> Vec<(u32, f64, f64)> {
    let mut ranges = Vec::new();
    if mode == RetakeMode::Keep {
        return ranges;
    }
    let mut cues = RETAKE_CUES
        .iter()
        .map(|cue| cue.split(' ').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    cues.sort_by_key(|cue| std::cmp::Reverse(cue.len()));
    for group in words.chunk_by(|a, b| a.recording == b.recording) {
        let recording = group[0].recording;
        let content = (0..group.len())
            .filter(|index| !group[*index].is_filler())
            .collect::<Vec<_>>();
        let token = |position: usize| group[content[position]].token.as_str();

        let mut position = 0;
        while position < content.len() {
            let Some(cue) = cues.iter().find(|cue| {
                position + cue.len() <= content.len()
                    && cue
                        .iter()
                        .enumerate()
                        .all(|(offset, word)| token(position + offset) == *word)
            }) else {
                position += 1;
                continue;
            };
            let cue_first = content[position];
            let cue_last = content[position + cue.len() - 1];
            let resume = position + cue.len();
            let mut key = resume;
            while key < content.len() && group[content[key]].is_lead_in() {
                key += 1;
            }
            let attempt = matching_attempt(&content, group, position, key)
                .unwrap_or_else(|| sentence_start(group, cue_first));
            let start = cut_start(group, absorb_lead_ins(group, attempt));
            let end = match content.get(resume) {
                Some(index) => group[*index].start - RETAKE_LEAD_SECS,
                None => group[cue_last].end + RETAKE_LEAD_SECS,
            };
            if end > start {
                ranges.push((recording, start, end));
            }
            position += cue.len();
        }

        if mode == RetakeMode::CueAndRepeats {
            ranges.extend(
                repeated_lines(&content, group)
                    .into_iter()
                    .map(|(start, end)| (recording, start, end)),
            );
        }
    }
    ranges
}

fn cut_start(group: &[TranscriptWord], index: usize) -> f64 {
    let start = group[index].start - RETAKE_LEAD_SECS;
    match index.checked_sub(1) {
        Some(previous) => start.max(group[previous].end),
        None => start.max(0.0),
    }
}

/// Pulls a cut's start back over the "um, so" that led into the attempt.
fn absorb_lead_ins(group: &[TranscriptWord], mut index: usize) -> usize {
    while index > 0
        && group[index - 1].is_lead_in()
        && group[index].start - group[index - 1].end < RETAKE_PAUSE_SECS
    {
        index -= 1;
    }
    index
}

/// The word where the abandoned attempt began: the place before the cue that
/// opens most like the line the speaker resumed with (same first word, then
/// the most words in common), pulled back to any earlier false start of it.
fn matching_attempt(
    content: &[usize],
    group: &[TranscriptWord],
    cue: usize,
    key: usize,
) -> Option<usize> {
    let key_len = RETAKE_KEY_WORDS.min(content.len().saturating_sub(key));
    if key_len < 2 {
        return None;
    }
    let needed = key_len.div_ceil(2).max(2);
    let tokens = |from: usize, to: usize| {
        (from..to)
            .map(|position| group[content[position]].token.as_str())
            .collect::<Vec<_>>()
    };
    let resumed = tokens(key, key + key_len);
    let score = |position: usize| {
        if group[content[position]].token != resumed[0] {
            return 0;
        }
        common_words(
            &tokens(position, (position + key_len + 2).min(cue)),
            &resumed,
        )
    };
    let cue_time = group[content[cue]].start;
    let mut best: Option<(usize, usize)> = None;
    for position in (0..cue).rev() {
        if group[content[position]].start < cue_time - RETAKE_LOOKBACK_SECS {
            break;
        }
        let score = score(position);
        if score >= needed && best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, position));
        }
    }
    let (_, mut found) = best?;
    for position in (0..found).rev() {
        if group[content[position]].start < group[content[found]].start - RETAKE_CHAIN_SECS {
            break;
        }
        if score(position) >= needed {
            found = position;
        }
    }
    Some(content[found])
}

/// The longest common subsequence of two short word lists.
fn common_words(a: &[&str], b: &[&str]) -> usize {
    let mut row = vec![0; b.len() + 1];
    for word in a {
        let mut diagonal = 0;
        for (column, other) in b.iter().enumerate() {
            let above = row[column + 1];
            row[column + 1] = if word == other {
                diagonal + 1
            } else {
                above.max(row[column])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

/// With no opener to match, the attempt is taken to be the sentence the cue
/// was said in, back to the last full stop or clear pause.
fn sentence_start(group: &[TranscriptWord], cue_first: usize) -> usize {
    let cue_time = group[cue_first].start;
    let mut index = cue_first;
    while index > 0
        && !group[index - 1].ends_sentence
        && group[index].start - group[index - 1].end < RETAKE_PAUSE_SECS
        && cue_time - group[index - 1].start <= RETAKE_FALLBACK_LOOKBACK_SECS
    {
        index -= 1;
    }
    index
}

/// A line restarted without a cue: at a break (punctuation, a filler or a
/// pause), the speaker repeats the opening words of something said moments
/// before, so the first attempt goes.
fn repeated_lines(content: &[usize], group: &[TranscriptWord]) -> Vec<(f64, f64)> {
    let token = |position: usize| group[content[position]].token.as_str();
    let mut ranges = Vec::new();
    let mut floor = 0;
    for again in REPEAT_WORDS..content.len() {
        if again + REPEAT_WORDS > content.len() {
            break;
        }
        let index = content[again];
        let at_break = index.checked_sub(1).is_none_or(|previous| {
            group[previous].ends_clause
                || group[previous].is_filler()
                || group[index].start - group[previous].end >= REPEAT_PAUSE_SECS
        });
        if !at_break || again < floor + REPEAT_WORDS {
            continue;
        }
        let first = (floor..=again - REPEAT_WORDS).find(|first| {
            group[content[*first]].start >= group[index].start - REPEAT_WINDOW_SECS
                && (0..REPEAT_WORDS).all(|offset| token(first + offset) == token(again + offset))
        });
        if let Some(first) = first {
            ranges.push((
                cut_start(group, absorb_lead_ins(group, content[first])),
                group[index].start - RETAKE_LEAD_SECS,
            ));
            floor = again;
        }
    }
    ranges
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
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert!(approx(cuts[0].start, 3.25));
        assert!(approx(cuts[0].end, 5.75));
    }

    #[test]
    fn silence_shorter_than_the_minimum_gap_is_kept() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (3.5, 10.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert!(cuts.is_empty());
    }

    #[test]
    fn leading_and_trailing_silence_is_trimmed_without_padding() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(2.0, 8.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
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
        let kept = detect_empty_sections(
            &timeline,
            &mic,
            &[],
            &activity,
            &[],
            AutoEditSettings::default(),
        );
        assert!(kept.iter().all(|cut| cut.end <= 4.5 || cut.start > 4.6));

        let ignored = detect_empty_sections(
            &timeline,
            &mic,
            &[],
            &activity,
            &[],
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
        let cuts = detect_empty_sections(
            &timeline,
            &mic,
            &system,
            &[],
            &[],
            AutoEditSettings::default(),
        );
        assert!(cuts.is_empty());
    }

    #[test]
    fn applying_cuts_removes_the_empty_ranges_from_the_clip_track() {
        let mut timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(2.0, 4.0), (7.0, 9.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
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
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
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
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
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
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
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
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());

        assert!(apply_cuts(
            &mut timeline,
            &cuts,
            AutoEditTransition::FadeThroughBlack
        ));

        let zoom = &timeline.zoom_segments[0];
        assert!(approx(zoom.start, 8.0 - 2.5 - 0.5), "{}", zoom.start);
        assert!(approx(zoom.end, 9.0 - 2.5 - 0.5), "{}", zoom.end);
    }

    fn word(text: &str, start: f64, end: f64) -> TranscriptWord {
        let segments = [CaptionSegment {
            words: vec![cap_project::CaptionWord {
                text: text.into(),
                start: start as f32,
                end: end as f32,
            }],
            ..Default::default()
        }];
        transcript_words(&segments, &[])
            .pop()
            .expect("fixture word has letters")
    }

    fn spoken(script: &[(&str, f64)]) -> Vec<TranscriptWord> {
        script
            .iter()
            .map(|(text, start)| word(text, *start, start + 0.3))
            .collect()
    }

    fn restarted_intro() -> Vec<TranscriptWord> {
        spoken(&[
            ("Today", 0.0),
            ("we'll", 0.3),
            ("look", 0.6),
            ("at", 0.9),
            ("the", 1.2),
            ("um", 1.6),
            ("restarting", 2.0),
            ("that", 2.3),
            ("section.", 2.6),
            ("Okay,", 4.0),
            ("today", 4.4),
            ("we'll", 4.7),
            ("look", 5.0),
            ("at", 5.3),
            ("settings", 5.6),
        ])
    }

    fn levels_db(seconds: f64, room: f32, sounds: &[(f64, f64, f32)]) -> Arc<Vec<f32>> {
        let bins = (seconds * BINS_PER_SECOND) as usize;
        Arc::new(
            (0..bins)
                .map(|bin| {
                    let time = bin as f64 / BINS_PER_SECOND;
                    sounds
                        .iter()
                        .find(|(start, end, _)| time >= *start && time < *end)
                        .map_or(room, |(_, _, level)| *level)
                })
                .collect(),
        )
    }

    #[test]
    fn the_threshold_sits_between_the_room_and_the_speech() {
        let timeline = single_clip(20.0);
        let mic = vec![levels_db(
            20.0,
            -100.0,
            &[(1.0, 6.0, -48.0), (9.0, 15.0, -50.0), (16.0, 19.0, -46.0)],
        )];
        let levels = measure_levels(&timeline, &mic, &[]).expect("two clear levels");
        assert!((-102.0..-98.0).contains(&levels.room_db), "{levels:?}");
        assert!((-50.0..-46.0).contains(&levels.speech_db), "{levels:?}");
        assert!((-90.0..-58.0).contains(&levels.threshold_db), "{levels:?}");

        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 4);
        assert!(approx(cuts[1].start, 6.25) && approx(cuts[1].end, 8.75));
    }

    #[test]
    fn digital_silence_counts_as_the_quietest_room() {
        let timeline = single_clip(10.0);
        let mic = vec![levels_db(
            10.0,
            -60.0,
            &[(0.0, 3.0), (6.0, 10.0)].map(|(s, e)| (s, e, -70.0)),
        )];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert!(approx(cuts[0].start, 3.25));
    }

    #[test]
    fn a_cough_between_pauses_is_cut_with_the_silence_around_it() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (4.5, 4.8), (6.0, 10.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].reason, CutReason::Noise);
        assert!(approx(cuts[0].start, 3.25));
        assert!(approx(cuts[0].end, 5.75));

        let kept = detect_empty_sections(
            &timeline,
            &mic,
            &[],
            &[],
            &[],
            AutoEditSettings {
                cut_noises: false,
                ..AutoEditSettings::default()
            },
        );
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|cut| cut.reason == CutReason::Silence));
    }

    #[test]
    fn a_cough_louder_than_speech_is_cut_even_with_short_pauses() {
        let timeline = single_clip(10.0);
        let mic = vec![levels_db(
            10.0,
            -100.0,
            &[(0.0, 4.0, -48.0), (4.3, 5.1, -22.0), (5.4, 10.0, -48.0)],
        )];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].reason, CutReason::Noise);
        assert!(cuts[0].start <= 4.3 && cuts[0].end >= 5.1, "{:?}", cuts[0]);
        assert!(cuts[0].start >= 4.0 && cuts[0].end <= 5.4, "{:?}", cuts[0]);
    }

    #[test]
    fn a_short_burst_the_transcript_heard_as_a_word_is_kept() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (4.5, 4.8), (6.0, 10.0)])];
        let words = spoken(&[("Yes.", 4.5)]);
        let cuts = detect_empty_sections(
            &timeline,
            &mic,
            &[],
            &[],
            &words,
            AutoEditSettings::default(),
        );
        assert_eq!(cuts.len(), 2);
        assert!(cuts.iter().all(|cut| cut.end <= 4.5 || cut.start >= 4.8));
    }

    #[test]
    fn a_burst_right_against_speech_is_not_a_cough() {
        let timeline = single_clip(10.0);
        let mic = vec![levels(10.0, &[(0.0, 3.0), (3.1, 3.4), (6.0, 10.0)])];
        let cuts =
            detect_empty_sections(&timeline, &mic, &[], &[], &[], AutoEditSettings::default());
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].reason, CutReason::Silence);
        assert!(approx(cuts[0].start, 3.65));
    }

    #[test]
    fn a_spoken_cue_rolls_back_to_where_the_resumed_line_first_began() {
        let ranges = retake_ranges(&restarted_intro(), RetakeMode::Cue);
        assert_eq!(ranges.len(), 1);
        let (recording, start, end) = ranges[0];
        assert_eq!(recording, 0);
        assert!(approx(start, 0.0));
        assert!(approx(end, 3.95));
    }

    #[test]
    fn a_cue_with_no_matching_opener_rolls_back_to_the_sentence_start() {
        let words = spoken(&[
            ("First", 0.0),
            ("line", 0.3),
            ("done.", 0.6),
            ("This", 1.0),
            ("bit", 1.3),
            ("went", 1.6),
            ("wrong,", 1.9),
            ("scratch", 2.2),
            ("that.", 2.5),
            ("Here", 5.0),
            ("is", 5.3),
            ("better", 5.6),
        ]);
        let ranges = retake_ranges(&words, RetakeMode::Cue);
        assert_eq!(ranges.len(), 1);
        assert!(approx(ranges[0].1, 0.95));
        assert!(approx(ranges[0].2, 4.95));
    }

    #[test]
    fn a_line_restarted_after_a_pause_drops_the_first_attempt() {
        let words = spoken(&[
            ("In", 0.0),
            ("this", 0.3),
            ("video", 0.6),
            ("we'll", 0.9),
            ("uh", 1.2),
            ("In", 3.0),
            ("this", 3.3),
            ("video", 3.6),
            ("we'll", 3.9),
            ("cover", 4.2),
            ("settings", 4.5),
        ]);
        assert!(retake_ranges(&words, RetakeMode::Cue).is_empty());
        let ranges = retake_ranges(&words, RetakeMode::CueAndRepeats);
        assert_eq!(ranges.len(), 1);
        assert!(approx(ranges[0].1, 0.0));
        assert!(approx(ranges[0].2, 2.95));
    }

    #[test]
    fn retakes_split_across_clips_and_merge_with_silence() {
        let timeline = timeline(serde_json::json!({
            "segments": [
                { "recordingSegment": 0, "timescale": 1.0, "start": 0.0, "end": 2.0 },
                { "recordingSegment": 0, "timescale": 1.0, "start": 2.0, "end": 10.0 }
            ]
        }));
        let mic = vec![levels(10.0, &[(0.0, 10.0)])];
        let cuts = detect_cuts(
            &timeline,
            &mic,
            &[],
            &[],
            &restarted_intro(),
            AutoEditSettings {
                retakes: RetakeMode::Cue,
                ..AutoEditSettings::default()
            },
        );
        assert_eq!(cuts.len(), 2);
        assert!(cuts.iter().all(|cut| cut.reason == CutReason::Retake));
        assert_eq!((cuts[0].clip_index, cuts[1].clip_index), (0, 1));
        assert!(approx(cuts[0].start, 0.0) && approx(cuts[0].end, 2.0));
        assert!(approx(cuts[1].start, 2.0) && approx(cuts[1].end, 3.95));

        let merged = merge_cuts(vec![
            AutoEditCut {
                clip_index: 0,
                recording_clip: 0,
                start: 1.0,
                end: 3.0,
                reason: CutReason::Silence,
            },
            AutoEditCut {
                clip_index: 0,
                recording_clip: 0,
                start: 2.5,
                end: 5.0,
                reason: CutReason::Retake,
            },
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].reason, CutReason::Retake);
        assert!(approx(merged[0].start, 1.0) && approx(merged[0].end, 5.0));
    }

    #[test]
    fn transcript_words_split_back_into_recording_segments() {
        let segments = [CaptionSegment {
            id: "a".into(),
            start: 1.0,
            end: 13.0,
            text: "Today, we\u{2019}ll".into(),
            words: vec![
                cap_project::CaptionWord {
                    text: " Today,".into(),
                    start: 1.0,
                    end: 1.4,
                },
                cap_project::CaptionWord {
                    text: "We\u{2019}ll.".into(),
                    start: 12.0,
                    end: 12.5,
                },
                cap_project::CaptionWord {
                    text: "...".into(),
                    start: 12.5,
                    end: 12.6,
                },
            ],
        }];
        let words = transcript_words(&segments, &[10.0, 10.0]);
        assert_eq!(words.len(), 2);
        assert_eq!((words[0].recording, words[0].token.as_str()), (0, "today"));
        assert!(words[0].ends_clause && !words[0].ends_sentence);
        assert_eq!((words[1].recording, words[1].token.as_str()), (1, "we'll"));
        assert!(words[1].ends_sentence);
        assert!(approx(words[1].start, 2.0));
    }

    #[test]
    fn cut_output_range_follows_earlier_clips() {
        let timeline = timeline(serde_json::json!({
            "segments": [
                { "recordingSegment": 0, "timescale": 1.0, "start": 2.0, "end": 5.0 },
                { "recordingSegment": 0, "timescale": 2.0, "start": 8.0, "end": 14.0 }
            ]
        }));
        let cut = AutoEditCut {
            clip_index: 1,
            recording_clip: 0,
            start: 10.0,
            end: 12.0,
            reason: CutReason::Silence,
        };
        let (start, end) = cut_output_range(&timeline, &cut).expect("clip exists");
        assert!(approx(start, 4.0) && approx(end, 5.0));
    }

    /// Mean-abs dB per 0.1 s of a real 36 s HyperX Quadcast take: speech near
    /// -48 dB, room tone below -80 dB and a pair of coughs at 10.4 s.
    const RECORDED_LEVELS: [f32; 359] = [
        -117.2, -99.9, -115.9, -120.0, -94.9, -94.2, -99.8, -110.3, -109.5, -110.5, -120.0, -88.4,
        -48.2, -45.3, -55.0, -44.2, -45.5, -57.8, -50.7, -45.8, -57.7, -50.2, -46.2, -47.7, -51.9,
        -47.6, -60.3, -54.2, -47.8, -68.8, -57.8, -51.3, -45.4, -66.6, -75.0, -97.3, -99.8, -80.9,
        -50.8, -48.2, -64.0, -44.6, -49.3, -49.2, -54.9, -59.9, -57.7, -49.3, -50.2, -59.6, -50.5,
        -62.3, -49.2, -45.9, -50.6, -46.9, -54.6, -46.0, -41.1, -41.8, -45.7, -58.3, -91.1, -97.7,
        -101.9, -102.7, -99.7, -104.1, -103.6, -104.1, -103.8, -105.8, -105.5, -106.4, -98.9,
        -105.3, -105.4, -106.5, -107.2, -109.3, -110.7, -110.4, -110.0, -116.3, -120.0, -120.0,
        -120.0, -120.0, -120.0, -120.0, -120.0, -89.0, -90.2, -102.9, -94.2, -79.4, -69.5, -67.0,
        -65.7, -66.8, -74.3, -86.6, -101.3, -104.4, -21.4, -23.9, -30.8, -22.6, -35.5, -26.9,
        -30.4, -46.7, -79.2, -98.2, -99.2, -104.4, -103.4, -94.8, -86.3, -83.4, -80.7, -78.4,
        -58.1, -53.3, -50.3, -60.3, -44.5, -44.0, -65.4, -54.4, -48.1, -67.2, -91.6, -81.5, -87.7,
        -95.9, -89.2, -89.7, -86.6, -97.8, -97.1, -98.9, -91.2, -92.5, -90.7, -97.4, -73.9, -80.4,
        -77.9, -82.8, -97.2, -56.3, -44.9, -50.8, -48.5, -44.3, -47.8, -47.6, -41.4, -46.4, -45.6,
        -57.8, -47.5, -47.4, -48.9, -64.7, -52.3, -49.2, -70.9, -89.6, -95.7, -67.8, -51.4, -49.8,
        -50.5, -52.2, -52.5, -53.3, -55.3, -57.6, -53.9, -49.8, -47.3, -50.8, -45.3, -51.7, -47.1,
        -49.3, -47.6, -46.8, -58.8, -49.5, -45.7, -46.3, -53.1, -75.2, -77.1, -75.0, -74.1, -76.6,
        -91.6, -81.4, -49.6, -41.4, -53.0, -53.2, -50.8, -57.8, -88.7, -89.1, -52.5, -49.6, -52.6,
        -52.0, -54.5, -54.0, -51.2, -47.8, -47.2, -52.3, -52.7, -60.3, -55.2, -46.0, -46.9, -47.8,
        -51.7, -53.0, -47.6, -50.4, -54.4, -57.1, -50.5, -48.0, -52.5, -49.3, -47.8, -51.6, -52.1,
        -52.3, -53.7, -68.8, -76.7, -54.9, -48.4, -47.5, -48.2, -58.1, -89.8, -78.8, -61.9, -45.0,
        -52.0, -83.1, -101.0, -65.3, -44.7, -50.1, -49.2, -54.6, -58.0, -50.1, -56.8, -51.7, -51.1,
        -51.6, -79.0, -97.7, -101.1, -81.6, -85.9, -83.4, -76.4, -89.7, -88.2, -94.6, -93.4, -66.5,
        -43.7, -41.6, -37.8, -45.3, -41.9, -40.0, -39.3, -47.4, -66.7, -42.0, -44.8, -43.7, -45.4,
        -58.8, -91.5, -57.7, -47.3, -51.0, -61.0, -41.3, -45.1, -46.9, -59.3, -49.1, -59.1, -70.9,
        -61.1, -52.5, -52.6, -53.3, -50.1, -68.6, -84.1, -85.5, -81.6, -82.2, -89.0, -99.1, -96.9,
        -57.3, -43.2, -48.0, -53.0, -46.2, -45.4, -47.4, -48.4, -48.2, -45.2, -44.7, -46.2, -60.6,
        -46.2, -45.5, -51.1, -52.5, -52.4, -54.3, -52.2, -54.3, -57.5, -65.1, -60.5, -52.6, -50.3,
        -53.3, -50.6, -47.3, -50.9, -63.9, -47.6, -50.7, -51.5, -57.9, -91.0, -100.8, -102.1,
        -103.2, -104.1, -80.1, -89.3,
    ];

    /// What Parakeet heard in that take, including a restart cue.
    const RECORDED_WORDS: [(&str, f64, f64); 87] = [
        ("I'm", 0.72, 1.20),
        ("just", 1.20, 1.44),
        ("gonna", 1.44, 1.76),
        ("chat", 1.76, 2.16),
        ("some", 2.16, 2.48),
        ("absolute", 2.48, 2.96),
        ("crap", 2.96, 3.60),
        ("for", 3.60, 3.84),
        ("a", 3.84, 4.00),
        ("few", 4.00, 4.24),
        ("minutes", 4.24, 4.56),
        ("and", 4.56, 4.88),
        ("just", 4.88, 5.12),
        ("put", 5.12, 5.28),
        ("in", 5.28, 5.44),
        ("some", 5.44, 5.68),
        ("pauses.", 5.68, 8.18),
        ("Couple", 11.92, 12.32),
        ("of", 12.32, 12.56),
        ("coughs.", 12.56, 15.04),
        ("So", 15.04, 15.28),
        ("I'm", 15.28, 15.44),
        ("gonna", 15.44, 15.68),
        ("talk", 15.68, 16.00),
        ("about", 16.00, 16.16),
        ("this", 16.16, 16.40),
        ("section.", 16.40, 17.20),
        ("Um", 17.20, 17.68),
        ("so", 17.68, 18.00),
        ("if", 18.00, 18.24),
        ("I'm", 18.24, 18.40),
        ("looking", 18.40, 18.64),
        ("through", 18.64, 18.80),
        ("this", 18.80, 19.04),
        ("Claude", 19.04, 20.08),
        ("program,", 20.08, 20.88),
        ("um", 20.88, 21.20),
        ("so", 21.20, 21.52),
        ("we're", 21.52, 21.76),
        ("walking", 21.76, 22.24),
        ("through", 22.24, 22.56),
        ("how", 22.56, 22.80),
        ("this", 22.80, 23.04),
        ("was", 23.04, 23.20),
        ("all", 23.20, 23.36),
        ("made", 23.36, 23.68),
        ("and", 23.68, 23.84),
        ("stuff,", 23.84, 24.08),
        ("uh", 24.08, 24.32),
        ("you", 24.32, 24.48),
        ("know", 24.48, 24.64),
        ("what,", 24.64, 25.04),
        ("go", 25.04, 25.60),
        ("back", 25.60, 25.76),
        ("and", 25.76, 25.92),
        ("start", 25.92, 26.24),
        ("that", 26.24, 26.48),
        ("again.", 26.48, 27.36),
        ("So", 27.36, 27.68),
        ("if", 27.68, 27.84),
        ("we're", 27.84, 28.00),
        ("looking", 28.00, 28.32),
        ("through", 28.32, 28.48),
        ("this", 28.48, 28.72),
        ("program,", 28.72, 29.12),
        ("this", 29.12, 29.52),
        ("how", 29.52, 29.76),
        ("I've", 29.76, 30.00),
        ("built", 30.00, 30.24),
        ("Claude", 30.24, 30.96),
        ("up,", 30.96, 31.52),
        ("and", 31.52, 31.84),
        ("you", 31.84, 32.00),
        ("can", 32.00, 32.24),
        ("really", 32.24, 32.56),
        ("see", 32.56, 32.80),
        ("how", 32.80, 33.12),
        ("this", 33.12, 33.28),
        ("is", 33.28, 33.44),
        ("a", 33.44, 33.52),
        ("good", 33.52, 33.84),
        ("test", 33.84, 34.16),
        ("and", 34.16, 34.40),
        ("see", 34.40, 34.56),
        ("what's", 34.56, 34.88),
        ("going", 34.88, 35.12),
        ("on.", 35.12, 35.37),
    ];

    #[test]
    fn a_real_take_loses_its_pauses_coughs_and_restart_but_keeps_its_speech() {
        let timeline = single_clip(35.8);
        let mic = vec![Arc::new(RECORDED_LEVELS.to_vec())];
        let words = RECORDED_WORDS
            .iter()
            .map(|(text, start, end)| word(text, *start, *end))
            .collect::<Vec<_>>();

        let levels = measure_levels(&timeline, &mic, &[]).expect("speech and room differ");
        assert!((-80.0..-60.0).contains(&levels.threshold_db), "{levels:?}");

        let cuts = detect_cuts(
            &timeline,
            &mic,
            &[],
            &[],
            &words,
            AutoEditSettings::default(),
        );
        let covered =
            |from: f64, to: f64| cuts.iter().any(|cut| cut.start <= from && cut.end >= to);
        let touched = |from: f64, to: f64| cuts.iter().any(|cut| cut.start < to && cut.end > from);

        assert!(covered(7.0, 11.5), "pause and coughs: {cuts:?}");
        assert!(
            cuts.iter()
                .any(|cut| cut.reason == CutReason::Noise && cut.start <= 10.4 && cut.end >= 11.1),
            "{cuts:?}"
        );
        let retake = cuts
            .iter()
            .find(|cut| cut.reason == CutReason::Retake)
            .expect("the restart is found");
        assert!(approx(retake.start, 17.2), "{retake:?}");
        assert!((27.3..27.7).contains(&retake.end), "{retake:?}");

        for (text, start, end) in [
            ("I'm just gonna", 1.2, 1.76),
            ("Couple of", 12.2, 12.56),
            ("talk about this section", 15.68, 17.0),
            ("if we're looking through this program", 27.68, 29.12),
            ("you can really see", 31.84, 32.8),
        ] {
            assert!(!touched(start, end), "{text} was cut: {cuts:?}");
        }
    }
}
