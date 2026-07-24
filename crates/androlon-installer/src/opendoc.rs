//! Receiving the double-clicked APK.
//!
//! Launch Services does not pass opened documents in `argv` — it sends an
//! `odoc` Apple Event, which AppKit turns into `application:openFile:` on the
//! app delegate. So a `.apk` handler *must* install a delegate and pump the
//! event loop once before it can know what it was asked to open.
//!
//! Ordering we rely on: `application:openFile:` is delivered before
//! `applicationDidFinishLaunching:`, so by the time the latter stops the run
//! loop, the path (if any) is already recorded.

#![allow(non_snake_case)]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSEvent, NSEventModifierFlags, NSEventSubtype,
    NSEventType,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSString};
use std::cell::RefCell;
use std::path::PathBuf;

#[derive(Default)]
pub struct DelegateIvars {
    opened: RefCell<Option<PathBuf>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AndrolonOpenDocDelegate"]
    #[ivars = DelegateIvars]
    pub struct OpenDocDelegate;

    unsafe impl NSObjectProtocol for OpenDocDelegate {}

    unsafe impl NSApplicationDelegate for OpenDocDelegate {
        #[unsafe(method(application:openFile:))]
        fn application_openFile(&self, _app: &NSApplication, path: &NSString) -> bool {
            *self.ivars().opened.borrow_mut() = Some(PathBuf::from(path.to_string()));
            true
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn applicationDidFinishLaunching(&self, _n: &NSNotification) {
            // Launch-time document events have already arrived; hand control
            // back to `main` so the wizard can run as a straight-line flow.
            let mtm = MainThreadMarker::new().unwrap();
            stop_run_loop(&NSApplication::sharedApplication(mtm));
        }
    }
);

impl OpenDocDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

/// Run the event loop just long enough to receive a launch-time `odoc`, and
/// return the document it asked us to open (None when launched bare).
pub fn opened_document(mtm: MainThreadMarker) -> Option<PathBuf> {
    let app = NSApplication::sharedApplication(mtm);
    let delegate = OpenDocDelegate::new(mtm);
    let proto = ProtocolObject::from_ref(&*delegate);
    unsafe {
        app.setDelegate(Some(proto));
        app.run(); // returns via applicationDidFinishLaunching → stop
        app.setDelegate(None); // the wizard drives the app from here on
    }
    let path = delegate.ivars().opened.borrow().clone();
    path.filter(|p| p.exists())
}

/// `stop:` only takes effect once another event is processed, so post a
/// no-op event to wake the loop immediately.
fn stop_run_loop(app: &NSApplication) {
    unsafe {
        app.stop(None);
        let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            NSEventSubtype::WindowExposed.0,
            0,
            0,
        );
        if let Some(event) = event {
            app.postEvent_atStart(&event, true);
        }
    }
}
