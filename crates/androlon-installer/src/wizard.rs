//! The install wizard window — macOS `Installer.app` idiom: a step sidebar on
//! the left, content on the right, Go Back / Continue in a bottom button bar.
//!
//! Layout is frame-based rather than auto-layout: the window is fixed-size
//! (so is Apple's own Installer), and manual frames keep the AppKit surface
//! we touch small.
//!
//! Flow control is a modal loop rather than async callbacks — each button
//! ends the modal session with its tag as the response code, so the wizard
//! reads as a straight-line state machine in `run()` instead of a web of
//! callbacks. The install step uses a modal *session*, which keeps the window
//! live and repainting while work happens on a background thread.

#![allow(non_snake_case)]

use androlon_core::appify::ApkInfo;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBezelStyle, NSBox, NSBoxType, NSButton, NSColor, NSFont, NSImage, NSImageView,
    NSModalResponse, NSProgressIndicator, NSProgressIndicatorStyle, NSTextField, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use std::cell::Cell;
use std::path::{Path, PathBuf};

const WIN_W: f64 = 620.0;
const WIN_H: f64 = 460.0;
const SIDEBAR_W: f64 = 190.0;
const BAR_H: f64 = 52.0;

/// Button tags, doubling as modal response codes.
pub const TAG_CONTINUE: isize = 1;
pub const TAG_BACK: isize = 2;
pub const TAG_CANCEL: isize = 3;
pub const TAG_CHOOSE: isize = 4;

/// A tiny Objective-C class so AppKit buttons have a target/action. Each
/// click ends the modal session with the sender's tag.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AndrolonWizardTarget"]
    #[ivars = ()]
    pub struct WizardTarget;

    impl WizardTarget {
        #[unsafe(method(buttonClicked:))]
        fn button_clicked(&self, sender: &AnyObject) {
            let mtm = MainThreadMarker::new().unwrap();
            let tag: isize = unsafe { msg_send![sender, tag] };
            unsafe { NSApplication::sharedApplication(mtm).stopModalWithCode(tag) };
        }
    }
);

impl WizardTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
    fn action() -> Sel {
        sel!(buttonClicked:)
    }
}

/// The wizard's steps, in order — mirrored by the sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Introduction,
    Destination,
    InstallType,
    Installing,
    Summary,
}

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::Introduction => "Introduction",
            Step::Destination => "Destination Select",
            Step::InstallType => "Installation Type",
            Step::Installing => "Installation",
            Step::Summary => "Summary",
        }
    }
    const ALL: [Step; 5] = [
        Step::Introduction,
        Step::Destination,
        Step::InstallType,
        Step::Installing,
        Step::Summary,
    ];
    fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

pub struct Wizard {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    target: Retained<WizardTarget>,
    step_labels: Vec<Retained<NSTextField>>,
    step_dots: Vec<Retained<NSTextField>>,
    heading: Retained<NSTextField>,
    body: Retained<NSTextField>,
    progress: Retained<NSProgressIndicator>,
    back: Retained<NSButton>,
    forward: Retained<NSButton>,
    choose: Retained<NSButton>,
    current: Cell<Step>,
}

