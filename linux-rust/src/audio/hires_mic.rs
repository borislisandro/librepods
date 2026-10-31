//! Hi-res microphone lifecycle: a persistent virtual input device plus a monitor
//! that runs the AACP 0x58 capture only while an app is recording from it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TokioHandle;

use crate::audio::eld::{ELD_CHANNELS, ELD_FRAME_SAMPLES, ELD_SAMPLE_RATE, EldDecoder};
use crate::audio::output::{self, Output, VirtualMic};
use crate::bluetooth::aacp::AACPManager;
use crate::bluetooth::aacp_audio;

/// Delay after sending 0x58 START/STOP before resetting the A2DP transport
const A2DP_RESET_DELAY: Duration = Duration::from_millis(800);
/// How often the monitor polls the virtual source for activity
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// If no audio SDUs arrive for this long while a capture is active, the stream
/// is considered stalled and the capture is torn down and restarted.
const STALL_TIMEOUT: Duration = Duration::from_millis(2000);
/// Upper bound for the delay between stall restarts that keep failing.
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(30);
const LEVEL_RELEASE: f32 = 0.85;

#[derive(Debug, PartialEq, Eq)]
enum CaptureAction {
    /// An app started recording while a bud is in an ear: send 0x58 START.
    Start,
    /// Capturing and the device delivers audio: nothing to do.
    Keep,
    /// Capturing but no audio SDUs for STALL_TIMEOUT while worn: STOP + START.
    Restart,
    /// Capture must end. `reset_a2dp`: cycle A2DP afterwards (only while worn;
    /// when the buds came out the next START resets anyway).
    Stop { reset_a2dp: bool },
    /// Not capturing and nothing to start.
    Idle,
}

/// Decide what the hi-res monitor does this poll. The AirPods only deliver mic
/// audio while a bud is in an ear, so capture is tied to `worn`; without that
/// the stall watchdog restarted (and cycled A2DP) every ~2.4 s with the buds out.
fn capture_action(
    enabled: bool,
    recording: bool,
    worn: bool,
    capturing: bool,
    stalled: bool,
) -> CaptureAction {
    let want = enabled && recording && worn;
    match (want, capturing) {
        (true, false) => CaptureAction::Start,
        (true, true) if stalled => CaptureAction::Restart,
        (true, true) => CaptureAction::Keep,
        (false, true) => CaptureAction::Stop { reset_a2dp: worn },
        (false, false) => CaptureAction::Idle,
    }
}

/// Process-wide "an app is recording from the hi-res mic right now" (there is
/// one AirPods connection at a time). Used to avoid disruptive actions such as a
/// WirePlumber restart in the middle of a call.
static ANY_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn any_capture_active() -> bool {
    ANY_CAPTURE_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Clone, Default)]
pub struct MicStatus {
    level: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
    app: Arc<Mutex<Option<String>>>,
    last_sdu: Arc<Mutex<Option<Instant>>>,
}

impl MicStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn app(&self) -> Option<String> {
        self.app.lock().unwrap().clone()
    }

    fn set_capture(&self, app: Option<String>) {
        self.active.store(app.is_some(), Ordering::Relaxed);
        ANY_CAPTURE_ACTIVE.store(app.is_some(), Ordering::Relaxed);
        *self.app.lock().unwrap() = app;
    }

    fn reset(&self) {
        self.active.store(false, Ordering::Relaxed);
        ANY_CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
        *self.app.lock().unwrap() = None;
        *self.last_sdu.lock().unwrap() = None;
        self.set_level(0.0);
    }

    /// Record that an audio SDU just arrived from the device.
    pub fn mark_sdu(&self) {
        *self.last_sdu.lock().unwrap() = Some(Instant::now());
    }

    /// Time since the last audio SDU, or None if none has arrived yet.
    fn since_last_sdu(&self) -> Option<Duration> {
        self.last_sdu.lock().unwrap().map(|t| t.elapsed())
    }
}

pub struct HiResMic {
    stop: Arc<Notify>,
    monitor: Option<TokioHandle<()>>,
}

impl HiResMic {
    // Create the persistent virtual input and spawn the activity monitor. The
    // monitor owns the virtual device and unloads it when it exits.
    pub async fn start(aacp: &AACPManager, addr: String, status: MicStatus) -> Option<HiResMic> {
        let vmic = VirtualMic::open(ELD_SAMPLE_RATE, ELD_CHANNELS as u8)?;
        let stop = Arc::new(Notify::new());
        let wake = aacp.hires_wake();
        // Spawn on the backend runtime, not the caller's (the UI toggle uses a
        // throwaway runtime that would abort this task immediately).
        let monitor = aacp.runtime().spawn(monitor_loop(
            aacp.clone(),
            addr,
            status,
            stop.clone(),
            wake,
            vmic,
        ));
        Some(HiResMic {
            stop,
            monitor: Some(monitor),
        })
    }

