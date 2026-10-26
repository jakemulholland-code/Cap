//! DeepFilterNet 3 on a worker thread of its own.
//!
//! tract's model state is neither `Send` nor `Sync`, while Studio Sound's
//! enhancers move between the playback, export and preview threads. So one
//! thread owns the loaded model and a fresh copy of its state per stream, and
//! each enhancer talks to it a frame at a time over channels.

use df::tract::{DfParams, DfTract, RuntimeParams};
use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

enum Request {
    Open {
        id: u64,
        channels: usize,
        frame: usize,
        reply: Sender<Result<Vec<f32>, String>>,
    },
    Process {
        id: u64,
        planar: Vec<f32>,
    },
    Close {
        id: u64,
    },
}

/// One denoising stream: its own DeepFilterNet state on the worker thread.
pub(crate) struct DeepFilterStream {
    id: u64,
    requests: Sender<Request>,
    replies: Receiver<Result<Vec<f32>, String>>,
    /// Samples between a frame going in and its denoised copy coming out.
    pub(crate) delay: usize,
}

impl DeepFilterStream {
    /// `None` (with a warning from the worker) when the model cannot load or
    /// does not work in `frame`-sample hops.
    pub(crate) fn open(channels: usize, frame: usize) -> Option<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let requests = worker()?.clone();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (reply, replies) = mpsc::channel();
        requests
            .send(Request::Open {
                id,
                channels,
                frame,
                reply,
            })
            .ok()?;
        let delay = match replies.recv().ok()? {
            Ok(delay) => delay.first().copied()? as usize,
            Err(error) => {
                tracing::warn!(%error, "DeepFilterNet is unavailable; Studio Sound uses RNNoise");
                return None;
            }
        };
        Some(Self {
            id,
            requests,
            replies,
            delay,
        })
    }

    /// Denoises one hop of planar samples (each channel's frame in turn) in
    /// place, returning the model's local SNR estimate in dB.
    pub(crate) fn process(&mut self, planar: &mut Vec<f32>) -> Result<f32, String> {
        self.requests
            .send(Request::Process {
                id: self.id,
                planar: std::mem::take(planar),
            })
            .map_err(|_| "the DeepFilterNet worker stopped".to_string())?;
        let mut reply = self
            .replies
            .recv()
            .map_err(|_| "the DeepFilterNet worker stopped".to_string())??;
        let lsnr = reply.pop().unwrap_or(f32::NEG_INFINITY);
        *planar = reply;
        Ok(lsnr)
    }
}

impl Drop for DeepFilterStream {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Close { id: self.id });
    }
}

fn worker() -> Option<&'static Sender<Request>> {
    static WORKER: OnceLock<Option<Sender<Request>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (requests, incoming) = mpsc::channel();
            std::thread::Builder::new()
                .name("deep-filter".into())
                .spawn(move || serve(incoming))
                .map_err(|error| tracing::warn!(%error, "Could not start DeepFilterNet"))
                .ok()?;
            Some(requests)
        })
        .as_ref()
}

fn delay_of(model: &DfTract) -> usize {
    model.fft_size - model.hop_size + model.lookahead * model.hop_size
}

fn load(channels: usize, frame: usize) -> Result<DfTract, String> {
    let params = std::panic::catch_unwind(DfParams::default)
        .map_err(|_| "the bundled model could not be read".to_string())?;
    let model = DfTract::new(params, &RuntimeParams::default_with_ch(channels))
        .map_err(|error| error.to_string())?;
    if model.hop_size != frame || delay_of(&model) % frame != 0 {
        return Err(format!(
            "unexpected frame layout: hop {}, delay {}",
            model.hop_size,
            delay_of(&model)
        ));
    }
    Ok(model)
}

fn serve(incoming: Receiver<Request>) {
    let mut prototypes: HashMap<usize, Result<DfTract, String>> = HashMap::new();
    let mut streams: HashMap<u64, (DfTract, Sender<Result<Vec<f32>, String>>)> = HashMap::new();
    while let Ok(request) = incoming.recv() {
        match request {
            Request::Open {
                id,
                channels,
                frame,
                reply,
            } => {
                let prototype = prototypes
                    .entry(channels)
                    .or_insert_with(|| load(channels, frame));
                match prototype {
                    Ok(prototype) => {
                        let delay = delay_of(prototype) as f32;
                        if reply.send(Ok(vec![delay])).is_ok() {
                            streams.insert(id, (prototype.clone(), reply));
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error.clone()));
                    }
                }
            }
            Request::Process { id, mut planar } => {
                let Some((model, reply)) = streams.get_mut(&id) else {
                    continue;
                };
                let result = denoise(model, &mut planar).map(|lsnr| {
                    planar.push(lsnr);
                    planar
                });
                let _ = reply.send(result);
            }
            Request::Close { id } => {
                streams.remove(&id);
            }
        }
    }
}

fn denoise(model: &mut DfTract, planar: &mut [f32]) -> Result<f32, String> {
    let channels = model.ch;
    let hop = model.hop_size;
    let noisy = ndarray::Array2::from_shape_vec((channels, hop), planar.to_vec())
        .map_err(|error| error.to_string())?;
    let enhanced = ndarray::ArrayViewMut2::from_shape((channels, hop), planar)
        .map_err(|error| error.to_string())?;
    model
        .process(noisy.view(), enhanced)
        .map_err(|error| error.to_string())
}
