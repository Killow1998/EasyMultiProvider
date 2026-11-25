use std::{iter::Sum, time::Duration};

#[derive(Clone, Copy)]
pub struct FixedWait(Duration);

impl FixedWait {
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

impl<'a> Sum<&'a FixedWait> for FixedWait {
    fn sum<I: Iterator<Item = &'a FixedWait>>(waits: I) -> Self {
        Self(waits.map(|wait| wait.get()).sum())
    }
}
