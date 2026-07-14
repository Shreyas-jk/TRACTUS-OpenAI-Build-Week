#[derive(Clone, Debug, Default)]
pub struct History {
    loop_result: Option<(u32, String)>,
}

impl History {
    pub fn loop_detected(&self) -> Option<(u32, String)> {
        self.loop_result.clone()
    }

    /// Temporary construction hook until the Section 6 ring-buffer detector is wired.
    pub fn with_detected_loop(n: u32, signature: impl Into<String>) -> Self {
        Self {
            loop_result: Some((n, signature.into())),
        }
    }
}
