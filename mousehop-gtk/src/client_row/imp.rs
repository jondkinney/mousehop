use std::cell::RefCell;

use adw::subclass::prelude::*;
use adw::{ActionRow, ComboRow, prelude::*};
use glib::{Binding, subclass::InitializingObject};
use gtk::glib::subclass::Signal;
use gtk::glib::{SignalHandlerId, clone};
use gtk::{Button, CompositeTemplate, Entry, Switch, glib};
use mousehop_ipc::{ConnectionMode, Position};
use std::sync::OnceLock;

use crate::client_object::{AddrEntry, ClientObject, LatencyState, format_latency, iface_label};

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/mousehop/Mousehop/client_row.ui")]
pub struct ClientRow {
    #[template_child]
    pub enable_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub clipboard_send_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub dns_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub hostname: TemplateChild<gtk::Entry>,
    #[template_child]
    pub port: TemplateChild<gtk::Entry>,
    #[template_child]
    pub position: TemplateChild<ComboRow>,
    #[template_child]
    pub address_select: TemplateChild<ComboRow>,
    #[template_child]
    pub delete_row: TemplateChild<ActionRow>,
    #[template_child]
    pub delete_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub dns_loading_indicator: TemplateChild<gtk::Spinner>,
    pub bindings: RefCell<Vec<Binding>>,
    pub title_handlers: RefCell<Vec<SignalHandlerId>>,
    hostname_change_handler: RefCell<Option<SignalHandlerId>>,
    port_change_handler: RefCell<Option<SignalHandlerId>>,
    position_change_handler: RefCell<Option<SignalHandlerId>>,
    set_state_handler: RefCell<Option<SignalHandlerId>>,
    pub clipboard_send_handler: RefCell<Option<SignalHandlerId>>,
    address_select_handler: RefCell<Option<SignalHandlerId>>,
    /// Maps a non-"Auto" dropdown index (i.e. `selected - 1`) to the
    /// candidate IP string it represents.
    address_select_ips: RefCell<Vec<String>>,
    /// Signature of the last-rendered dropdown (labels + selection).
    /// Lets [`Self::set_addresses`] skip a model rebuild when nothing
    /// visible changed, so a 5-second latency refresh never snaps an
    /// open dropdown shut.
    address_signature: RefCell<Option<String>>,
    pub client_object: RefCell<Option<ClientObject>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ClientRow {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "ClientRow";
    const ABSTRACT: bool = false;

    type Type = super::ClientRow;
    type ParentType = adw::ExpanderRow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for ClientRow {
    fn constructed(&self) {
        self.parent_constructed();
        self.delete_button.connect_clicked(clone!(
            #[weak(rename_to = row)]
            self,
            move |button| {
                row.handle_client_delete(button);
            }
        ));
        let handler = self.hostname.connect_changed(clone!(
            #[weak(rename_to = row)]
            self,
            move |entry| {
                row.handle_hostname_changed(entry);
            }
        ));
        self.hostname_change_handler.replace(Some(handler));
        let handler = self.port.connect_changed(clone!(
            #[weak(rename_to = row)]
            self,
            move |entry| {
                row.handle_port_changed(entry);
            }
        ));
        self.port_change_handler.replace(Some(handler));
        let handler = self.position.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |position| {
                row.handle_position_changed(position);
            }
        ));
        self.position_change_handler.replace(Some(handler));
        let handler = self.enable_switch.connect_state_set(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |switch, state| {
                row.handle_activate_switch(state, switch);
                glib::Propagation::Proceed
            }
        ));
        self.set_state_handler.replace(Some(handler));
        let handler = self.clipboard_send_switch.connect_state_set(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, state| {
                row.obj()
                    .emit_by_name::<()>("request-clipboard-send-change", &[&state]);
                glib::Propagation::Proceed
            }
        ));
        self.clipboard_send_handler.replace(Some(handler));
        let handler = self.address_select.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |combo| {
                // Index 0 = "Auto", 1 = "Fastest", and every later index
                // maps to `address_select_ips[index - 2]`. Emit a single
                // string the window turns into the right request:
                // "auto" / "fastest" / "<ip>".
                let idx = combo.selected();
                let choice = match idx {
                    0 => "auto".to_string(),
                    1 => "fastest".to_string(),
                    _ => row
                        .address_select_ips
                        .borrow()
                        .get(idx as usize - 2)
                        .cloned()
                        .unwrap_or_else(|| "auto".to_string()),
                };
                row.obj()
                    .emit_by_name::<()>("request-connection-choice", &[&choice]);
            }
        ));
        self.address_select_handler.replace(Some(handler));
        // The default AdwComboRow popup factory ellipsizes long
        // labels, which truncates IPv6 candidate addresses (and the
        // latency that follows them). Give the popup a factory whose
        // label never ellipsizes so the full "<ip> — <latency>" is
        // always readable; the popup widens to fit.
        self.address_select
            .set_list_factory(Some(&non_ellipsizing_factory()));
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("request-activate")
                    .param_types([bool::static_type()])
                    .build(),
                Signal::builder("request-delete").build(),
                Signal::builder("request-dns").build(),
                Signal::builder("request-hostname-change")
                    .param_types([String::static_type()])
                    .build(),
                Signal::builder("request-port-change")
                    .param_types([u32::static_type()])
                    .build(),
                Signal::builder("request-position-change")
                    .param_types([u32::static_type()])
                    .build(),
                Signal::builder("request-clipboard-send-change")
                    .param_types([bool::static_type()])
                    .build(),
                // Carries the connection choice: "auto", "fastest", or
                // a specific candidate IP (= lock on the current net).
                Signal::builder("request-connection-choice")
                    .param_types([String::static_type()])
                    .build(),
            ]
        })
    }
}

