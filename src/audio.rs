//! System audio (WASAPI loopback) to a WAV file, timestamped against `Instant` for A/V sync.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

pub struct AudioCapture {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<Result<Option<Instant>, String>>,
}

impl AudioCapture {
    /// Starts capturing the default output device. Streams live on their own thread (cpal streams aren't Send).
    pub fn start(wav: PathBuf) -> Result<Self, String> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || run(wav, stop_rx, ready_tx));
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { stop: stop_tx, thread }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(thread.join().ok().and_then(|r| r.err()).unwrap_or_else(|| "audio thread died".into())),
        }
    }

    /// Stops and finalizes the WAV. Returns when the first recorded sample was played, if any.
    pub fn stop(self) -> Result<Option<Instant>, String> {
        let _ = self.stop.send(());
        self.thread.join().map_err(|_| "audio thread panicked".to_string())?
    }
}

fn run(wav: PathBuf, stop: mpsc::Receiver<()>, ready: mpsc::Sender<Result<(), String>>) -> Result<Option<Instant>, String> {
    let setup = || -> Result<_, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!("unsupported mix format {:?}", supported.sample_format()));
        }
        let config = supported.config();
        let spec = hound::WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(&wav, spec).map_err(|e| e.to_string())?;
        let writer = Arc::new(Mutex::new(Some(writer)));
        let first: Arc<Mutex<Option<Instant>>> = Arc::default();

        // Loopback delivers nothing while the device is silent, which would collapse gaps and break sync.
        // Playing silence on the same device keeps the stream (and its clock) running continuously.
        let keep_alive = device
            .build_output_stream(config.clone(), |data: &mut [f32], _: &cpal::OutputCallbackInfo| data.fill(0.0), |_| {}, None)
            .map_err(|e| e.to_string())?;

        let (w, f) = (writer.clone(), first.clone());
        // cpal opens an output device as an input stream in WASAPI loopback mode.
        let capture = device
            .build_input_stream(
                config,
                move |data: &[f32], info: &cpal::InputCallbackInfo| {
                    let mut first = f.lock().unwrap();
                    if first.is_none() {
                        let ts = info.timestamp();
                        let age = ts.callback.duration_since(ts.capture);
                        *first = Instant::now().checked_sub(age);
                    }
                    if let Some(w) = w.lock().unwrap().as_mut() {
                        for &s in data {
                            let _ = w.write_sample(s);
                        }
                    }
                },
                |e| eprintln!("audio capture error: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        keep_alive.play().map_err(|e| e.to_string())?;
        capture.play().map_err(|e| e.to_string())?;
        Ok((keep_alive, capture, writer, first))
    };

    let (keep_alive, capture, writer, first) = match setup() {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    let _ = stop.recv();
    drop(capture);
    drop(keep_alive);
    if let Some(w) = writer.lock().unwrap().take() {
        w.finalize().map_err(|e| e.to_string())?;
    }
    let first = *first.lock().unwrap();
    Ok(first)
}
