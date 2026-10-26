//! Media work for guided recording: reading frames from a point in a video,
//! playing a section back in the recorder, and writing each section's slide
//! video and slide audio for the built project.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use cap_audio::AudioData;
use ffmpeg::format::{self as avformat, Pixel};
use gpui::RenderImage;

use crate::guided_recording::SegmentPlan;

/// Decodes a video from a chosen time, converting each frame to one size and
/// pixel format.
pub struct VideoReader {
    input: avformat::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    time_base: f64,
    stream_start: i64,
    from: f64,
    scaler: Option<ffmpeg::software::scaling::Context>,
    format: Pixel,
    width: u32,
    height: u32,
    frame_rate: f64,
    flushed: bool,
    done: bool,
}

impl VideoReader {
    /// `max_width` scales the frames down (never up), keeping the aspect ratio.
    pub fn open(
        path: &Path,
        from: f64,
        format: Pixel,
        max_width: Option<u32>,
    ) -> Result<Self, String> {
        let mut input =
            avformat::input(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
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
        let rate = stream.avg_frame_rate();
        let frame_rate = if rate.denominator() > 0 && rate.numerator() > 0 {
            f64::from(rate)
        } else {
            30.0
        };
        let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        context.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
        });
        let decoder = context.decoder().video().map_err(|e| e.to_string())?;
        if decoder.width() == 0 || decoder.height() == 0 {
            return Err("Invalid video dimensions".to_string());
        }
        let (width, height) = match max_width {
            Some(max) if max < decoder.width() => {
                let height = (f64::from(decoder.height()) * f64::from(max)
                    / f64::from(decoder.width()))
                .round() as u32;
                (max & !1, height.max(2) & !1)
            }
            _ => (decoder.width() & !1, decoder.height() & !1),
        };
        if from > 0.0 {
            let target = ((from - 0.5).max(0.0) * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            input
                .seek(target, ..target)
                .map_err(|e| format!("Failed to seek to {from:.2}s: {e}"))?;
        }
        Ok(Self {
            input,
            decoder,
            stream_index,
            time_base,
            stream_start,
            from,
            scaler: None,
            format,
            width,
            height,
            frame_rate,
            flushed: false,
            done: false,
        })
    }

    pub fn frame_rate(&self) -> f64 {
        self.frame_rate
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The next frame at or after `from`, with its time in the file.
    pub fn next(&mut self) -> Result<Option<(f64, ffmpeg::frame::Video)>, String> {
        let mut decoded = ffmpeg::frame::Video::empty();
        loop {
            if self.done {
                return Ok(None);
            }
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.timestamp().or(decoded.pts()).unwrap_or(0);
                let time = (pts - self.stream_start) as f64 * self.time_base;
                if time + 0.5 / self.frame_rate < self.from {
                    continue;
                }
                return self.convert(&decoded).map(|frame| Some((time, frame)));
            }
            if self.flushed {
                self.done = true;
                continue;
            }
            match self.input.packets().next() {
                Some((stream, packet)) => {
                    if stream.index() == self.stream_index {
                        let _ = self.decoder.send_packet(&packet);
                    }
                }
                None => {
                    let _ = self.decoder.send_eof();
                    self.flushed = true;
                }
            }
        }
    }

    fn convert(&mut self, frame: &ffmpeg::frame::Video) -> Result<ffmpeg::frame::Video, String> {
        if self.scaler.is_none() {
            self.scaler = Some(
                ffmpeg::software::scaling::Context::get(
                    frame.format(),
                    frame.width(),
                    frame.height(),
                    self.format,
                    self.width,
                    self.height,
                    ffmpeg::software::scaling::Flags::BILINEAR,
                )
                .map_err(|e| e.to_string())?,
            );
        }
        let mut converted = ffmpeg::frame::Video::new(self.format, self.width, self.height);
        self.scaler
            .as_mut()
            .expect("just created")
            .run(frame, &mut converted)
            .map_err(|e| e.to_string())?;
        Ok(converted)
    }
}

pub fn bgra_image(frame: &ffmpeg::frame::Video) -> Option<Arc<RenderImage>> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.stride(0);
    let buffer =
        cap_gpui_kernels::frame::copy_rows(frame.data(0), width * 4, stride, height, false)?;
    let image = image::RgbaImage::from_raw(width as u32, height as u32, buffer)?;
    Some(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(image)
    ])))
}

/// One picture in a playback: `file_time` is the file's time at playback 0,
/// and after `until` seconds of playback the picture holds its last frame.
pub struct PlayerVideo {
    pub path: PathBuf,
    pub file_time: f64,
    pub until: f64,
    pub max_width: u32,
}

