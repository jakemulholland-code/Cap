//! Follows the reader through the teleprompter script by listening to the
//! microphone: Parakeet hears the last few seconds and the words are matched
//! against the script, and between matches the place moves on only while the
//! reader is speaking.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cap_recording::feeds::microphone::MicrophoneSamples;

use crate::guided_prompt;

const HEARING_RATE: f64 = 16_000.0;
const HEARD_SECS: f64 = 3.0;
const MIN_HEARD_SECS: f64 = 0.6;
const LISTEN_EVERY: Duration = Duration::from_millis(500);
/// Speech is this far over the room's level.
const SPEECH_OVER_ROOM_DB: f64 = 12.0;
const QUIETEST_SPEECH_DB: f64 = -65.0;
/// How fast the room level estimate creeps up, in dB a second.
const ROOM_RISE_DB: f64 = 3.0;
const SPEECH_HANGOVER_SECS: f64 = 0.35;
/// How far the place may run ahead of the last heard match.
const RUN_AHEAD_WORDS: f64 = 3.0;

pub struct Listener {
    shared: Arc<Shared>,
    hearing: bool,
}

struct Shared {
    stop: AtomicBool,
    audio: Mutex<Audio>,
    place: Mutex<Place>,
}

#[derive(Default)]
struct Audio {
    samples: VecDeque<f32>,
    new_speech: bool,
}

struct Place {
    word: usize,
    /// Seconds of speech since `word` was last matched.
    spoken: f64,
    speaking: bool,
    words: usize,
    wpm: f64,
    /// Matches move the place; without them it runs at `wpm` while speaking.
    hearing: bool,
}

impl Listener {
    /// Listens on `samples` for the reader saying `script` (match keys).
    pub fn start(
        samples: flume::Receiver<MicrophoneSamples>,
        script: Vec<String>,
        wpm: f64,
        model: Option<String>,
    ) -> Self {
        let hearing = model.is_some();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            audio: Mutex::new(Audio::default()),
            place: Mutex::new(Place {
                word: 0,
                spoken: 0.0,
                speaking: false,
                words: script.len(),
                wpm,
                hearing,
            }),
        });
        let audio_shared = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("guided-listen".into())
            .spawn(move || take_audio(&audio_shared, &samples))
        {
            tracing::warn!(%error, "could not start listening to the microphone");
        }
        if let Some(model) = model {
            let hear_shared = shared.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("guided-hear".into())
                .spawn(move || hear(&hear_shared, &model, &script))
            {
                tracing::warn!(%error, "could not start following the script");
            }
        }
        Self { shared, hearing }
    }

    /// The word the reader is on.
    pub fn word(&self) -> usize {
        self.shared.place().current()
    }

    pub fn speaking(&self) -> bool {
        self.shared.place().speaking
    }

    /// Following by the words heard, not only by speaking time.
    pub fn hearing(&self) -> bool {
        self.shared.place().hearing
    }

    pub fn set_wpm(&self, wpm: f64) {
        self.shared.place().wpm = wpm;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if self.hearing {
            crate::transcription::release_live_model();
        }
    }
}

impl Shared {
    fn place(&self) -> std::sync::MutexGuard<'_, Place> {
        self.place.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn audio(&self) -> std::sync::MutexGuard<'_, Audio> {
        self.audio.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

impl Place {
    fn current(&self) -> usize {
        let mut ahead = self.spoken * self.wpm / 60.0;
        if self.hearing {
            ahead = ahead.min(RUN_AHEAD_WORDS);
        }
        (self.word + ahead as usize).min(self.words.saturating_sub(1))
    }
}

fn take_audio(shared: &Shared, samples: &flume::Receiver<MicrophoneSamples>) {
    let mut room: Option<f64> = None;
    let mut since_speech = f64::INFINITY;
    let mut shrink = Shrink::default();
    while !shared.stopped() {
        let packet = match samples.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(flume::RecvTimeoutError::Timeout) => continue,
            Err(flume::RecvTimeoutError::Disconnected) => break,
        };
        let mono = mono_samples(&packet);
        if mono.is_empty() || packet.sample_rate == 0 {
            continue;
        }
        let seconds = mono.len() as f64 / f64::from(packet.sample_rate);
        let level = rms_db(&mono);
        let floor = room.map_or(level, |room| level.min(room + ROOM_RISE_DB * seconds));
        room = Some(floor);
        if level > (floor + SPEECH_OVER_ROOM_DB).max(QUIETEST_SPEECH_DB) {
            since_speech = 0.0;
        } else {
            since_speech += seconds;
        }
        let speaking = since_speech <= SPEECH_HANGOVER_SECS;

        {
            let mut audio = shared.audio();
            shrink.push(&mono, f64::from(packet.sample_rate), &mut audio.samples);
            let keep = (HEARD_SECS * HEARING_RATE) as usize;
            let extra = audio.samples.len().saturating_sub(keep);
            audio.samples.drain(..extra);
            audio.new_speech |= speaking;
        }
        let mut place = shared.place();
        place.speaking = speaking;
        if speaking {
            place.spoken += seconds;
        }
    }
}

