mod imp;

use std::net::SocketAddr;

use glib::Object;
use gtk::{gio, glib, subclass::prelude::ObjectSubclassIsExt};
use mousehop_ipc::RemoteHostState;

glib::wrapper! {
    pub struct LockRecoveryWindow(ObjectSubclass<imp::LockRecoveryWindow>)
    @extends adw::Window, gtk::Window, gtk::Widget,
    @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecoveryContent {
    pub title: &'static str,
    pub instructions: &'static str,
    pub assurance: &'static str,
    pub status: &'static str,
    pub icon: &'static str,
}

pub(crate) fn recovery_content(state: RemoteHostState) -> RecoveryContent {
    match state {
        RemoteHostState::Locked => RecoveryContent {
            title: "Mac is locked",
            instructions: "Mousehop paused forwarding and returned the keyboard to the Mac. Use the keyboard physically attached to the Mac: press Shift once to wake it without entering a character, then wait. If this window still says locked, type your Mac password and press Return.",
            assurance: "Nothing is entered here. Mousehop never reads, stores, or sends your password.",
            status: "Waiting for confirmed unlock…",
            icon: "dialog-warning-symbolic",
        },
        RemoteHostState::Unavailable => RecoveryContent {
            title: "Mac status unavailable",
            instructions: "Mousehop can no longer confirm the Mac’s lock state. Do not type your password while this message is shown. Switch the monitor to the Mac, use Touch ID or Apple Watch, or use your hardware input switch.",
            assurance: "Mousehop will never ask for or transmit your Mac password.",
            status: "Waiting for the Mac to reconnect…",
            icon: "network-wired-disconnected-symbolic",
        },
        RemoteHostState::Unlocked => RecoveryContent {
            title: "Mac is unlocked",
            instructions: "Move the pointer across the configured screen edge to start a new forwarding session.",
            assurance: "The previous capture was not resumed automatically.",
            status: "Confirmed unlocked",
            icon: "emblem-ok-symbolic",
        },
    }
}

impl LockRecoveryWindow {
    pub(crate) fn new(fingerprint: &str, addr: SocketAddr, state: RemoteHostState) -> Self {
        let window: Self = Object::builder().build();
        window.imp().set_fingerprint(fingerprint);
        window.imp().set_state(addr, state);
        window
    }

    pub(crate) fn fingerprint(&self) -> String {
        self.imp().fingerprint.borrow().clone()
    }

    pub(crate) fn set_state(&self, addr: SocketAddr, state: RemoteHostState) {
        self.imp().set_state(addr, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_copy_routes_password_to_the_mac_attached_keyboard_only() {
        let content = recovery_content(RemoteHostState::Locked);
        assert!(
            content
                .instructions
                .contains("physically attached to the Mac")
        );
        assert!(content.instructions.contains("press Shift once"));
        assert!(content.instructions.contains("Mac password"));
        assert!(content.assurance.contains("Nothing is entered here"));
    }

    #[test]
    fn unavailable_copy_never_invites_blind_password_entry() {
        let content = recovery_content(RemoteHostState::Unavailable);
        assert!(content.instructions.contains("Do not type your password"));
        assert!(!content.instructions.contains("type your Mac password"));
    }

    #[test]
    fn recovery_dialog_has_no_text_entry() {
        let template = include_str!("../resources/lock_recovery_window.ui");
        assert!(!template.contains("GtkEntry"));
        assert!(!template.contains("GtkText"));
    }
}