/// Plays pictures (and optionally sound) from a background thread; the
/// window reads `frames` whenever it repaints.
pub struct Player {
    pub frames: Arc<Mutex<Vec<Option<Arc<RenderImage>>>>>,
    pub started: Instant,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl Player {
    pub fn start(videos: Vec<PlayerVideo>, audio: Option<AudioData>, length: f64) -> Self {
        let frames = Arc::new(Mutex::new(vec![None; videos.len()]));
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let thread_frames = frames.clone();
        let thread_stop = stop.clone();
        let thread_finished = finished.clone();
        std::thread::Builder::new()
            .name("guided-player".into())
            .spawn(move || {
                play(videos, audio, length, started, &thread_frames, &thread_stop);
                thread_finished.store(true, Ordering::Release);
            })
            .ok();
        Self {
            frames,
            started,
            stop,
            finished,
        }
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn frame(&self, index: usize) -> Option<Arc<RenderImage>> {
        self.frames.lock().ok()?.get(index).cloned().flatten()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct Playing {
    reader: VideoReader,
    file_time: f64,
    until: f64,
    pending: Option<(f64, ffmpeg::frame::Video)>,
}

fn play(
    videos: Vec<PlayerVideo>,
    audio: Option<AudioData>,
    length: f64,
    started: Instant,
    frames: &Mutex<Vec<Option<Arc<RenderImage>>>>,
    stop: &AtomicBool,
) {
    let mut readers: Vec<Option<Playing>> = videos
        .iter()
        .map(|video| {
            VideoReader::open(
                &video.path,
                video.file_time.max(0.0),
                Pixel::BGRA,
                Some(video.max_width),
            )
            .map_err(|error| tracing::warn!(%error, "guided player could not open a video"))
            .ok()
            .map(|reader| Playing {
                reader,
                file_time: video.file_time,
                until: video.until,
                pending: None,
            })
        })
        .collect();

    let _sound = audio.and_then(|audio| {
        let (stream, handle) = rodio::OutputStream::try_default().ok()?;
        let sink = rodio::Sink::try_new(&handle).ok()?;
        sink.append(rodio::buffer::SamplesBuffer::new(
            audio.channels(),
            AudioData::SAMPLE_RATE,
            audio.samples().to_vec(),
        ));
        Some((stream, sink))
    });

    while !stop.load(Ordering::Acquire) {
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed > length {
            break;
        }
        for (index, slot) in readers.iter_mut().enumerate() {
            let Some(Playing {
                reader,
                file_time,
                until,
                pending,
            }) = slot
            else {
                continue;
            };
            if elapsed > *until {
                continue;
            }
            let target = *file_time + elapsed;
            let mut shown = None;
            loop {
                let next = match pending.take() {
                    Some(next) => Some(next),
                    None => reader.next().ok().flatten(),
                };
                let Some((time, frame)) = next else {
                    *until = elapsed;
                    break;
                };
                if time > target {
                    *pending = Some((time, frame));
                    break;
                }
                shown = Some(frame);
            }
            if let Some(frame) = shown
                && let Some(image) = bgra_image(&frame)
                && let Ok(mut frames) = frames.lock()
            {
                frames[index] = Some(image);
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Writes the slide video for one built segment: the first frame held for
/// `plan.lead`, the section playing, then its last frame held to the end.
pub fn write_segment_display(
    source: &Path,
    plan: &SegmentPlan,
    output: &Path,
) -> Result<u32, String> {
    let mut reader = VideoReader::open(source, plan.source_start, Pixel::YUV420P, None)?;
    let fps = reader.frame_rate().round().clamp(1.0, 120.0) as u32;
    let (width, height) = reader.size();
    let mut out = avformat::output(output)
        .map_err(|e| format!("Failed to create {}: {e}", output.display()))?;
    let mut encoder = crate::import::open_h264_encoder(
        &mut out,
        width,
        height,
        fps,
        ffmpeg::Rational::new(1, fps as i32 * 1000),
    )?;
    out.write_header()
        .map_err(|e| format!("Failed to write header: {e}"))?;

    let step = 1.0 / f64::from(fps);
    let total = plan.total() + step;
    let mut scaler = None;
    let mut current: Option<ffmpeg::frame::Video> = None;
    let mut pending: Option<(f64, ffmpeg::frame::Video)> = None;
    let mut exhausted = false;
    let mut index = 0u64;
    loop {
        let t = index as f64 * step;
        if t > total {
            break;
        }
        let wanted = plan.source_time(t);
        while !exhausted {
            let next = match pending.take() {
                Some(next) => Some(next),
                None => reader.next()?,
            };
            match next {
                Some((time, _)) if time >= plan.source_end => exhausted = true,
                Some((time, frame)) if time <= wanted + 1e-6 || current.is_none() => {
                    current = Some(frame);
                }
                Some(next) => {
                    pending = Some(next);
                    break;
                }
                None => exhausted = true,
            }
        }
        let Some(frame) = current.as_ref() else {
            return Err("The section has no video frames".to_string());
        };
        let frame = crate::import::convert_for_encode(
            frame,
            &mut scaler,
            encoder.pixel_format,
            width,
            height,
        )?;
        encoder.queue_frame(frame, Duration::from_secs_f64(t), &mut out)?;
        index += 1;
    }
    encoder.flush(&mut out)?;
    out.write_trailer()
        .map_err(|e| format!("Failed to write trailer: {e}"))?;
    Ok(fps)
}

/// Writes the slide audio for one built segment as 16-bit WAV: silence for
/// `plan.lead`, the section's audio, then silence to the end.
pub fn write_segment_audio(
    source: &Path,
    plan: &SegmentPlan,
    output: &Path,
) -> Result<bool, String> {
    let rate = AudioData::SAMPLE_RATE as f64;
    let first = (plan.source_start * rate) as usize;
    let last = (plan.source_end * rate) as usize;
    let audio = match AudioData::from_file_range(source, first, last) {
        Ok(audio) => audio,
        Err(error) if error.contains("No Stream") => return Ok(false),
        Err(error) => return Err(format!("Failed to read the video's audio: {error}")),
    };
    let channels = usize::from(audio.channels().max(1));
    let lead = (plan.lead * rate) as usize * channels;
    let total = (plan.total() * rate) as usize * channels;
    let mut samples = vec![0.0f32; lead];
    let take = (last - first) * channels;
    samples.extend(audio.samples().iter().take(take));
    samples.resize(total.max(samples.len()), 0.0);
    samples.truncate(total);
    write_wav(output, &samples, channels as u16, AudioData::SAMPLE_RATE)?;
    Ok(true)
}

fn write_wav(path: &Path, samples: &[f32], channels: u16, rate: u32) -> Result<(), String> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    bytes.extend_from_slice(&(channels * 2).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// Mixes audio tracks that each start `offset` seconds into the playback.
pub fn mix(tracks: Vec<(AudioData, f64)>, length: f64) -> Option<AudioData> {
    if tracks.is_empty() {
        return None;
    }
    let channels = 2usize;
    let rate = f64::from(AudioData::SAMPLE_RATE);
    let mut mixed = vec![0.0f32; (length * rate) as usize * channels];
    for (audio, offset) in tracks {
        let source_channels = usize::from(audio.channels().max(1));
        let start = (offset * rate).round() as i64;
        for (frame, chunk) in audio.samples().chunks(source_channels).enumerate() {
            let at = start + frame as i64;
            if at < 0 {
                continue;
            }
            let at = at as usize * channels;
            if at + 1 >= mixed.len() {
                break;
            }
            let left = chunk[0];
            let right = *chunk.get(1).unwrap_or(&left);
            mixed[at] += left;
            mixed[at + 1] += right;
        }
    }
    for sample in &mut mixed {
        *sample = sample.clamp(-1.0, 1.0);
    }
    Some(AudioData::from_samples(mixed, channels as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_the_samples() {
        let dir = std::env::temp_dir().join(format!("guided-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");
        write_wav(&path, &[0.0, 0.5, -0.5, 1.0], 2, 48_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(i16::from_le_bytes(bytes[46..48].try_into().unwrap()), 16384);
        assert_eq!(bytes.len(), 52);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mix_offsets_and_widens_mono() {
        let mono = AudioData::from_samples(vec![0.5; 4], 1);
        let mixed = mix(vec![(mono, 2.0 / 48_000.0)], 6.0 / 48_000.0).unwrap();
        assert_eq!(mixed.channels(), 2);
        assert_eq!(
            mixed.samples(),
            &[0.0, 0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]
        );
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;

    #[test]
    #[ignore = "needs CAP_GUIDED_VIDEO"]
    fn real_segment_render() {
        ffmpeg::init().unwrap();
        let video = PathBuf::from(std::env::var("CAP_GUIDED_VIDEO").expect("CAP_GUIDED_VIDEO"));
        let dir = std::env::temp_dir().join("guided-render");
        std::fs::create_dir_all(&dir).unwrap();
        let plan = SegmentPlan {
            lead: 2.0,
            length: 20.0,
            source_start: 14.0,
            source_end: 30.0,
        };
        let started = Instant::now();
        let fps = write_segment_display(&video, &plan, &dir.join("display.mp4")).unwrap();
        println!(
            "display {fps}fps in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        assert!(write_segment_audio(&video, &plan, &dir.join("audio.wav")).unwrap());
        let input = avformat::input(&dir.join("display.mp4")).unwrap();
        let duration = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
        println!("display duration {duration:.2}s");
        assert!((duration - 22.0).abs() < 0.2, "{duration}");
        let audio = AudioData::from_file(dir.join("audio.wav")).unwrap();
        let seconds = audio.sample_count() as f64 / 48_000.0;
        println!("audio {seconds:.2}s");
        assert!((seconds - 22.0).abs() < 0.05, "{seconds}");
        for at in [1.0, 10.0, 21.5] {
            let mut reader =
                VideoReader::open(&dir.join("display.mp4"), at, Pixel::RGBA, Some(480)).unwrap();
            let (_, frame) = reader.next().unwrap().unwrap();
            let (width, height) = reader.size();
            let stride = frame.stride(0);
            let mut rgba = Vec::new();
            for row in 0..height as usize {
                rgba.extend_from_slice(
                    &frame.data(0)[row * stride..row * stride + width as usize * 4],
                );
            }
            image::RgbaImage::from_raw(width, height, rgba)
                .unwrap()
                .save(dir.join(format!("frame-{at}.png")))
                .unwrap();
        }
        println!("frames in {}", dir.display());
    }
}
