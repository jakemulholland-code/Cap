use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store;

/// The pace Claude Motion scripts are written for.
pub const READ_WORDS_PER_MINUTE: f64 = 140.0;
const SAMPLES_PER_SECOND: f64 = 10.0;
const SIGNATURE_WIDTH: u32 = 64;
const SIGNATURE_HEIGHT: u32 = 36;
/// Scores compare frames this many samples apart, so a fade still reads as one change.
const SCORE_SPAN: usize = 3;
/// Mean luma change (0-255) below which a frame change is animation, not a new slide.
const MIN_CUT_SCORE: f32 = 6.0;
const CUT_SUPPRESS_SECS: f64 = 0.6;
/// How far a written timestamp may be from the slide change it means.
const TIMED_SNAP_SECS: f64 = 1.5;
/// How far a boundary estimated from word counts may move to reach a slide change.
const ESTIMATED_SNAP_SECS: f64 = 6.0;
const MIN_SECTION_SECS: f64 = 1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptSection {
    pub title: String,
    pub start: Option<f64>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneCut {
    pub time: f64,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BoundarySource {
    Start,
    SlideChange,
    Script,
    Estimate,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidedSection {
    pub title: String,
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub source: BoundarySource,
}

impl GuidedSection {
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }

    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }

    pub fn read_secs(&self) -> f64 {
        read_secs(&self.text)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidedSession {
    pub title: String,
    pub video_path: PathBuf,
    pub script_path: PathBuf,
    pub video_duration: f64,
    pub sections: Vec<GuidedSection>,
    #[serde(default)]
    pub cuts: Vec<f64>,
}

impl GuidedSession {
    pub fn directory(&self) -> PathBuf {
        sessions_dir().join(slug(&self.title))
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let dir = self.directory();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
        let path = dir.join("session.json");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Moves the start of section `index` (never the first), keeping every
    /// section at least `MIN_SECTION_SECS` long.
    pub fn move_boundary(&mut self, index: usize, start: f64) {
        if index == 0 || index >= self.sections.len() {
            return;
        }
        let low = self.sections[index - 1].start + MIN_SECTION_SECS;
        let high = self.sections[index].end - MIN_SECTION_SECS;
        if low > high {
            return;
        }
        let start = start.clamp(low, high);
        self.sections[index - 1].end = start;
        self.sections[index].start = start;
        self.sections[index].source = if self.cuts.iter().any(|cut| (cut - start).abs() < 1e-6) {
            BoundarySource::SlideChange
        } else {
            BoundarySource::Manual
        };
    }

    /// The nearest slide change before (`forward == false`) or after the
    /// start of section `index`.
    pub fn neighbouring_cut(&self, index: usize, forward: bool) -> Option<f64> {
        let start = self.sections.get(index)?.start;
        if forward {
            self.cuts.iter().copied().find(|cut| *cut > start + 0.05)
        } else {
            self.cuts
                .iter()
                .rev()
                .copied()
                .find(|cut| *cut < start - 0.05)
        }
    }
}

pub fn sessions_dir() -> PathBuf {
    store::app_data_dir().join("guided")
}

pub fn read_secs(text: &str) -> f64 {
    text.split_whitespace().count() as f64 * 60.0 / READ_WORDS_PER_MINUTE
}

/// Reads a Claude Motion transcript: a `# Title`, then one `## [m:ss] Heading`
/// per section with its spoken text below. Text before the first `##` is
/// notes for the reader, not script.
pub fn parse_script(markdown: &str) -> (String, Vec<ScriptSection>) {
    let mut title = String::new();
    let mut sections: Vec<ScriptSection> = Vec::new();
    let mut body: Vec<&str> = Vec::new();

    let flush = |sections: &mut Vec<ScriptSection>, body: &mut Vec<&str>| {
        if let Some(section) = sections.last_mut() {
            section.text = join_paragraphs(body);
        }
        body.clear();
    };

    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            flush(&mut sections, &mut body);
            let (start, heading) = split_timestamp(heading.trim());
            sections.push(ScriptSection {
                title: heading.to_string(),
                start,
                text: String::new(),
            });
        } else if let Some(heading) = trimmed.strip_prefix("# ") {
            if title.is_empty() && sections.is_empty() {
                title = heading.trim().to_string();
            }
        } else if !sections.is_empty() {
            body.push(trimmed);
        }
    }
    flush(&mut sections, &mut body);

    let title = title
        .trim_end_matches("Video Transcript")
        .trim_end_matches([':', ' ', '-'])
        .to_string();
    (title, sections)
}

fn join_paragraphs(lines: &[&str]) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in lines {
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs.join("\n\n")
}

/// `"1:08 Fewer, better impressions"` -> `(Some(68.0), "Fewer, better impressions")`.
/// Accepts `m:ss`, `h:mm:ss` and fractional seconds, optionally in brackets.
fn split_timestamp(heading: &str) -> (Option<f64>, &str) {
    let (first, rest) = heading
        .split_once(char::is_whitespace)
        .unwrap_or((heading, ""));
    let stamp = first
        .trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'))
        .trim_end_matches(['-', '–']);
    match parse_timestamp(stamp) {
        Some(secs) => (
            Some(secs),
            rest.trim()
                .trim_start_matches(['-', '–', '—', '·', '|'])
                .trim(),
        ),
        None => (None, heading),
    }
}

fn parse_timestamp(stamp: &str) -> Option<f64> {
    let parts: Vec<&str> = stamp.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut secs = 0.0;
    for (index, part) in parts.iter().enumerate() {
        let last = index == parts.len() - 1;
        let valid = !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_digit() || (last && c == '.'));
        if !valid {
            return None;
        }
        let value: f64 = part.parse().ok()?;
        if index > 0 && value >= 60.0 {
            return None;
        }
        secs = secs * 60.0 + value;
    }
    Some(secs)
}

/// Finds slide changes from per-sample frame signatures (`SAMPLES_PER_SECOND`
/// apart): local peaks in the change between frames `SCORE_SPAN` samples apart.
pub fn scene_cuts(times: &[f64], signatures: &[Vec<u8>]) -> Vec<SceneCut> {
    let count = times.len().min(signatures.len());
    if count <= SCORE_SPAN {
        return Vec::new();
    }
    let scores: Vec<(f64, f32)> = (SCORE_SPAN..count)
        .map(|index| {
            let before = &signatures[index - SCORE_SPAN];
            let after = &signatures[index];
            let len = before.len().min(after.len()).max(1);
            let total: u64 = before
                .iter()
                .zip(after)
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum();
            let midpoint = (times[index - SCORE_SPAN] + times[index]) / 2.0;
            (midpoint, total as f32 / len as f32)
        })
        .collect();

    let mut cuts: Vec<SceneCut> = Vec::new();
    for &(time, score) in &scores {
        if score < MIN_CUT_SCORE {
            continue;
        }
        let peak = scores
            .iter()
            .filter(|(other_time, _)| (other_time - time).abs() <= CUT_SUPPRESS_SECS)
            .all(|&(other_time, other)| other < score || (other == score && other_time >= time));
        if !peak
            || cuts
                .last()
                .is_some_and(|last| time - last.time <= CUT_SUPPRESS_SECS)
        {
            continue;
        }
        cuts.push(SceneCut { time, score });
    }
    cuts
}

/// Turns the script into sections over the video. Written timestamps are
/// kept and missing ones estimated from word counts between them; then each
/// boundary moves to the strongest slide change near it.
pub fn plan_sections(
    script: &[ScriptSection],
    video_duration: f64,
    cuts: &[SceneCut],
) -> Vec<GuidedSection> {
    if script.is_empty() || video_duration <= 0.0 {
        return Vec::new();
    }
    let estimated = estimate_starts(script, video_duration);

    let mut starts: Vec<(f64, BoundarySource)> = Vec::with_capacity(script.len());
    for (index, section) in script.iter().enumerate() {
        if index == 0 {
            starts.push((0.0, BoundarySource::Start));
            continue;
        }
        let previous = starts[index - 1].0;
        let guess = estimated[index];
        let window = if section.start.is_some() {
            TIMED_SNAP_SECS
        } else {
            ESTIMATED_SNAP_SECS
        };
        let snapped = cuts
            .iter()
            .filter(|cut| {
                (cut.time - guess).abs() <= window && cut.time >= previous + MIN_SECTION_SECS
            })
            .max_by(|a, b| {
                let rank = |cut: &SceneCut| cut.score as f64 / (1.0 + (cut.time - guess).abs());
                rank(a).total_cmp(&rank(b))
            });
        let (start, source) = match snapped {
            Some(cut) => (cut.time, BoundarySource::SlideChange),
            None if section.start.is_some() => (guess, BoundarySource::Script),
            None => (guess, BoundarySource::Estimate),
        };
        starts.push((start.max(previous + MIN_SECTION_SECS), source));
    }

    let last = starts.len() - 1;
    let mut sections = Vec::with_capacity(script.len());
    for (index, section) in script.iter().enumerate() {
        let (start, source) = starts[index];
        let end = if index == last {
            video_duration
        } else {
            starts[index + 1].0
        };
        sections.push(GuidedSection {
            title: section.title.clone(),
            text: section.text.clone(),
            start: start.min(video_duration),
            end: end.min(video_duration),
            source,
        });
    }
    sections
}

/// Section starts with the written timestamps as anchors and the gaps between
/// them shared out by read time.
fn estimate_starts(script: &[ScriptSection], video_duration: f64) -> Vec<f64> {
    let mut anchors: Vec<(usize, f64)> = vec![(0, 0.0)];
    for (index, section) in script.iter().enumerate().skip(1) {
        if let Some(start) = section.start
            && start > anchors.last().map_or(0.0, |anchor| anchor.1)
            && start < video_duration
        {
            anchors.push((index, start));
        }
    }
    anchors.push((script.len(), video_duration));

    let weights: Vec<f64> = script
        .iter()
        .map(|section| read_secs(&section.text).max(1.0))
        .collect();
    let mut starts = vec![0.0; script.len()];
    for pair in anchors.windows(2) {
        let (from, from_time) = pair[0];
        let (to, to_time) = pair[1];
        let total: f64 = weights[from..to].iter().sum();
        let mut time = from_time;
        for index in from..to {
            starts[index] = time;
            time += (to_time - from_time) * weights[index] / total;
        }
    }
    starts
}

pub fn slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "guided-recording".to_string()
    } else {
        slug
    }
}