    // Stop the monitor (tearing down any active capture) and unload the device.
    pub async fn stop(mut self) {
        self.stop.notify_one();
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.monitor.as_ref().is_some_and(|h| !h.is_finished())
    }
}

// A live capture session: the playback stream feeding the sink plus its decode
// thread, started when an app opens the mic.
struct Capture {
    decode_thread: Option<JoinHandle<()>>,
}

async fn monitor_loop(
    aacp: AACPManager,
    addr: String,
    status: MicStatus,
    stop: Arc<Notify>,
    wake: Arc<Notify>,
    _vmic: VirtualMic,
) {
    let mut capture: Option<Capture> = None;
    // Conversation detection value saved while we override it off for capture.
    let mut old_convo_state: Option<bool> = None;
    let mut restart_backoff = STALL_TIMEOUT;
    let mut next_restart_at = Instant::now();
    info!(
        "[hires] activity monitor started, watching '{}'",
        output::SOURCE_NAME
    );

    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        let app = tokio::task::spawn_blocking(|| output::source_consumer(output::SOURCE_NAME))
            .await
            .unwrap_or(None);
        let enabled = aacp.hires_mic_enabled();
        let recording = app.is_some();

        let worn = aacp.is_worn().await;
        let stalled = capture.is_some()
            && status.since_last_sdu().is_some_and(|d| d > STALL_TIMEOUT);

        match capture_action(enabled, recording, worn, capture.is_some(), stalled) {
            CaptureAction::Start => {
                info!("[hires] recorder detected ({:?}), starting capture", app);
                if let Some(c) = start_capture(&aacp, &addr, &status).await {
                    status.set_capture(app);
                    capture = Some(c);

                    // Only disable (and remember to restore) if it was on.
                    if crate::utils::AppSettings::load().hires_mic_pause_convo
                        && aacp.conversation_detection_enabled().await
                    {
                        old_convo_state = Some(true);
                        aacp.set_conversation_detection(false).await;
                    }
                }
            }
            CaptureAction::Keep => {
                status.set_capture(app);
                restart_backoff = STALL_TIMEOUT;
            }
            CaptureAction::Restart => {
                status.set_capture(app);
                if Instant::now() >= next_restart_at {
                    warn!(
                        "[hires] no audio from device for {}ms, restarting capture",
                        STALL_TIMEOUT.as_millis()
                    );
                    stop_capture(capture.take().unwrap(), &aacp).await;
                    capture = start_capture(&aacp, &addr, &status).await;
                    if capture.is_none() {
                        warn!("[hires] capture restart failed; will retry on next poll");
                        status.reset();
                    }
                    // Back off if the device keeps not delivering (e.g. another
                    // device holds the AirPods' mic), instead of cycling A2DP
                    // every couple of seconds.
                    next_restart_at = Instant::now() + restart_backoff;
                    restart_backoff = (restart_backoff * 2).min(MAX_RESTART_BACKOFF);
                }
            }
            // Disabled, recorder gone, or no bud in an ear: end the 0x58 stream.
            // The virtual source stays up feeding silence, so an active recorder
            // isn't dropped onto the AirPods' HFP mic (handsfree) or another mic.
            CaptureAction::Stop { reset_a2dp } => {
                info!(
                    "[hires] stopping capture (enabled={}, recording={}, worn={})",
                    enabled, recording, worn
                );
                stop_capture(capture.take().unwrap(), &aacp).await;
                // STOP leaves A2DP in the AirPods' mic-active state: media keeps
                // playing from only one earbud until the transport is cycled.
                // Skipped when the buds came out: the START on re-insertion resets.
                if reset_a2dp {
                    schedule_a2dp_reset(&addr);
                }
                if let Some(prev) = old_convo_state.take() {
                    restore_conversation_detection(&aacp, prev).await;
                }
                status.reset();
                restart_backoff = STALL_TIMEOUT;
            }
            CaptureAction::Idle => {}
        }

        // Once disabled and nothing is recording from the source, unload it.
        if !enabled && !recording {
            break;
        }
    }

    if let Some(c) = capture.take() {
        stop_capture(c, &aacp).await;
        if let Some(prev) = old_convo_state.take() {
            restore_conversation_detection(&aacp, prev).await;
        }
    }
    status.reset();
}

/// Turn conversation awareness back on after a capture. If the AirPods are
/// already gone (disconnect mid-call), remember it so the next connection
/// restores it; otherwise it stayed off on the AirPods for good.
async fn restore_conversation_detection(aacp: &AACPManager, enabled: bool) {
    if !aacp.set_conversation_detection(enabled).await && enabled {
        info!("[hires] AirPods gone; will restore conversation awareness on next connect");
        let mut settings = crate::utils::AppSettings::load();
        settings.convo_restore_pending = true;
        settings.save();
    }
}

