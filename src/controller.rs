/// Main-thread state and generation checks shared by capture and worker callbacks.
#[derive(Debug)]
pub struct Session {
    pub phase: &'static str,
    pub generation: u64,
    pub ready: bool,
    pub busy: bool,
    pub unloaded: bool,
    pub closing: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            phase: "loading",
            generation: 0,
            ready: false,
            busy: false,
            unloaded: false,
            closing: false,
        }
    }
}

impl Session {
    pub fn start(&mut self) -> Option<u64> {
        if !self.ready
            || self.busy
            || matches!(self.phase, "starting" | "recording" | "delivering")
            || self.closing
        {
            return None;
        }
        self.generation += 1;
        self.phase = "starting";
        Some(self.generation)
    }

    pub fn frame(&mut self, generation: u64) -> bool {
        if self.current(generation) && matches!(self.phase, "starting" | "recording") {
            self.phase = "recording";
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) -> Option<u64> {
        if !matches!(self.phase, "starting" | "recording") {
            return None;
        }
        self.phase = "transcribing";
        self.busy = true;
        Some(self.generation)
    }

    pub fn current(&self, generation: u64) -> bool {
        !self.closing && generation == self.generation
    }

    /// Ends a failed dictation; the worker restarts on the next recording, which stays allowed.
    pub fn fail(&mut self) {
        self.busy = false;
        self.unloaded = true;
    }

    pub fn cancel(&mut self) -> bool {
        if !matches!(self.phase, "starting" | "recording" | "transcribing") {
            return false;
        }
        self.generation += 1;
        self.phase = "idle";
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_only_once_and_reject_old_microphone_callbacks() {
        let mut state = Session {
            ready: true,
            phase: "idle",
            ..Session::default()
        };
        let old = state.start().unwrap();
        assert!(state.frame(old));
        assert_eq!(state.stop(), Some(old));
        assert_eq!(state.stop(), None);
        assert!(state.cancel());
        state.busy = false;
        let new = state.start().unwrap();
        assert!(!state.frame(old));
        assert!(state.frame(new));
    }
    #[test]
    fn cancelled_decode_cannot_deliver_and_delivery_cannot_claim_cancellation() {
        let mut state = Session {
            ready: true,
            phase: "idle",
            ..Session::default()
        };
        let generation = state.start().unwrap();
        state.stop();
        state.cancel();
        assert!(!state.current(generation));
        state.phase = "delivering";
        let generation = state.generation;
        assert!(!state.cancel());
        assert!(state.current(generation));
        state.closing = true;
        assert!(!state.current(generation));
    }
    #[test]
    fn cancel_without_running_dictation_changes_nothing() {
        for phase in ["idle", "done", "error", "loading", "delivering"] {
            let mut state = Session {
                ready: true,
                phase,
                generation: 7,
                ..Session::default()
            };
            assert!(!state.cancel(), "{phase}");
            assert_eq!((state.phase, state.generation), (phase, 7));
        }
    }
    #[test]
    fn press_after_failed_dictation_records_again() {
        let mut state = Session {
            ready: true,
            phase: "idle",
            ..Session::default()
        };
        let failed = state.start().unwrap();
        state.stop();
        state.fail();
        state.phase = "error";
        assert!(state.unloaded);
        assert!(state.start().is_some_and(|next| next > failed));
    }
}
