//! The macOS "About Runki" panel.
//!
//! winit adds an About item to the app menu that opens the bare standard
//! panel. Run with `cargo run`, Runki is a plain binary rather than an `.app`
//! bundle, so that panel has no icon, version or licence to show; even the
//! release bundle's `Info.plist` has no licence or link. `install` points the
//! item at the same standard panel, filled in with Runki's own details.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName,
    NSAboutPanelOptionApplicationVersion, NSAboutPanelOptionCredits, NSAboutPanelOptionVersion,
    NSApplication, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage,
    NSLinkAttributeName, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment,
};
use objc2_foundation::{
    ns_string, NSData, NSDictionary, NSMutableAttributedString, NSRange, NSString, NSURL,
};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
const COPYRIGHT: &str = "© 2026 Francisco Hola";
const LICENCE_LINE: &str = "Free and open source under the MIT License.";
const REPO_URL: &str = "https://github.com/fhello23/runki";
const REPO_LABEL: &str = "github.com/fhello23/runki";

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // has no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RunkiAboutTarget"]
    struct AboutTarget;

    impl AboutTarget {
        #[unsafe(method(showAbout:))]
        fn show_about(&self, _sender: Option<&AnyObject>) {
            show_panel(self.mtm());
        }
    }
);

impl AboutTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSObject's plain `init`.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// Points the app menu's About item at Runki's panel. Call on the main
/// thread once the menu exists (from the eframe app creator). Does nothing if
/// the menu is not the one winit builds.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let item = NSApplication::sharedApplication(mtm)
        .mainMenu()
        .and_then(|bar| bar.itemAtIndex(0))
        .and_then(|app_item| app_item.submenu())
        .and_then(|app_menu| app_menu.itemAtIndex(0));
    let Some(item) = item else {
        return;
    };
    if item.action() != Some(sel!(orderFrontStandardAboutPanel:)) {
        return;
    }
    let target = AboutTarget::new(mtm);
    // SAFETY: `target` implements `showAbout:`, and is kept alive below.
    unsafe {
        item.setTarget(Some(&target));
        item.setAction(Some(sel!(showAbout:)));
    }
    item.setTitle(ns_string!("About Runki"));
    // Menu items hold their target weakly; this one is needed until exit.
    std::mem::forget(target);
}

fn show_panel(mtm: MainThreadMarker) {
    let icon = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON_PNG));
    let name = NSString::from_str("Runki");
    let version = NSString::from_str(runki::VERSION);
    // An empty build version hides the "(…)" after the version.
    let build = NSString::from_str("");
    let copyright = NSString::from_str(COPYRIGHT);
    let credits = credits();

    // SAFETY: the option keys are AppKit constants.
    let (mut keys, mut values): (Vec<&NSString>, Vec<&AnyObject>) = unsafe {
        (
            vec![
                NSAboutPanelOptionApplicationName,
                NSAboutPanelOptionApplicationVersion,
                NSAboutPanelOptionVersion,
                NSAboutPanelOptionCredits,
                // AppKit has no constant for this key.
                ns_string!("Copyright"),
            ],
            vec![&name, &version, &build, &credits, &copyright],
        )
    };
    if let Some(icon) = &icon {
        keys.push(unsafe { NSAboutPanelOptionApplicationIcon });
        values.push(icon);
    }
    let options = NSDictionary::from_slices(&keys, &values);
    // SAFETY: every value has the type AppKit documents for its key.
    unsafe {
        NSApplication::sharedApplication(mtm).orderFrontStandardAboutPanelWithOptions(&options)
    };
}

/// The licence line and a link to the repository, centred in the panel's
/// small system font. The colour adapts to light and dark mode.
fn credits() -> Retained<NSMutableAttributedString> {
    let text = NSString::from_str(&format!("{LICENCE_LINE}\n{REPO_LABEL}"));
    let credits = NSMutableAttributedString::from_nsstring(&text);
    let all = NSRange::new(0, text.length());
    let label_len = NSString::from_str(REPO_LABEL).length();
    let link = NSRange::new(all.length - label_len, label_len);

    let font = NSFont::systemFontOfSize(NSFont::smallSystemFontSize());
    let colour = NSColor::secondaryLabelColor();
    let centred = NSMutableParagraphStyle::new();
    centred.setAlignment(NSTextAlignment::Center);
    let url = NSURL::URLWithString(&NSString::from_str(REPO_URL));

    // SAFETY: each value has the type AppKit expects for its attribute.
    unsafe {
        credits.addAttribute_value_range(NSFontAttributeName, &font, all);
        credits.addAttribute_value_range(NSForegroundColorAttributeName, &colour, all);
        credits.addAttribute_value_range(NSParagraphStyleAttributeName, &centred, all);
        if let Some(url) = &url {
            credits.addAttribute_value_range(NSLinkAttributeName, url, link);
        }
    }
    credits
}