async fn start_capture(aacp: &AACPManager, addr: &str, status: &MicStatus) -> Option<Capture> {
    let decoder = EldDecoder::new()?;
    let output = Output::open(ELD_SAMPLE_RATE, ELD_CHANNELS as u8)?;

    let rx = aacp.take_audio_channel().await;
    // Start the stall grace period now; the device should deliver SDUs (which
    // refresh this) well within STALL_TIMEOUT.
    status.mark_sdu();
    let decode_thread = spawn_decode_thread(rx, decoder, output, status.clone());

    if let Err(e) = aacp.send_start_audio().await {
        error!("failed to send 0x58 START: {}", e);
        aacp.clear_audio_channel().await;
        return None;
    }
    info!("[aacp] microphone stream started");

    schedule_a2dp_reset(addr);

    Some(Capture {
        decode_thread: Some(decode_thread),
    })
}

// Cycle the A2DP card profile shortly after a 0x58 START or STOP; both leave the
// transport in a bad state (silent after START, one earbud only after STOP).
fn schedule_a2dp_reset(addr: &str) {
    let reset_addr = addr.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(A2DP_RESET_DELAY).await;
        let _ = tokio::task::spawn_blocking(move || output::reset_a2dp(&reset_addr)).await;
    });
}

async fn stop_capture(mut capture: Capture, aacp: &AACPManager) {
    if let Err(e) = aacp.send_stop_audio().await {
        warn!("failed to send 0x58 STOP: {}", e);
    }
    aacp.clear_audio_channel().await;

    if let Some(handle) = capture.decode_thread.take() {
        let _ = tokio::task::spawn_blocking(move || handle.join()).await;
    }
    // The A2DP reset after a STOP is scheduled by the caller (monitor_loop), so a
    // stall restart (STOP immediately followed by START) only resets once.
}

fn spawn_decode_thread(
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut decoder: EldDecoder,
    mut output: Output,
    status: MicStatus,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("hires-decode".into())
        .spawn(move || {
            let mut frames: u64 = 0;
            let mut errors: u64 = 0;
            let mut env: f32 = 0.0;
            let mut pcm: Vec<i16> = Vec::with_capacity(4096);

            while let Some(sdu) = rx.blocking_recv() {
                pcm.clear();
                aacp_audio::demux_type58(&sdu, |au| match decoder.decode(au, &mut pcm) {
                    Some(_) => frames += 1,
                    None => {
                        // insert a silent frame
                        errors += 1;
                        pcm.resize(pcm.len() + ELD_FRAME_SAMPLES * ELD_CHANNELS as usize, 0);
                    }
                });

                let peak = if pcm.is_empty() {
                    0.0
                } else {
                    match output.write(&pcm) {
                        Ok(peak) => peak,
                        Err(()) => {
                            warn!("hi-res output broke; stopping decode loop");
                            break;
                        }
                    }
                };

                env = if peak >= env {
                    peak
                } else {
                    env * LEVEL_RELEASE
                };
                status.set_level(env);

                if frames > 0 && frames % 400 == 0 {
                    let secs = frames as f64 * ELD_FRAME_SAMPLES as f64 / ELD_SAMPLE_RATE as f64;
                    info!(
                        "[audio] {} frames ({:.0}s), {} errors, level {:.2}",
                        frames, secs, errors, env
                    );
                }
            }
            status.set_level(0.0);
            info!(
                "[audio] hi-res decode loop ended ({} frames, {} errors)",
                frames, errors
            );
        })
        .expect("failed to spawn hires-decode thread")
}

#[cfg(test)]
mod tests {
    use super::{CaptureAction, capture_action};

    #[test]
    fn starts_only_when_enabled_recording_and_worn() {
        assert_eq!(capture_action(true, true, true, false, false), CaptureAction::Start);
        assert_eq!(capture_action(false, true, true, false, false), CaptureAction::Idle);
        assert_eq!(capture_action(true, false, true, false, false), CaptureAction::Idle);
        // Both buds out: an app holding the mic must not start the 0x58 stream.
        assert_eq!(capture_action(true, true, false, false, false), CaptureAction::Idle);
    }

    #[test]
    fn buds_out_stops_without_reset_and_never_restarts() {
        assert_eq!(
            capture_action(true, true, false, true, true),
            CaptureAction::Stop { reset_a2dp: false }
        );
        assert_eq!(
            capture_action(true, true, false, true, false),
            CaptureAction::Stop { reset_a2dp: false }
        );
    }

    #[test]
    fn app_closing_mic_while_worn_stops_with_reset() {
        // Leaving a Discord call: recorder gone, buds still in.
        assert_eq!(
            capture_action(true, false, true, true, false),
            CaptureAction::Stop { reset_a2dp: true }
        );
        // Feature disabled mid-capture.
        assert_eq!(
            capture_action(false, true, true, true, false),
            CaptureAction::Stop { reset_a2dp: true }
        );
    }

    #[test]
    fn stall_restarts_only_while_capturing_and_worn() {
        assert_eq!(capture_action(true, true, true, true, true), CaptureAction::Restart);
        assert_eq!(capture_action(true, true, true, true, false), CaptureAction::Keep);
    }
}