impl Wizard {
    pub fn new(mtm: MainThreadMarker, info: &ApkInfo, icon: Option<&NSImage>) -> Self {
        unsafe {
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                objc2_app_kit::NSBackingStoreType::Buffered,
                false,
            );
            window.setTitle(&NSString::from_str(&format!("Install {}", info.label)));
            window.center();
            let content = window.contentView().expect("window content view");

            // Sidebar: a recessed box, like Installer's step list.
            let sidebar = NSBox::initWithFrame(
                NSBox::alloc(mtm),
                NSRect::new(NSPoint::new(-2.0, -2.0), NSSize::new(SIDEBAR_W + 2.0, WIN_H + 4.0)),
            );
            sidebar.setBoxType(NSBoxType::Custom);
            sidebar.setBorderWidth(0.0);
            sidebar.setFillColor(&NSColor::windowBackgroundColor());
            sidebar.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
            content.addSubview(&sidebar);

            // The app's own icon above the step list.
            if let Some(icon) = icon {
                let iv = NSImageView::initWithFrame(
                    NSImageView::alloc(mtm),
                    NSRect::new(
                        NSPoint::new(SIDEBAR_W / 2.0 - 32.0, WIN_H - 100.0),
                        NSSize::new(64.0, 64.0),
                    ),
                );
                iv.setImage(Some(icon));
                content.addSubview(&iv);
            }

            // Step list.
            let mut step_labels = Vec::new();
            let mut step_dots = Vec::new();
            let mut y = WIN_H - 140.0;
            for step in Step::ALL {
                // Bullet + title, so the dot can be blue while the title is
                // bold — exactly how Installer marks the current step.
                let dot = make_label(mtm, "●", 11.0, false);
                dot.setFrame(NSRect::new(NSPoint::new(22.0, y + 1.0), NSSize::new(14.0, 18.0)));
                content.addSubview(&dot);
                step_dots.push(dot);

                let label = make_label(mtm, step.title(), 13.0, false);
                label.setFrame(NSRect::new(
                    NSPoint::new(42.0, y),
                    NSSize::new(SIDEBAR_W - 54.0, 20.0),
                ));
                content.addSubview(&label);
                step_labels.push(label);
                y -= 26.0;
            }

            // Content pane.
            let heading = make_label(mtm, "", 15.0, true);
            heading.setFrame(NSRect::new(
                NSPoint::new(SIDEBAR_W + 24.0, WIN_H - 70.0),
                NSSize::new(WIN_W - SIDEBAR_W - 48.0, 22.0),
            ));
            content.addSubview(&heading);

            // The recessed content frame the native Installer draws its copy
            // inside. Added before the body label so the label draws on top.
            let box_x = SIDEBAR_W + 24.0;
            let box_y = BAR_H + 8.0;
            let box_w = WIN_W - SIDEBAR_W - 48.0;
            let box_h = WIN_H - 92.0 - box_y;
            let content_box = NSBox::initWithFrame(
                NSBox::alloc(mtm),
                NSRect::new(NSPoint::new(box_x, box_y), NSSize::new(box_w, box_h)),
            );
            content_box.setBoxType(NSBoxType::Primary);
            content_box.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
            content_box.setContentViewMargins(NSSize::new(0.0, 0.0));
            content.addSubview(&content_box);

            let body = make_label(mtm, "", 13.0, false);
            body.setFrame(NSRect::new(
                NSPoint::new(box_x + 18.0, box_y + 16.0),
                NSSize::new(box_w - 36.0, box_h - 34.0),
            ));
            content.addSubview(&body);

            let progress = NSProgressIndicator::initWithFrame(
                NSProgressIndicator::alloc(mtm),
                NSRect::new(
                    NSPoint::new(box_x + 18.0, box_y + 24.0),
                    NSSize::new(box_w - 36.0, 20.0),
                ),
            );
            progress.setStyle(NSProgressIndicatorStyle::Bar);
            progress.setIndeterminate(true);
            progress.setHidden(true);
            content.addSubview(&progress);

            let target = WizardTarget::new(mtm);

            // Button bar.
            let forward = make_button(mtm, "Continue", TAG_CONTINUE, &target);
            forward.setFrame(NSRect::new(
                NSPoint::new(WIN_W - 130.0, 14.0),
                NSSize::new(112.0, 30.0),
            ));
            forward.setKeyEquivalent(&NSString::from_str("\r")); // Return = default
            content.addSubview(&forward);

            let back = make_button(mtm, "Go Back", TAG_BACK, &target);
            back.setFrame(NSRect::new(
                NSPoint::new(WIN_W - 248.0, 14.0),
                NSSize::new(112.0, 30.0),
            ));
            content.addSubview(&back);

            let choose = make_button(mtm, "Choose Folder…", TAG_CHOOSE, &target);
            choose.setFrame(NSRect::new(
                NSPoint::new(SIDEBAR_W + 24.0, 14.0),
                NSSize::new(150.0, 30.0),
            ));
            choose.setHidden(true);
            content.addSubview(&choose);

            Wizard {
                mtm,
                window,
                target,
                step_labels,
                step_dots,
                heading,
                body,
                progress,
                back,
                forward,
                choose,
                current: Cell::new(Step::Introduction),
            }
        }
    }

    /// Paint one step: sidebar highlighting, text, and which buttons apply.
    pub fn show_step(&self, step: Step, info: &ApkInfo, dest: &Path) {
        self.current.set(step);
        unsafe {
            for (i, label) in self.step_labels.iter().enumerate() {
                let current = i == step.index();
                // Current step is bold + label colour; others are dimmed —
                // the same read as Installer's blue/grey step dots.
                let font = if current {
                    NSFont::boldSystemFontOfSize(13.0)
                } else {
                    NSFont::systemFontOfSize(13.0)
                };
                label.setFont(Some(&font));
                let colour = if current {
                    NSColor::labelColor()
                } else {
                    NSColor::secondaryLabelColor()
                };
                label.setTextColor(Some(&colour));
                let dot_colour = if current {
                    NSColor::controlAccentColor()
                } else {
                    NSColor::tertiaryLabelColor()
                };
                self.step_dots[i].setTextColor(Some(&dot_colour));
            }

            let (heading, body) = match step {
                Step::Introduction => (
                    format!("Welcome to the {} Installer", info.label),
                    format!(
                        "You will be guided through the steps necessary to install this Android \
                         app on your Mac.\n\n{} will run in its own window, with its own icon in \
                         the Dock, like any other Mac app. The Android runtime it needs starts \
                         automatically and stays out of your way.",
                        info.label
                    ),
                ),
                Step::Destination => (
                    "Select a Destination".to_string(),
                    format!(
                        "{} will be created as a Mac app in:\n\n{}\n\nChoose a different folder \
                         if you'd prefer it elsewhere.",
                        info.label,
                        dest.display()
                    ),
                ),
                Step::InstallType => (
                    format!("Standard Install of {}", info.label),
                    format!(
                        "This will install {} ({}) into the Android runtime and create an app in \
                         {}.\n\nVersion {}  ·  Requires Android API {}  ·  {:.1} MB\n\nClick \
                         Install to perform a standard installation.",
                        info.label,
                        info.package,
                        dest.display(),
                        info.version,
                        info.min_sdk,
                        info.size_bytes as f64 / (1024.0 * 1024.0),
                    ),
                ),
                Step::Installing => (
                    "Installing…".to_string(),
                    format!("Installing {} and creating its Mac app.", info.label),
                ),
                Step::Summary => (
                    "The installation was successful.".to_string(),
                    format!(
                        "{} is installed and ready.\n\nIt has been added to {} — open it from \
                         there, from Launchpad, or from Spotlight, like any Mac app.",
                        info.label,
                        dest.display()
                    ),
                ),
            };
            self.heading.setStringValue(&NSString::from_str(&heading));
            self.body.setStringValue(&NSString::from_str(&body));

            self.choose.setHidden(step != Step::Destination);
            // Installer keeps Go Back visible and greys it out, rather than
            // making the button bar jump around between steps.
            self.back
                .setEnabled(!matches!(step, Step::Introduction | Step::Installing | Step::Summary));
            self.forward.setHidden(step == Step::Installing);
            self.forward.setTitle(&NSString::from_str(match step {
                Step::InstallType => "Install",
                Step::Summary => "Close",
                _ => "Continue",
            }));
            self.progress.setHidden(step != Step::Installing);
        }
    }

    pub fn show(&self) {
        unsafe {
            NSApplication::sharedApplication(self.mtm).activate();
            self.window.makeKeyAndOrderFront(None);
        }
    }

    /// Run the window modally until a button is pressed; returns its tag.
    /// A closed window reads as Cancel.
    pub fn next_action(&self) -> isize {
        let app = NSApplication::sharedApplication(self.mtm);
        let response: NSModalResponse = unsafe { app.runModalForWindow(&self.window) };
        if response == 0 {
            TAG_CANCEL // window closed
        } else {
            response
        }
    }

    /// Run `work` on a background thread while the window keeps repainting.
    /// Used for the install step, which is slow enough to look frozen
    /// otherwise.
    pub fn run_with_progress<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let app = NSApplication::sharedApplication(self.mtm);
        unsafe {
            self.progress.startAnimation(None);
            let session = app.beginModalSessionForWindow(&self.window);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(work());
            });
            let result = loop {
                app.runModalSession(session);
                if let Ok(result) = rx.try_recv() {
                    break result;
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            };
            app.endModalSession(session);
            self.progress.stopAnimation(None);
            result
        }
    }

    pub fn close(&self) {
        unsafe { self.window.close() };
    }

    pub fn window(&self) -> &NSWindow {
        &self.window
    }
}

