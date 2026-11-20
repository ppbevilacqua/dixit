//! Windows system audio: process loopback of every app except Dixit (Windows 10 2004+).
//! Unlike the loopback of the default output, it hears Teams on any device (headset, Bluetooth
//! hands-free), like the global Core Audio tap on macOS.
#![cfg(target_os = "windows")]

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// Running capture; dropping it stops the thread.
pub struct ProcessLoopback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessLoopback {
    /// Starts capturing; `on_data` gets interleaved stereo f32 at 48 kHz. Fails (so the caller can
    /// fall back to the default-output loopback) when Windows does not support process loopback.
    pub fn start(mut on_data: impl FnMut(&[f32]) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("process-loopback".into())
            .spawn({
                let stop = stop.clone();
                move || {
                    let _ = wasapi::initialize_mta();
                    let opened = open();
                    let (client, capture, event) = match opened {
                        Ok(parts) => {
                            let _ = ready_tx.send(Ok(()));
                            parts
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let mut bytes = VecDeque::new();
                    let mut samples = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        // Nothing playing means no events: the timeout only lets us check `stop`.
                        if event.wait_for_event(200).is_err() {
                            continue;
                        }
                        if let Err(e) = capture.read_from_device_to_deque(&mut bytes) {
                            log::error!("Process loopback read failed: {e}");
                            break;
                        }
                        let whole = bytes.len() / 4 * 4;
                        let raw: Vec<u8> = bytes.drain(..whole).collect();
                        samples.clear();
                        samples.extend(raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                        if !samples.is_empty() {
                            on_data(&samples);
                        }
                    }
                    let _ = client.stop_stream();
                    log::info!("Process loopback capture stopped");
                }
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(anyhow!("process loopback thread ended before starting")),
        }
    }
}

fn open() -> Result<(AudioClient, wasapi::AudioCaptureClient, wasapi::Handle)> {
    // Exclude our own process tree: everything else on the PC, on any output device.
    let mut client = AudioClient::new_application_loopback_client(std::process::id(), false)
        .map_err(|e| anyhow!("process loopback not available: {e}"))?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, CHANNELS as usize, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| anyhow!("process loopback initialize failed: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| anyhow!("{e}"))?;
    let capture = client.get_audiocaptureclient().map_err(|e| anyhow!("{e}"))?;
    client.start_stream().map_err(|e| anyhow!("{e}"))?;
    Ok((client, capture, event))
}

impl Drop for ProcessLoopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