fn hear(shared: &Shared, model: &str, script: &[String]) {
    let mut last = std::time::Instant::now();
    while !shared.stopped() {
        std::thread::sleep(LISTEN_EVERY.saturating_sub(last.elapsed()));
        last = std::time::Instant::now();
        let heard: Vec<f32> = {
            let mut audio = shared.audio();
            if !audio.new_speech || (audio.samples.len() as f64) < MIN_HEARD_SECS * HEARING_RATE {
                continue;
            }
            audio.new_speech = false;
            audio.samples.iter().copied().collect()
        };
        let words = match crate::transcription::hear_words(model, &heard) {
            Ok(words) => words,
            Err(error) => {
                tracing::warn!(%error, "could not hear the reader; following speaking time instead");
                shared.place().hearing = false;
                return;
            }
        };
        let keys = guided_prompt::heard_keys(&words);
        let mut place = shared.place();
        if let Some(word) = guided_prompt::follow_heard(script, place.word, &keys) {
            place.word = word.min(place.words.saturating_sub(1));
            place.spoken = 0.0;
        }
    }
}

/// Averages input samples down to `HEARING_RATE`.
#[derive(Default)]
struct Shrink {
    sum: f64,
    count: f64,
}

impl Shrink {
    fn push(&mut self, input: &[f32], rate: f64, output: &mut VecDeque<f32>) {
        let step = (rate / HEARING_RATE).max(1.0);
        for sample in input {
            self.sum += f64::from(*sample);
            self.count += 1.0;
            if self.count >= step {
                output.push_back((self.sum / self.count) as f32);
                self.count -= step;
                self.sum = 0.0;
            }
        }
    }
}

fn rms_db(samples: &[f32]) -> f64 {
    let power = samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len().max(1) as f64;
    10.0 * power.max(1e-12).log10()
}

fn mono_samples(packet: &MicrophoneSamples) -> Vec<f32> {
    use cpal::{Sample, SampleFormat};

    let size = packet.format.sample_size();
    let channels = usize::from(packet.channels.max(1));
    if size == 0 {
        return Vec::new();
    }
    let values: Vec<f32> = packet
        .data
        .chunks_exact(size)
        .map(|bytes| match packet.format {
            SampleFormat::I8 => f32::from(i8::from_ne_bytes([bytes[0]])) / f32::from(i8::MAX),
            SampleFormat::U8 => u8::from_ne_bytes([bytes[0]]).to_sample::<f32>(),
            SampleFormat::I16 => {
                f32::from(i16::from_ne_bytes([bytes[0], bytes[1]])) / f32::from(i16::MAX)
            }
            SampleFormat::U16 => u16::from_ne_bytes([bytes[0], bytes[1]]).to_sample::<f32>(),
            SampleFormat::I32 => {
                i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).to_sample::<f32>()
            }
            SampleFormat::U32 => {
                u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).to_sample::<f32>()
            }
            SampleFormat::F32 => f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            SampleFormat::F64 => f64::from_ne_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]) as f32,
            _ => 0.0,
        })
        .collect();
    values
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinking_averages_down_to_the_hearing_rate() {
        let mut shrink = Shrink::default();
        let mut output = VecDeque::new();
        shrink.push(&[0.3; 48], 48_000.0, &mut output);
        assert_eq!(output.len(), 16);
        assert!(output.iter().all(|sample| (sample - 0.3).abs() < 1e-6));
    }

    /// Replays a recorded mic track as if live and prints where the prompt
    /// would be. `CAP_GUIDED_MIC` is the audio, `CAP_GUIDED_TEXT` a file with
    /// the section's script.
    #[test]
    #[ignore = "needs CAP_GUIDED_MIC and CAP_GUIDED_TEXT"]
    fn real_follow_replay() {
        let audio =
            cap_audio::AudioData::from_file(std::env::var("CAP_GUIDED_MIC").unwrap()).unwrap();
        let text = std::fs::read_to_string(std::env::var("CAP_GUIDED_TEXT").unwrap()).unwrap();
        let words = guided_prompt::prompt_words(&text);
        let script: Vec<String> = words.iter().map(|word| word.key.clone()).collect();
        let model = crate::transcription::live_model_dir().expect("a Parakeet model");
        let channels = usize::from(audio.channels());
        let mono: Vec<f32> = audio
            .samples()
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect();
        let mut heard = VecDeque::new();
        Shrink::default().push(&mono, 48_000.0, &mut heard);
        let heard: Vec<f32> = heard.into();
        let step = (HEARING_RATE * LISTEN_EVERY.as_secs_f64()) as usize;
        let window = (HEARD_SECS * HEARING_RATE) as usize;
        let mut word = 0;
        let mut slowest = Duration::ZERO;
        let mut end = step;
        while end <= heard.len() {
            let started = std::time::Instant::now();
            let said =
                crate::transcription::hear_words(&model, &heard[end.saturating_sub(window)..end])
                    .unwrap();
            let took = started.elapsed();
            if end > step * 2 {
                slowest = slowest.max(took);
            }
            let keys = guided_prompt::heard_keys(&said);
            if let Some(next) = guided_prompt::follow_heard(&script, word, &keys) {
                word = next.min(script.len() - 1);
            }
            println!(
                "{:5.1}s {:5.1}dB {:4}ms -> word {word:3} {:?}  heard: {}",
                end as f64 / HEARING_RATE,
                rms_db(&heard[end.saturating_sub(step)..end]),
                took.as_millis(),
                words[word].text,
                said.join(" ")
            );
            end += step;
        }
        println!(
            "slowest hearing {}ms, reached word {word} of {}",
            slowest.as_millis(),
            script.len()
        );
    }

    #[test]
    fn the_place_runs_ahead_only_a_little_while_hearing() {
        let mut place = Place {
            word: 10,
            spoken: 5.0,
            speaking: true,
            words: 100,
            wpm: 120.0,
            hearing: true,
        };
        assert_eq!(place.current(), 13);
        place.hearing = false;
        assert_eq!(place.current(), 20);
        place.spoken = 100.0;
        assert_eq!(place.current(), 99);
    }
}
