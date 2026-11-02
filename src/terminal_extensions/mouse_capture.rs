use crossterm::{event, execute};

/// Helper managing proper setup and teardown of terminal mouse capture
///
/// Without [`event::EnableMouseCapture`] no terminal reports mouse events, so
/// [`crate::MouseClickMode`] would never see a click.
#[derive(Default)]
pub(crate) struct MouseCaptureGuard {
    enabled: bool,
    active: bool,
}

impl MouseCaptureGuard {
    pub fn set(&mut self, enable: bool) {
        self.enabled = enable;
    }
    pub fn enter(&mut self) {
        if self.enabled && !self.active {
            let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
            self.active = true;
        }
    }
    pub fn exit(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
            self.active = false;
        }
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
        }
    }
}
