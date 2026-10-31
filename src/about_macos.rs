//! The macOS app menu: About Runki and LAN web server controls.
//!
//! winit adds an About item to the app menu that opens the bare standard
//! panel. Run with `cargo run`, Runki is a plain binary rather than an `.app`
//! bundle, so that panel has no icon, version or licence to show; even the
//! release bundle's `Info.plist` has no licence or link. `install` points the
//! item at the same standard panel, filled in with Runki's own details.

use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName,
    NSAboutPanelOptionApplicationVersion, NSAboutPanelOptionCredits, NSAboutPanelOptionVersion,
    NSApplication, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSLinkAttributeName, NSMenu,
    NSMenuDelegate, NSMenuItem, NSMenuItemValidation, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSPasteboard, NSPasteboardTypeString, NSTextAlignment,
};
use objc2_foundation::{
    ns_string, NSData, NSDictionary, NSMutableAttributedString, NSObjectProtocol, NSRange,
    NSString, NSURL,
};

use runki::web_menu::WebMenuBridge;

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
pub fn install(bridge: Arc<WebMenuBridge>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app_menu = NSApplication::sharedApplication(mtm)
        .mainMenu()
        .and_then(|bar| bar.itemAtIndex(0))
        .and_then(|app_item| app_item.submenu());
    let Some(app_menu) = app_menu else {
        return;
    };
    let Some(item) = app_menu.itemAtIndex(0) else {
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
    install_web_menu(mtm, &app_menu, bridge);
}

struct WebTargetIvars {
    bridge: Arc<WebMenuBridge>,
    toggle: Retained<NSMenuItem>,
    address: Retained<NSMenuItem>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. AppKit references and
    // callbacks are main-thread-only, and WebTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RunkiWebTarget"]
    #[ivars = WebTargetIvars]
    struct WebTarget;

    impl WebTarget {
        #[unsafe(method(toggleWeb:))]
        fn toggle_web(&self, _sender: Option<&AnyObject>) {
            self.ivars().bridge.request_toggle();
        }

        #[unsafe(method(copyWebURL:))]
        fn copy_web_url(&self, _sender: Option<&AnyObject>) {
            let status = self.ivars().bridge.status();
            if let Some(url) = status.lan_url.filter(|_| status.on) {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                // SAFETY: NSPasteboardTypeString is AppKit's string type.
                let _ = unsafe { pasteboard.setString_forType(&NSString::from_str(&url), NSPasteboardTypeString) };
            }
        }
    }

    // SAFETY: NSObjectProtocol has no additional safety requirements.
    unsafe impl NSObjectProtocol for WebTarget {}

    // SAFETY: This delegate only refreshes its own retained menu items, on the
    // main thread. No server operations run during AppKit menu tracking.
    unsafe impl NSMenuDelegate for WebTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            self.refresh();
        }
    }

    // SAFETY: Validation only reads the Rust status and returns a bool. Keep
    // AppKit's normal automatic validation for the other app menu commands.
    unsafe impl NSMenuItemValidation for WebTarget {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            if std::ptr::eq(item, &*self.ivars().address) {
                let status = self.ivars().bridge.status();
                status.on && status.lan_url.is_some()
            } else {
                true
            }
        }
    }
);

impl WebTarget {
    fn new(mtm: MainThreadMarker, ivars: WebTargetIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's plain init, after initializing the Rust ivars.
        unsafe { msg_send![super(this), init] }
    }

    fn refresh(&self) {
        let ivars = self.ivars();
        let status = ivars.bridge.status();
        ivars.toggle.setState(if status.on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        let url = status.lan_url.filter(|_| status.on);
        ivars.address.setTitle(&NSString::from_str(
            url.as_deref().unwrap_or("Web Server Off"),
        ));
        ivars.address.setEnabled(url.is_some());
    }
}

fn install_web_menu(mtm: MainThreadMarker, menu: &NSMenu, bridge: Arc<WebMenuBridge>) {
    // winit places a separator immediately after About. Preserve any existing
    // delegate instead of taking over a menu owned by another integration.
    if menu.numberOfItems() < 2
        || !menu
            .itemAtIndex(1)
            .is_some_and(|item| item.isSeparatorItem())
        || menu.delegate().is_some()
    {
        return;
    }
    // SAFETY: Both selectors are implemented by WebTarget below; no shortcuts
    // are assigned. Targets are installed before these items enter the menu.
    let (toggle, address) = unsafe {
        (
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Web Server"),
                Some(sel!(toggleWeb:)),
                ns_string!(""),
            ),
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Web Server Off"),
                Some(sel!(copyWebURL:)),
                ns_string!(""),
            ),
        )
    };
    address.setToolTip(Some(ns_string!("Copy the LAN web address")));
    let target = WebTarget::new(
        mtm,
        WebTargetIvars {
            bridge,
            toggle,
            address,
        },
    );
    // SAFETY: WebTarget implements both action selectors and lives until exit.
    unsafe {
        target.ivars().toggle.setTarget(Some(&target));
        target.ivars().address.setTarget(Some(&target));
    }
    target.refresh();
    menu.insertItem_atIndex(&target.ivars().toggle, 2);
    menu.insertItem_atIndex(&target.ivars().address, 3);
    menu.insertItem_atIndex(&NSMenuItem::separatorItem(mtm), 4);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    // NSMenu's delegate and NSMenuItem targets are weak references.
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
