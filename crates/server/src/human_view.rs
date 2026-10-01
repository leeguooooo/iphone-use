//! Browser view of a phone handed to a person (Direct backend only).
//!
//! `POST /agent/mode {"mode":"human"}` stops the on-phone WDA runner and opens
//! iPhone Mirroring on the Mac. Until now the person then had to be at that Mac,
//! or reach it over Screen Sharing. This module lets them stay in the browser:
//! while the phone is handed over, it captures the Mirroring window into the
//! WebRTC pipeline the legacy Mirror backend uses, and injects the browser's
//! taps into that window. Taking the phone back for the agent stops both.
//!
//! The capture needs the Mac's Screen Recording and Accessibility grants, which
//! the Direct backend otherwise never asks for. A missing grant fails the view
//! with a named reason instead of failing the hand-off: the person can still
//! use the Mirroring window on the Mac.
//!
//! One daemon drives one phone, so the view is a process-global, installed by
//! `main` for the Direct backend (the same choice as the hand-off flag, which
//! spares every `AppState` constructor a new field).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use core::encode::{NullPipeline, SwitchablePipeline, VideoPipeline};

use crate::http::LeaseState;
use crate::input_bridge::InputInjector;

/// How long a starting view waits for the Mirroring window. Opening the app
/// and connecting to the phone takes a few seconds; a phone that is in use
/// keeps Mirroring on its "iPhone in Use" screen, which is still a window.
const WINDOW_WAIT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Off,
    Starting,
    Live,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Starting => "starting",
            Phase::Live => "live",
            Phase::Failed => "failed",
        }
    }
}

pub struct HumanView {
    pipeline: Arc<SwitchablePipeline>,
    injector: InputInjector,
    lease_state: Arc<Mutex<LeaseState>>,
    status: Mutex<(Phase, String)>,
    /// Held across a generation check and the swap it guards, so a stop can
    /// never land between a start deciding to install and installing.
    transition: Mutex<()>,
    /// Bumped by every start and stop. A start that finds the generation moved
    /// on was superseded and must not install what it built.
    generation: AtomicU64,
}

static VIEW: OnceLock<Arc<HumanView>> = OnceLock::new();

/// Install the process's view. `pipeline` and `injector` must be the very
/// objects `AppState` hands to WebRTC sessions.
pub fn install(
    pipeline: Arc<SwitchablePipeline>,
    injector: InputInjector,
    lease_state: Arc<Mutex<LeaseState>>,
) {
    let _ = VIEW.set(Arc::new(HumanView {
        pipeline,
        injector,
        lease_state,
        status: Mutex::new((Phase::Off, String::new())),
        transition: Mutex::new(()),
        generation: AtomicU64::new(0),
    }));
}

fn view() -> Option<&'static Arc<HumanView>> {
    VIEW.get()
}

/// The view's phase and, when it failed, why. `off` when no view is installed
/// (the Mirror backend, or tests).
pub fn snapshot() -> (Phase, String) {
    match view() {
        Some(view) => view.status(),
        None => (Phase::Off, String::new()),
    }
}

/// Whether the browser may open a WebRTC session to the handed-over phone.
pub fn is_live() -> bool {
    snapshot().0 == Phase::Live
}

/// Start capturing the Mirroring window in the background. Returns at once;
/// progress shows in [`snapshot`].
pub fn start() {
    let Some(view) = view() else { return };
    // Bump and publish under the transition lock, or a stop landing between
    // the two would leave "starting" behind with no start left to finish it.
    let generation = {
        let _transition = view.lock_transition();
        let generation = view.generation.fetch_add(1, Ordering::AcqRel) + 1;
        view.set_status(Phase::Starting, String::new());
        generation
    };
    let worker = Arc::clone(view);
    if let Err(error) = std::thread::Builder::new()
        .name("human-view-start".into())
        .spawn(move || worker.run_start(generation))
    {
        view.fail(generation, format!("start: could not spawn the capture thread ({error})"));
    }
}