pub struct VideoSamples {
    pub duration: f64,
    pub times: Vec<f64>,
    pub signatures: Vec<Vec<u8>>,
}

/// Decodes the video once, keeping a small greyscale signature every
/// `1 / SAMPLES_PER_SECOND` seconds.
pub fn sample_video(path: &Path) -> Result<VideoSamples, String> {
    let mut input =
        ffmpeg::format::input(path).map_err(|e| format!("Failed to open video: {e}"))?;
    let duration = match input.duration() {
        duration if duration > 0 => duration as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE),
        _ => 0.0,
    };
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or("No video stream found")?;
    let stream_index = stream.index();
    let time_base = f64::from(stream.time_base());
    let stream_start = match stream.start_time() {
        ffmpeg::ffi::AV_NOPTS_VALUE => 0,
        timestamp => timestamp,
    };

    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| e.to_string())?;
    context.set_threading(ffmpeg::threading::Config {
        kind: ffmpeg::threading::Type::Frame,
        count: 0,
    });
    let mut decoder = context.decoder().video().map_err(|e| e.to_string())?;
    if decoder.width() == 0 || decoder.height() == 0 {
        return Err("Invalid video dimensions".to_string());
    }
    let mut scaler = ffmpeg::software::scaling::context::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::GRAY8,
        SIGNATURE_WIDTH,
        SIGNATURE_HEIGHT,
        ffmpeg::software::scaling::flag::Flags::AREA,
    )
    .map_err(|e| e.to_string())?;

    let mut times = Vec::new();
    let mut signatures = Vec::new();
    let mut next_sample = 0.0;
    let mut decoded = ffmpeg::frame::Video::empty();
    let mut grey = ffmpeg::frame::Video::empty();

    let mut drain = |decoder: &mut ffmpeg::decoder::Video,
                     times: &mut Vec<f64>,
                     signatures: &mut Vec<Vec<u8>>|
     -> Result<(), String> {
        while decoder.receive_frame(&mut decoded).is_ok() {
            let Some(pts) = decoded.timestamp().or(decoded.pts()) else {
                continue;
            };
            let time = (pts - stream_start) as f64 * time_base;
            if time + 1e-6 < next_sample {
                continue;
            }
            next_sample = time + 1.0 / SAMPLES_PER_SECOND;
            scaler.run(&decoded, &mut grey).map_err(|e| e.to_string())?;
            let stride = grey.stride(0);
            let data = grey.data(0);
            let mut signature = Vec::with_capacity((SIGNATURE_WIDTH * SIGNATURE_HEIGHT) as usize);
            for row in 0..SIGNATURE_HEIGHT as usize {
                let start = row * stride;
                signature.extend_from_slice(&data[start..start + SIGNATURE_WIDTH as usize]);
            }
            times.push(time);
            signatures.push(signature);
        }
        Ok(())
    };

    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        drain(&mut decoder, &mut times, &mut signatures)?;
    }
    let _ = decoder.send_eof();
    drain(&mut decoder, &mut times, &mut signatures)?;

    let duration = if duration > 0.0 {
        duration
    } else {
        times.last().copied().unwrap_or(0.0)
    };
    Ok(VideoSamples {
        duration,
        times,
        signatures,
    })
}

