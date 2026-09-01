use std::{cell::RefCell, net::SocketAddr};

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::subclass::InitializingObject;
use gtk::{Button, CompositeTemplate, Image, Label, glib, template_callbacks};
use mousehop_ipc::RemoteHostState;

use super::recovery_content;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/mousehop/Mousehop/lock_recovery_window.ui")]
pub struct LockRecoveryWindow {
    #[template_child]
    pub state_icon: TemplateChild<Image>,
    #[template_child]
    pub title_label: TemplateChild<Label>,
    #[template_child]
    pub status_label: TemplateChild<Label>,
    #[template_child]
    pub instructions_label: TemplateChild<Label>,
    #[template_child]
    pub assurance_label: TemplateChild<Label>,
    #[template_child]
    pub peer_label: TemplateChild<Label>,
    pub fingerprint: RefCell<String>,
}

#[glib::object_subclass]
impl ObjectSubclass for LockRecoveryWindow {
    const NAME: &'static str = "LockRecoveryWindow";
    const ABSTRACT: bool = false;

    type Type = super::LockRecoveryWindow;
    type ParentType = adw::Window;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[template_callbacks]
impl LockRecoveryWindow {
    #[template_callback]
    fn handle_dismiss(&self, _: Button) {
        self.obj().close();
    }

    pub(super) fn set_fingerprint(&self, fingerprint: &str) {
        self.fingerprint.replace(fingerprint.to_owned());
    }

    pub(super) fn set_state(&self, addr: SocketAddr, state: RemoteHostState) {
        let content = recovery_content(state);
        self.obj().set_title(Some(content.title));
        self.state_icon.set_icon_name(Some(content.icon));
        self.title_label.set_label(content.title);
        self.status_label.set_label(content.status);
        self.instructions_label.set_label(content.instructions);
        self.assurance_label.set_label(content.assurance);
        self.peer_label
            .set_label(&format!("Authenticated status from {addr}"));
    }
}

impl ObjectImpl for LockRecoveryWindow {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        crate::modal_keys::wire_close_shortcuts(&*obj);
    }
}

impl WidgetImpl for LockRecoveryWindow {}
impl WindowImpl for LockRecoveryWindow {}
impl ApplicationWindowImpl for LockRecoveryWindow {}
impl AdwWindowImpl for LockRecoveryWindow {}