/// Stop the capture and the input sink. Blocks while ScreenCaptureKit stops,
/// so call it from a blocking context.
pub fn stop() {
    let Some(view) = view() else { return };
    let previous = {
        let _transition = view.lock_transition();
        view.generation.fetch_add(1, Ordering::AcqRel);
        view.injector.replace_with(&InputInjector::null());
        view.set_status(Phase::Off, String::new());
        view.pipeline.replace(Arc::new(NullPipeline::new()))
    };
    // Outside the lock: dropping a capture pipeline waits for SCK to stop.
    drop(previous);
}

impl HumanView {
    fn status(&self) -> (Phase, String) {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_status(&self, phase: Phase, message: String) {
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (phase, message);
    }

    fn lock_transition(&self) -> std::sync::MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    fn fail(&self, generation: u64, message: String) {
        let _transition = self.lock_transition();
        if self.current(generation) {
            tracing::warn!("browser view of the handed-over phone failed: {message}");
            self.set_status(Phase::Failed, message);
        }
    }

    fn run_start(&self, generation: u64) {
        // Ask for both missing grants at once, so one hand-off surfaces both
        // system prompts. A grant recorded for an earlier signature of the
        // app (a self-signed or ad-hoc build) still shows as switched on in
        // System Settings but does not match this one; only removing the app
        // from the list and adding it again rewrites it.
        let tcc = crate::macos::tcc_status();
        if !tcc.screen_recording {
            crate::macos::request_screen_capture();
        }
        if !tcc.accessibility {
            crate::macos::request_accessibility();
        }
        const STALE: &str = "if iPhoneUse is already switched on there, remove it with − and add it again (an earlier build's grant does not cover this one)";
        if !tcc.screen_recording {
            return self.fail(
                generation,
                format!("screen_recording: allow iPhoneUse in System Settings › Privacy & Security › Screen Recording, restart the daemon, then hand the phone over again; {STALE}"),
            );
        }
        if !tcc.accessibility {
            return self.fail(
                generation,
                format!("accessibility: allow iPhoneUse in System Settings › Privacy & Security › Accessibility, then hand the phone over again; {STALE}"),
            );
        }

        let deadline = Instant::now() + WINDOW_WAIT;
        let geometry = loop {
            if !self.current(generation) {
                return;
            }
            match core::capture::find_mirroring_geometry() {
                Ok(geometry) => break geometry,
                Err(error) if Instant::now() >= deadline => {
                    return self.fail(
                        generation,
                        format!("mirroring_window: iPhone Mirroring did not open on the Mac ({error:#})"),
                    );
                }
                Err(_) => std::thread::sleep(Duration::from_secs(1)),
            }
        };

        let pipeline = match core::encode::start_pipeline(core::encode::PipelineConfig::default()) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                return self.fail(generation, format!("capture: {error:#}"));
            }
        };
        let lease_state = Arc::clone(&self.lease_state);
        let injector = crate::input_bridge::spawn_injector(geometry, move || {
            lease_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .allows_injection()
        });

        // Install only if nobody stopped or restarted the view meanwhile;
        // otherwise drop what was built (that stops the capture again).
        let previous = {
            let _transition = self.lock_transition();
            if !self.current(generation) {
                None
            } else {
                self.injector.replace_with(&injector);
                self.set_status(Phase::Live, String::new());
                Some(self.pipeline.replace(pipeline.clone() as Arc<dyn VideoPipeline>))
            }
        };
        match previous {
            Some(previous) => {
                tracing::info!("browser view of the handed-over phone is live");
                drop(previous);
            }
            // Superseded: dropping our only handles stops the capture and
            // ends the injector thread.
            None => drop((pipeline, injector)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_name_themselves_for_status() {
        assert_eq!(Phase::Off.as_str(), "off");
        assert_eq!(Phase::Starting.as_str(), "starting");
        assert_eq!(Phase::Live.as_str(), "live");
        assert_eq!(Phase::Failed.as_str(), "failed");
    }

    #[test]
    fn no_installed_view_reads_as_off() {
        // Tests never call `install`, so the global stays empty.
        assert_eq!(snapshot().0, Phase::Off);
        assert!(!is_live());
    }
}
