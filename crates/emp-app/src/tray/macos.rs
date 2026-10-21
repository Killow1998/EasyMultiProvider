//! AppKit and the menu bar item belong to the original launch thread.
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

pub(super) struct Events(Retained<NSApplication>);

impl Events {
    pub(super) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let main = MainThreadMarker::new().ok_or("EMP menu bar requires the main thread")?;
        let application = NSApplication::sharedApplication(main);
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        application.finishLaunching();
        Ok(Self(application))
    }

    pub(super) fn pump(&mut self) {
        autoreleasepool(|_| {
            let deadline = NSDate::dateWithTimeIntervalSinceNow(0.05);
            // SAFETY: Foundation exports this immutable run-loop mode for the
            // process lifetime; AppKit calls remain on the launch thread.
            let mode = unsafe { NSDefaultRunLoopMode };
            if let Some(event) = self.0.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&deadline),
                mode,
                true,
            ) {
                self.0.sendEvent(&event);
            }
            self.0.updateWindows();
        });
    }
}
