//! Builds the editor project from a guided session: one recording segment per
//! section, each with the slide video (held on its last frame while the take
//! runs on), the slide audio, and the take's camera and mic.

use std::path::{Path, PathBuf};

use cap_project::{
    AudioMeta, Cursors, MultipleSegment, MultipleSegments, Platform, ProjectConfiguration,
    RecordingMeta, RecordingMetaInner, StudioRecordingMeta, StudioRecordingStatus, VideoMeta,
};

use crate::guided_media;
use crate::guided_recording::{GuidedSession, SegmentPlan, Take};

/// The camera video and mic audio a take recorded. A camera-only recording
/// keeps the camera as its `display`.
struct TakeTracks {
    meta: RecordingMeta,
    camera: VideoMeta,
    mic: Option<AudioMeta>,
}

fn take_tracks(take: &Take) -> Result<TakeTracks, String> {
    let meta = RecordingMeta::load_for_project(&take.project_path)
        .map_err(|e| format!("Could not read take {}: {e}", take.project_path.display()))?;
    let (camera, mic) = match meta.studio_meta() {
        Some(StudioRecordingMeta::SingleSegment { segment }) => (
            segment
                .camera
                .clone()
                .unwrap_or_else(|| segment.display.clone()),
            segment.audio.clone(),
        ),
        Some(StudioRecordingMeta::MultipleSegments { inner }) => {
            let segment = inner
                .segments
                .first()
                .ok_or_else(|| format!("Take {} is empty", take.project_path.display()))?;
            (
                segment
                    .camera
                    .clone()
                    .unwrap_or_else(|| segment.display.clone()),
                segment.mic.clone(),
            )
        }
        None => {
            return Err(format!(
                "Take {} is not a Studio recording",
                take.project_path.display()
            ));
        }
    };
    Ok(TakeTracks { meta, camera, mic })
}

/// The files a take's review plays, each with its start in the take's
/// recording time.
pub struct TakeMedia {
    pub camera: PathBuf,
    pub camera_start: f64,
    pub mic: Option<(PathBuf, f64)>,
}

pub fn take_media(take: &Take) -> Result<TakeMedia, String> {
    let tracks = take_tracks(take)?;
    Ok(TakeMedia {
        camera: tracks.meta.path(&tracks.camera.path),
        camera_start: tracks.camera.start_time.unwrap_or(0.0),
        mic: tracks
            .mic
            .as_ref()
            .map(|mic| (tracks.meta.path(&mic.path), mic.start_time.unwrap_or(0.0))),
    })
}

fn copy_track(source: &Path, dir: &Path, stem: &str) -> Result<String, String> {
    if !source.is_file() {
        return Err(format!("{} is missing", source.display()));
    }
    let extension = source
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let name = format!("{stem}{extension}");
    std::fs::copy(source, dir.join(&name))
        .map_err(|e| format!("Failed to copy {}: {e}", source.display()))?;
    Ok(name)
}

/// Writes the project and returns its path. `progress` gets
/// (sections done, total) as each section is written.
pub fn build_project(
    session: &GuidedSession,
    progress: &dyn Fn(usize, usize),
) -> Result<PathBuf, String> {
    if !session.ready_to_build() {
        return Err("Every section needs a take first".to_string());
    }
    let base = crate::recording::recordings_dir();
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let filename = cap_utils::ensure_unique_filename(
        &crate::recording::project_bundle_filename(&session.title),
        &base,
    )?;
    let project_path = base.join(filename);
    let result = write_project(session, &project_path, progress);
    if result.is_err() {
        std::fs::remove_dir_all(&project_path).ok();
    }
    result.map(|()| project_path)
}