#[gtk::template_callbacks]
impl ClientRow {
    #[template_callback]
    fn handle_activate_switch(&self, state: bool, _switch: &Switch) -> bool {
        self.obj().emit_by_name::<()>("request-activate", &[&state]);
        true // dont run default handler
    }

    #[template_callback]
    fn handle_request_dns(&self, _: &Button) {
        self.obj().emit_by_name::<()>("request-dns", &[]);
    }

    #[template_callback]
    fn handle_client_delete(&self, _button: &Button) {
        self.obj().emit_by_name::<()>("request-delete", &[]);
    }

    fn handle_port_changed(&self, port_entry: &Entry) {
        if let Ok(port) = port_entry.text().parse::<u16>() {
            self.obj()
                .emit_by_name::<()>("request-port-change", &[&(port as u32)]);
        }
    }

    fn handle_hostname_changed(&self, hostname_entry: &Entry) {
        self.obj()
            .emit_by_name::<()>("request-hostname-change", &[&hostname_entry.text()]);
    }

    fn handle_position_changed(&self, position: &ComboRow) {
        self.obj()
            .emit_by_name("request-position-change", &[&position.selected()])
    }

    pub(super) fn set_hostname(&self, hostname: Option<String>) {
        let position = self.hostname.position();
        let handler = self.hostname_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.hostname.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_property("hostname", hostname);
        self.hostname.unblock_signal(handler);
        self.hostname.set_position(position);
    }

    pub(super) fn set_port(&self, port: u16) {
        let position = self.port.position();
        let handler = self.port_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.port.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_port(port as u32);
        self.port.unblock_signal(handler);
        self.port.set_position(position);
    }

    pub(super) fn set_pos(&self, pos: Position) {
        let handler = self.position_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.position.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_position(pos.to_string());
        self.position.unblock_signal(handler);
    }

    pub(super) fn set_active(&self, active: bool) {
        let handler = self.set_state_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.enable_switch.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_active(active);
        self.enable_switch.unblock_signal(handler);
    }

    pub(super) fn set_dns_state(&self, resolved: bool) {
        if resolved {
            self.dns_button.set_css_classes(&["success"])
        } else {
            self.dns_button.set_css_classes(&["warning"])
        }
    }

