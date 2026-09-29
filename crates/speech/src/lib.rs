//! The speech layer: push announcements to the user's screen reader for
//! events that have no natural focus change (assemble results, step
//! narration, run completion). Structural accessibility stays with native
//! widgets; this crate only handles directed speech.

/// Where announcements go. Implementations must never block meaningfully:
/// speech is fire-and-forget from the caller's perspective.
pub trait Speaker {
    /// Speak `text`. `interrupt` cancels whatever the reader is saying.
    fn speak(&mut self, text: &str, interrupt: bool);
    /// Stop all queued speech.
    fn stop(&mut self);
    /// Human-readable backend name for the settings dialog and logs.
    fn backend_name(&self) -> &str;
    /// Whether the backend actually connected (reader running).
    fn is_connected(&self) -> bool;
}

/// No-op speaker: speech disabled or no backend available.
pub struct NullSpeaker;

impl Speaker for NullSpeaker {
    fn speak(&mut self, _text: &str, _interrupt: bool) {}
    fn stop(&mut self) {}
    fn backend_name(&self) -> &str {
        "none"
    }
    fn is_connected(&self) -> bool {
        false
    }
}

#[cfg(feature = "prism")]
mod prism_backend {
    use super::Speaker;

    /// Prism-backed speaker. Prism routes to whichever reader bridge is
    /// available through one API. Backends are acquired per call from
    /// Prism's cache, so the first `speak` pays initialization and later
    /// calls reuse it.
    pub struct PrismSpeaker {
        prism: prismer::Prism,
        name: String,
    }

    impl PrismSpeaker {
        /// Connect and verify that at least one backend initializes.
        pub fn connect() -> Option<Self> {
            let prism = prismer::Prism::new().ok()?;
            let name = {
                let backend = prism.acquire_best().ok()?;
                backend.name()
            };
            Some(PrismSpeaker { prism, name })
        }

        /// Names of every backend Prism knows about on this machine,
        /// regardless of availability. For the settings dialog and spikes.
        pub fn known_backends() -> Vec<String> {
            match prismer::Prism::new() {
                Ok(prism) => prism
                    .backend_ids()
                    .into_iter()
                    .filter_map(|id| prism.backend_name(id))
                    .collect(),
                Err(_) => Vec::new(),
            }
        }

        fn with_backend(&self, f: impl FnOnce(&prismer::Backend)) {
            if let Ok(backend) = self.prism.acquire_best() {
                f(&backend);
            }
        }
    }

    impl Speaker for PrismSpeaker {
        fn speak(&mut self, text: &str, interrupt: bool) {
            // Failure to speak is never fatal; the UI keeps working.
            self.with_backend(|b| {
                let _ = b.speak(text, interrupt);
            });
        }

        fn stop(&mut self) {
            self.with_backend(|b| {
                let _ = b.stop();
            });
        }

        fn backend_name(&self) -> &str {
            &self.name
        }

        fn is_connected(&self) -> bool {
            self.prism.backend_count() > 0
        }
    }
}

#[cfg(feature = "prism")]
pub use prism_backend::PrismSpeaker;

/// Pick the best available speaker. Order: Prism, then none (later phases
/// add the direct NVDA controller client and SAPI fallbacks behind the same
/// trait).
pub fn best_speaker() -> Box<dyn Speaker> {
    #[cfg(feature = "prism")]
    {
        if let Some(p) = PrismSpeaker::connect() {
            return Box::new(p);
        }
    }
    Box::new(NullSpeaker)
}