fn write_project(
    session: &GuidedSession,
    project_path: &Path,
    progress: &dyn Fn(usize, usize),
) -> Result<(), String> {
    let total = session.sections.len();
    let mut segments = Vec::with_capacity(total);
    let mut plans = Vec::with_capacity(total);
    progress(0, total);

    for (index, section) in session.sections.iter().enumerate() {
        let take = section
            .chosen_take()
            .expect("ready_to_build checked every section");
        let tracks = take_tracks(take)?;
        let camera_start = tracks.camera.start_time.unwrap_or(0.0);
        let plan = SegmentPlan::new(section, take, camera_start);

        let relative = format!("content/segments/segment-{index}");
        let dir = project_path.join(&relative);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;

        let camera_name = copy_track(&tracks.meta.path(&tracks.camera.path), &dir, "camera")?;
        let mic = match &tracks.mic {
            Some(mic) => {
                let name = copy_track(&tracks.meta.path(&mic.path), &dir, "audio-input")?;
                Some(AudioMeta {
                    path: format!("{relative}/{name}").into(),
                    start_time: mic.start_time,
                    device_id: mic.device_id.clone(),
                    gap_summary: None,
                    digital_silence_ms: None,
                })
            }
            None => None,
        };
        let fps = guided_media::write_segment_display(
            &session.video_path,
            &plan,
            &dir.join("display.mp4"),
        )?;
        let has_audio = guided_media::write_segment_audio(
            &session.video_path,
            &plan,
            &dir.join("system_audio.wav"),
        )?;

        segments.push(MultipleSegment {
            display: VideoMeta {
                path: format!("{relative}/display.mp4").into(),
                fps,
                start_time: Some(camera_start),
                device_id: None,
            },
            camera: Some(VideoMeta {
                path: format!("{relative}/{camera_name}").into(),
                fps: tracks.camera.fps,
                start_time: tracks.camera.start_time,
                device_id: tracks.camera.device_id.clone(),
            }),
            mic,
            system_audio: has_audio.then(|| AudioMeta {
                path: format!("{relative}/system_audio.wav").into(),
                start_time: Some(camera_start),
                device_id: None,
                gap_summary: None,
                digital_silence_ms: None,
            }),
            cursor: None,
            keyboard: None,
            display_notch: None,
        });
        plans.push((plan, section.title.clone()));
        progress(index + 1, total);
    }

    RecordingMeta {
        platform: Some(Platform::default()),
        project_path: project_path.to_path_buf(),
        pretty_name: session.title.clone(),
        sharing: None,
        inner: RecordingMetaInner::Studio(Box::new(StudioRecordingMeta::MultipleSegments {
            inner: MultipleSegments {
                segments,
                cursors: Cursors::default(),
                status: Some(StudioRecordingStatus::Complete),
            },
        })),
        upload: None,
    }
    .save_for_project()
    .map_err(|e| format!("Failed to save the project: {e:?}"))?;

    let first_take = session.sections[0]
        .chosen_take()
        .expect("ready_to_build checked every section");
    let mut config = ProjectConfiguration::load(&first_take.project_path).unwrap_or_default();
    config.timeline = None;
    let totals: Vec<f64> = plans.iter().map(|(plan, _)| plan.total()).collect();
    crate::editor_edits::ensure_timeline(&mut config, &totals);
    if let Some(timeline) = config.timeline.as_mut() {
        for (segment, (plan, title)) in timeline.segments.iter_mut().zip(&plans) {
            segment.start = plan.lead;
            segment.end = plan.total();
            segment.name = Some(title.clone());
        }
    }
    config
        .write(project_path)
        .map_err(|e| format!("Failed to save the project settings: {e}"))?;
    if let Err(error) = crate::recording::enable_studio_sound(project_path) {
        tracing::warn!(%error, "could not turn on Studio Sound for the guided project");
    }

    let thumbnail = crate::library::bundle_thumbnail_path(project_path);
    let first_display = project_path.join("content/segments/segment-0/display.mp4");
    if let Err(error) = crate::library::create_screenshot(&first_display, &thumbnail, None) {
        tracing::warn!(%error, "could not write the guided project's thumbnail");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided_recording::{parse_script, plan_sections, sample_video, scene_cuts};

    /// A stand-in camera-only take: a stretch of the source video as the
    /// camera and its audio as the mic, laid out the way a Studio recording
    /// lays out its first segment.
    fn fake_take(video: &Path, dir: &Path, from: f64, length: f64) -> Take {
        let segment = dir.join("content/segments/segment-0");
        std::fs::create_dir_all(&segment).unwrap();
        let plan = SegmentPlan {
            lead: 0.0,
            length,
            source_start: from,
            source_end: from + length,
            timing: Vec::new(),
        };
        let fps = guided_media::write_segment_display(video, &plan, &segment.join("display.mp4"))
            .unwrap();
        guided_media::write_segment_audio(video, &plan, &segment.join("audio-input.wav")).unwrap();
        RecordingMeta {
            platform: Some(Platform::default()),
            project_path: dir.to_path_buf(),
            pretty_name: "take".into(),
            sharing: None,
            inner: RecordingMetaInner::Studio(Box::new(StudioRecordingMeta::MultipleSegments {
                inner: MultipleSegments {
                    segments: vec![MultipleSegment {
                        display: VideoMeta {
                            path: "content/segments/segment-0/display.mp4".into(),
                            fps,
                            start_time: Some(0.2),
                            device_id: None,
                        },
                        camera: None,
                        mic: Some(AudioMeta {
                            path: "content/segments/segment-0/audio-input.wav".into(),
                            start_time: Some(0.25),
                            device_id: None,
                            gap_summary: None,
                            digital_silence_ms: None,
                        }),
                        system_audio: None,
                        cursor: None,
                        keyboard: None,
                        display_notch: None,
                    }],
                    cursors: Cursors::default(),
                    status: Some(StudioRecordingStatus::Complete),
                },
            })),
            upload: None,
        }
        .save_for_project()
        .unwrap();
        Take {
            project_path: dir.to_path_buf(),
            lead_secs: 3.3,
            duration: length - 3.1,
            timing: Vec::new(),
        }
    }

    #[test]
    #[ignore = "needs CAP_GUIDED_VIDEO, CAP_GUIDED_SCRIPT and CAP_GPUI_APP_DATA_DIR"]
    fn real_build_from_fake_takes() {
        ffmpeg::init().unwrap();
        let video = PathBuf::from(std::env::var("CAP_GUIDED_VIDEO").unwrap());
        let script = PathBuf::from(std::env::var("CAP_GUIDED_SCRIPT").unwrap());
        let (_, script_sections) = parse_script(&std::fs::read_to_string(&script).unwrap());
        let samples = sample_video(&video).unwrap();
        let cuts = scene_cuts(&samples.times, &samples.signatures);
        let mut sections = plan_sections(&script_sections, samples.duration, &cuts);
        sections.truncate(3);
        let mut session = GuidedSession {
            title: "Guided build check".into(),
            video_path: video.clone(),
            script_path: script,
            video_duration: samples.duration,
            sections,
            cuts: cuts.iter().map(|cut| cut.time).collect(),
            beats: None,
        };
        let lengths = [18.0, 22.0, 30.0];
        for (index, length) in lengths.into_iter().enumerate() {
            let dir = session.next_take_path(index);
            let mut take = fake_take(&video, &dir, 60.0 + index as f64 * 30.0, length);
            if index == 1 {
                take.timing = vec![[0.0, 14.0], [2.4, 15.2], [6.0, 15.2], [take.duration, 30.0]];
            }
            session.add_take(index, take);
        }
        session.save().unwrap();
        let path =
            build_project(&session, &|done, total| println!("built {done}/{total}")).unwrap();
        println!("project {}", path.display());

        let meta = RecordingMeta::load_for_project(&path).unwrap();
        let Some(StudioRecordingMeta::MultipleSegments { inner }) = meta.studio_meta() else {
            panic!("not multi-segment");
        };
        assert_eq!(inner.segments.len(), 3);
        let config = ProjectConfiguration::load(&path).unwrap();
        let timeline = config.timeline.unwrap();
        for (segment, length) in timeline.segments.iter().zip(lengths) {
            println!("{:?} {:.2}-{:.2}", segment.name, segment.start, segment.end);
            assert!((segment.start - 3.1).abs() < 1e-6);
            assert!((segment.end - segment.start - (length - 3.1)).abs() < 1e-6);
        }
        assert!(config.audio.improve);

        let second = SegmentPlan::new(
            &session.sections[1],
            session.sections[1].chosen_take().unwrap(),
            0.2,
        );
        let audio = guided_media::segment_audio(&video, &second)
            .unwrap()
            .unwrap();
        let channels = usize::from(audio.channels());
        let loud = |from: f64, to: f64| {
            let range = &audio.samples()
                [(from * 48_000.0) as usize * channels..(to * 48_000.0) as usize * channels];
            range
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
        };
        println!(
            "slide audio peak while paused {:.3}, after resuming {:.3}",
            loud(second.lead + 3.0, second.lead + 5.9),
            loud(second.lead + 6.0, second.lead + 8.0)
        );
        assert!(loud(second.lead + 3.0, second.lead + 5.9) < 0.01);
    }
}