    /// Push a server-originated `clipboard-send` value into the
    /// switch without retriggering the user-change signal — same
    /// block/unblock pattern as `set_active` for the activate
    /// switch.
    pub(super) fn set_clipboard_send(&self, value: bool) {
        let handler = self.clipboard_send_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.clipboard_send_switch.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_clipboard_send(value);
        self.clipboard_send_switch.unblock_signal(handler);
    }

    /// Rebuild the connection-address dropdown from the candidate list,
    /// base mode and current-network lock. Entries are: `Auto`,
    /// `Fastest`, then `<ip> — [<kind> · ]<latency>` per candidate (the
    /// active address bulleted). A lock that has dropped out of the
    /// candidate set is appended so the user can still see/release it.
    /// The selection is set without firing the user signal, and the
    /// rebuild is skipped when nothing visible changed (see
    /// `address_signature`) so periodic latency refreshes don't disturb
    /// an open dropdown.
    pub(super) fn set_addresses(
        &self,
        entries: Vec<AddrEntry>,
        mode: ConnectionMode,
        locked: Option<String>,
    ) {
        // Fixed leading options. Keep these indices in sync with the
        // selection handler in `constructed`.
        let mut labels: Vec<String> = vec!["Auto".to_string(), "Fastest".to_string()];
        let lead = labels.len();
        let mut ips: Vec<String> = Vec::with_capacity(entries.len());
        for e in &entries {
            let marker = if e.active { "● " } else { "" };
            // Build the trailing detail cleanly so we never render a
            // dangling separator (e.g. "<ip> — —"): with an interface
            // kind it's "Wired · <lat>"; without a kind we only append a
            // real measured latency; otherwise just show the address.
            let detail = match iface_label(e.kind) {
                Some(kind) => format!("{kind} · {}", format_latency(&e.latency)),
                None => match e.latency {
                    LatencyState::Rtt(_) => format_latency(&e.latency),
                    _ => String::new(),
                },
            };
            if detail.is_empty() {
                labels.push(format!("{marker}{}", e.ip));
            } else {
                labels.push(format!("{marker}{} — {detail}", e.ip));
            }
            ips.push(e.ip.clone());
        }
        // Keep a locked-but-no-longer-a-candidate address visible.
        if let Some(ip) = locked.as_ref() {
            if !ips.iter().any(|x| x == ip) {
                labels.push(format!("{ip} — locked"));
                ips.push(ip.clone());
            }
        }
        let selected = match locked.as_ref() {
            // A pinned address takes precedence in the display.
            Some(ip) => ips
                .iter()
                .position(|x| x == ip)
                .map(|i| (i + lead) as u32)
                .unwrap_or(0),
            None => match mode {
                ConnectionMode::Auto => 0,
                ConnectionMode::Fastest => 1,
            },
        };

        let signature = format!("{selected}\u{1f}{}", labels.join("\u{1e}"));
        if self.address_signature.borrow().as_deref() == Some(signature.as_str()) {
            return;
        }
        self.address_signature.replace(Some(signature));
        self.address_select_ips.replace(ips);

        let handler = self.address_select_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.address_select.block_signal(handler);
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let model = gtk::StringList::new(&label_refs);
        self.address_select.set_model(Some(&model));
        self.address_select.set_selected(selected);
        self.address_select.unblock_signal(handler);
    }
}

/// A list-item factory whose label never ellipsizes — used for the
/// address-selector popup so long (IPv6) addresses and their latency
/// stay fully visible instead of being cut off with "…".
fn non_ellipsizing_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::None);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string())
            .unwrap_or_default();
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.set_text(&text);
        }
    });
    factory
}

impl WidgetImpl for ClientRow {}
impl BoxImpl for ClientRow {}
impl ListBoxRowImpl for ClientRow {}
impl PreferencesRowImpl for ClientRow {}
impl ExpanderRowImpl for ClientRow {}
