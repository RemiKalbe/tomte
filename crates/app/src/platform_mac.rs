//! AppKit integration: status item + menu + activation policy (spec §3.2).
//! Compile-verified as a spike against gpui 0.2.2 + objc2-app-kit 0.3.2.

use std::sync::mpsc::{Receiver, Sender, channel};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    OpenApp,
    Review,
    SyncAll,
    Settings,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuSpec {
    pub header: String,
    pub freshness: String,
    pub review_label: Option<String>,
    pub sync_all_enabled: bool,
}

struct TargetIvars {
    tx: Sender<MenuCommand>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TomteMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openApp:))]
        fn open_app(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::OpenApp);
        }

        #[unsafe(method(review:))]
        fn review(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Review);
        }

        #[unsafe(method(syncAll:))]
        fn sync_all(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::SyncAll);
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Settings);
        }

        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Quit);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, tx: Sender<MenuCommand>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { tx });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct StatusItem {
    item: Retained<NSStatusItem>,
    target: Retained<MenuTarget>,
}

pub fn set_accessory_policy(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

impl StatusItem {
    pub fn install(mtm: MainThreadMarker) -> (Self, Receiver<MenuCommand>) {
        let (tx, rx) = channel();
        let target = MenuTarget::new(mtm, tx);
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let this = Self { item, target };
        this.set_title(mtm, "tomte");
        (this, rx)
    }

    pub fn set_title(&self, mtm: MainThreadMarker, title: &str) {
        if let Some(button) = self.item.button(mtm) {
            button.setTitle(&NSString::from_str(title));
        }
    }

    pub fn set_menu(&self, mtm: MainThreadMarker, spec: &MenuSpec) {
        let menu = NSMenu::new(mtm);
        unsafe {
            let add_info = |text: &str| {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str(text));
                it.setEnabled(false);
                menu.addItem(&it);
            };
            add_info(&spec.header);
            add_info(&spec.freshness);
            menu.addItem(&NSMenuItem::separatorItem(mtm));

            if let Some(label) = &spec.review_label {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str(label));
                it.setTarget(Some(&self.target));
                it.setAction(Some(sel!(review:)));
                menu.addItem(&it);
            }
            let sync = NSMenuItem::new(mtm);
            sync.setTitle(&NSString::from_str("Sync all"));
            if spec.sync_all_enabled {
                sync.setTarget(Some(&self.target));
                sync.setAction(Some(sel!(syncAll:)));
            } else {
                sync.setEnabled(false);
            }
            menu.addItem(&sync);
            menu.addItem(&NSMenuItem::separatorItem(mtm));

            let open = NSMenuItem::new(mtm);
            open.setTitle(&NSString::from_str("Open chezmoi UI"));
            open.setTarget(Some(&self.target));
            open.setAction(Some(sel!(openApp:)));
            menu.addItem(&open);

            let settings = NSMenuItem::new(mtm);
            settings.setTitle(&NSString::from_str("Settings…"));
            settings.setTarget(Some(&self.target));
            settings.setAction(Some(sel!(openSettings:)));
            menu.addItem(&settings);

            let quit = NSMenuItem::new(mtm);
            quit.setTitle(&NSString::from_str("Quit chezmoi UI"));
            quit.setTarget(Some(&self.target));
            quit.setAction(Some(sel!(quitApp:)));
            menu.addItem(&quit);
        }
        self.item.setMenu(Some(&menu));
    }
}