/// Builds a session from a Claude Motion video and its transcript.
pub fn prepare_session(video_path: &Path, script_path: &Path) -> Result<GuidedSession, String> {
    let markdown = std::fs::read_to_string(script_path)
        .map_err(|e| format!("Failed to read {}: {e}", script_path.display()))?;
    let (title, script) = parse_script(&markdown);
    if script.is_empty() {
        return Err("The transcript has no `## ` sections".to_string());
    }
    let samples = sample_video(video_path)?;
    let duration = samples.duration;
    let cuts = scene_cuts(&samples.times, &samples.signatures);
    let sections = plan_sections(&script, duration, &cuts);
    let title = if title.is_empty() {
        video_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        title
    };
    Ok(GuidedSession {
        title,
        video_path: video_path.to_path_buf(),
        script_path: script_path.to_path_buf(),
        video_duration: duration,
        sections,
        cuts: cuts.iter().map(|cut| cut.time).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAMILTON: &str = "# Hamilton SEO Review, September 2026: Video Transcript

Oct 26, 2026 · @Jake

Read each section as its timestamp comes up.

## 0:00 Title

Hi, I'm Jake from Paramount Digital.

## 0:14 What's inside

Here's what we'll cover.
September at a glance.

Why impressions are falling.

## 1:08 Fewer, better impressions

Impressions fell 33% year on year.
";

    fn section(title: &str, start: Option<f64>, words: usize) -> ScriptSection {
        ScriptSection {
            title: title.to_string(),
            start,
            text: vec!["word"; words].join(" "),
        }
    }

    fn cut(time: f64, score: f32) -> SceneCut {
        SceneCut { time, score }
    }

    #[test]
    fn parses_claude_motion_transcript() {
        let (title, sections) = parse_script(HAMILTON);
        assert_eq!(title, "Hamilton SEO Review, September 2026");
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].title, "Title");
        assert_eq!(sections[0].start, Some(0.0));
        assert_eq!(sections[0].text, "Hi, I'm Jake from Paramount Digital.");
        assert_eq!(sections[1].start, Some(14.0));
        assert_eq!(
            sections[1].text,
            "Here's what we'll cover. September at a glance.\n\nWhy impressions are falling."
        );
        assert_eq!(sections[2].title, "Fewer, better impressions");
        assert_eq!(sections[2].start, Some(68.0));
    }

    #[test]
    fn parses_timestamp_forms() {
        assert_eq!(split_timestamp("1:02:03 Long"), (Some(3723.0), "Long"));
        assert_eq!(split_timestamp("[0:14.5] - Half"), (Some(14.5), "Half"));
        assert_eq!(split_timestamp("Next steps"), (None, "Next steps"));
        assert_eq!(split_timestamp("10:75 Bad"), (None, "10:75 Bad"));
        assert_eq!(split_timestamp("2026: the year"), (None, "2026: the year"));
    }

    #[test]
    fn timed_boundaries_snap_to_nearby_slide_changes() {
        let script = [
            section("A", Some(0.0), 30),
            section("B", Some(14.0), 30),
            section("C", Some(30.0), 30),
        ];
        let cuts = [cut(5.0, 40.0), cut(14.6, 30.0), cut(33.0, 50.0)];
        let sections = plan_sections(&script, 50.0, &cuts);
        assert_eq!(sections[0].start, 0.0);
        assert_eq!(sections[0].end, 14.6);
        assert_eq!(sections[1].source, BoundarySource::SlideChange);
        assert_eq!(sections[2].start, 30.0);
        assert_eq!(sections[2].source, BoundarySource::Script);
        assert_eq!(sections[2].end, 50.0);
    }

    #[test]
    fn snap_prefers_the_stronger_close_change() {
        let script = [section("A", Some(0.0), 10), section("B", Some(10.0), 10)];
        let cuts = [cut(9.6, 8.0), cut(10.3, 45.0)];
        let sections = plan_sections(&script, 20.0, &cuts);
        assert_eq!(sections[1].start, 10.3);
    }

    #[test]
    fn untimed_script_is_shared_out_by_read_time() {
        let script = [
            section("A", None, 70),
            section("B", None, 140),
            section("C", None, 70),
        ];
        let sections = plan_sections(&script, 120.0, &[]);
        assert_eq!(sections[1].start, 30.0);
        assert_eq!(sections[2].start, 90.0);
        assert_eq!(sections[2].source, BoundarySource::Estimate);

        let snapped = plan_sections(&script, 120.0, &[cut(33.0, 20.0), cut(88.0, 20.0)]);
        assert_eq!(snapped[1].start, 33.0);
        assert_eq!(snapped[2].start, 88.0);
    }

    #[test]
    fn missing_timestamps_fill_between_anchors() {
        let script = [
            section("A", Some(0.0), 10),
            section("B", None, 10),
            section("C", Some(40.0), 10),
        ];
        let sections = plan_sections(&script, 60.0, &[]);
        assert_eq!(sections[1].start, 20.0);
        assert_eq!(sections[2].start, 40.0);
    }

    #[test]
    fn scene_cuts_find_the_slide_change() {
        let mut times = Vec::new();
        let mut signatures = Vec::new();
        for index in 0..60 {
            times.push(index as f64 / SAMPLES_PER_SECOND);
            let value = if index < 30 { 20 } else { 200 };
            let drift = (index % 4) as u8;
            signatures.push(vec![value + drift; 16]);
        }
        let cuts = scene_cuts(&times, &signatures);
        assert_eq!(cuts.len(), 1);
        assert!((cuts[0].time - 3.0).abs() <= 0.2, "{cuts:?}");
    }

    #[test]
    fn moving_a_boundary_keeps_sections_in_order() {
        let mut session = GuidedSession {
            title: "t".into(),
            video_path: PathBuf::new(),
            script_path: PathBuf::new(),
            video_duration: 30.0,
            sections: plan_sections(
                &[
                    section("A", Some(0.0), 5),
                    section("B", Some(10.0), 5),
                    section("C", Some(20.0), 5),
                ],
                30.0,
                &[],
            ),
            cuts: vec![12.0, 25.0],
        };
        session.move_boundary(1, 0.5);
        assert_eq!(session.sections[1].start, 1.0);
        assert_eq!(session.sections[0].end, 1.0);
        assert_eq!(session.neighbouring_cut(1, true), Some(12.0));
        session.move_boundary(1, 12.0);
        assert_eq!(session.sections[1].source, BoundarySource::SlideChange);
        session.move_boundary(2, 40.0);
        assert_eq!(session.sections[2].start, 29.0);
        assert_eq!(session.sections[2].source, BoundarySource::Manual);
    }

    #[test]
    fn slugs_titles() {
        assert_eq!(
            slug("Hamilton SEO Review, September 2026"),
            "hamilton-seo-review-september-2026"
        );
        assert_eq!(slug("!!"), "guided-recording");
    }

    #[test]
    #[ignore = "needs CAP_GUIDED_VIDEO and CAP_GUIDED_SCRIPT"]
    fn real_video_split() {
        let video = std::env::var("CAP_GUIDED_VIDEO").expect("CAP_GUIDED_VIDEO");
        let script = std::env::var("CAP_GUIDED_SCRIPT").expect("CAP_GUIDED_SCRIPT");
        ffmpeg::init().unwrap();
        let VideoSamples {
            duration,
            times,
            signatures,
        } = sample_video(Path::new(&video)).unwrap();
        let cuts = scene_cuts(&times, &signatures);
        println!(
            "duration {duration:.2}s, {} samples, {} cuts",
            times.len(),
            cuts.len()
        );
        for cut in &cuts {
            println!("  cut {:7.2} score {:5.1}", cut.time, cut.score);
        }
        if std::env::var("CAP_GPUI_APP_DATA_DIR").is_ok() {
            let session = prepare_session(Path::new(&video), Path::new(&script)).unwrap();
            println!("saved {}", session.save().unwrap().display());
        }
        let (_, sections) = parse_script(&std::fs::read_to_string(script).unwrap());
        for section in plan_sections(&sections, duration, &cuts) {
            println!(
                "{:7.2}-{:7.2} {:?} {} (read {:.1}s of {:.1}s)",
                section.start,
                section.end,
                section.source,
                section.title,
                section.read_secs(),
                section.duration()
            );
        }
    }
}