fn make_label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    bold: bool,
) -> Retained<NSTextField> {
    unsafe {
        let label = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::ZERO);
        label.setStringValue(&NSString::from_str(text));
        label.setBezeled(false);
        label.setDrawsBackground(false);
        label.setEditable(false);
        label.setSelectable(false);
        let font = if bold {
            NSFont::boldSystemFontOfSize(size)
        } else {
            NSFont::systemFontOfSize(size)
        };
        label.setFont(Some(&font));
        // Wrap long body copy instead of clipping it to one line.
        let cell = label.cell();
        if let Some(cell) = cell {
            let _: () = msg_send![&*cell, setWraps: true];
        }
        label
    }
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    tag: isize,
    target: &WizardTarget,
) -> Retained<NSButton> {
    unsafe {
        let button = NSButton::initWithFrame(NSButton::alloc(mtm), NSRect::ZERO);
        button.setTitle(&NSString::from_str(title));
        button.setBezelStyle(NSBezelStyle::Rounded);
        button.setTag(tag);
        button.setTarget(Some(target));
        button.setAction(Some(WizardTarget::action()));
        button
    }
}

/// System folder picker, seeded at `start`.
pub fn choose_folder(mtm: MainThreadMarker, start: &Path) -> Option<PathBuf> {
    use objc2_app_kit::NSOpenPanel;
    use objc2_foundation::NSURL;
    unsafe {
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setCanCreateDirectories(true);
        panel.setAllowsMultipleSelection(false);
        panel.setMessage(Some(&NSString::from_str("Where should the app be created?")));
        panel.setPrompt(Some(&NSString::from_str("Use Folder")));
        let start = NSString::from_str(&start.display().to_string());
        panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&start)));
        if panel.runModal() != 1 {
            return None;
        }
        let url = panel.URL()?;
        url.path().map(|p| PathBuf::from(p.to_string()))
    }
}
