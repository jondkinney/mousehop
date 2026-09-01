use crate::error::EmulationError;

use super::{Emulation, error::WlrootsEmulationCreationError};
use async_trait::async_trait;
use bitflags::bitflags;
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use wayland_client::backend::WaylandError;
use wayland_client::{Proxy, WEnum};

use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as VkManager,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as Vk,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use input_event::{Event, KeyboardEvent, PointerEvent, display::DisplayLayout, scancode};

use super::EmulationHandle;
use super::error::WaylandBindError;

struct State {
    keymap: Option<(u32, OwnedFd, u32)>,
    /// Temporary keyboard used only to receive the compositor's
    /// keymap. The wlroots backend does not otherwise drive this
    /// Wayland event queue, so leaving the object alive would let
    /// later keymap FDs accumulate unread in the compositor.
    keymap_keyboard: Option<WlKeyboard>,
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    seat: wl_seat::WlSeat,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    vkm: VkManager,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    /// All wl_outputs the compositor advertises, keyed by registry global
    /// name. Registry events add/remove entries after startup; output and
    /// xdg-output events keep each entry's geometry fresh.
    outputs: HashMap<u32, Output>,
    /// Registry advertisement order is the compositor's stable monitor order.
    /// Preserve it explicitly: Wayland global names are opaque and HashMap
    /// iteration is randomized, while Hyprland uses monitor order to resolve
    /// exact equal-distance gap clamps.
    output_order: Vec<u32>,
    /// Dedicated virtual pointer used only for absolute-position
    /// warps on `Enter`. Separate from per-handle pointers so warp
    /// works regardless of which client is active.
    warp_pointer: Vp,
}

struct Output {
    /// Keep both proxies alive so geometry events continue flowing.
    _output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    info: OutputInfo,
    pending: PendingOutputInfo,
}

/// One not-yet-committed Wayland output generation. Both wl_output and
/// xdg-output deliver the fields of a rectangle as separate events; exposing
/// them before the matching `done` event can pair a new position with an old
/// size (or the reverse) during a hotplug/scale update.
#[derive(Default, Clone, Copy)]
struct PendingOutputInfo {
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

#[derive(Default, Clone, Copy)]
struct OutputInfo {
    /// Whether this output has an xdg-output companion. While its logical
    /// rectangle is incomplete, omit the output instead of falling back to
    /// raw pixels and mixing coordinate spaces with already-logical outputs.
    has_xdg_output: bool,
    /// Position in the compositor's global coordinate space, from
    /// wl_output::Event::Geometry. Raw-pixel coordinates.
    x: i32,
    y: i32,
    /// Pixel dimensions of the active mode, from wl_output::Event::Mode.
    width: i32,
    height: i32,
    /// Logical position in the compositor's coordinate space, from
    /// zxdg_output_v1::Event::LogicalPosition. Reflects software
    /// scaling (e.g. fractional or HiDPI). Falls back to (x, y) when
    /// xdg-output isn't available.
    logical_x: Option<i32>,
    logical_y: Option<i32>,
    /// Logical dimensions, from zxdg_output_v1::Event::LogicalSize.
    /// This is the coordinate space the compositor uses for cursor
    /// positions and the same one the capture side uses, so we
    /// prefer it for `display_bounds()` to keep both sides in sync.
    /// Falls back to (width, height) when xdg-output isn't available.
    logical_width: Option<i32>,
    logical_height: Option<i32>,
}

impl OutputInfo {
    /// Return one internally-consistent rectangle. xdg-output sends
    /// logical position and size as separate events, so using a
    /// partially-updated logical rectangle with raw mode dimensions
    /// would briefly mix coordinate spaces during a monitor change.
    fn rectangle(self) -> Option<(i32, i32, i32, i32)> {
        match (
            self.logical_x,
            self.logical_y,
            self.logical_width,
            self.logical_height,
        ) {
            (Some(x), Some(y), Some(width), Some(height)) => Some((x, y, width, height)),
            _ if self.has_xdg_output => None,
            _ => Some((self.x, self.y, self.width, self.height)),
        }
    }

    fn commit_raw(&mut self, pending: &mut PendingOutputInfo) {
        if let Some((x, y)) = pending.position.take() {
            self.x = x;
            self.y = y;
        }
        if let Some((width, height)) = pending.size.take() {
            self.width = width;
            self.height = height;
        }
    }

    fn commit_logical(&mut self, pending: &mut PendingOutputInfo) {
        if let Some((x, y)) = pending.logical_position.take() {
            self.logical_x = Some(x);
            self.logical_y = Some(y);
        }
        if let Some((width, height)) = pending.logical_size.take() {
            self.logical_width = Some(width);
            self.logical_height = Some(height);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DisplayGeometry {
    origin: (i32, i32),
    size: (u32, u32),
}

/// Compute the bounding rectangle of every usable output in a single
/// compositor coordinate space. Retaining the origin alongside the
/// size is important for layouts with a monitor left of or above the
/// primary; even though virtual-pointer absolute coordinates are
/// union-relative, the origin is part of the geometry invariant.
fn display_geometry(outputs: impl Iterator<Item = OutputInfo>) -> Option<DisplayGeometry> {
    let mut xmin = i64::MAX;
    let mut ymin = i64::MAX;
    let mut xmax = i64::MIN;
    let mut ymax = i64::MIN;
    let mut any = false;

    for output in outputs {
        let Some((x, y, width, height)) = output.rectangle() else {
            continue;
        };
        if width <= 0 || height <= 0 {
            continue;
        }
        let x = i64::from(x);
        let y = i64::from(y);
        xmin = xmin.min(x);
        ymin = ymin.min(y);
        xmax = xmax.max(x + i64::from(width));
        ymax = ymax.max(y + i64::from(height));
        any = true;
    }

    if !any || xmax <= xmin || ymax <= ymin {
        return None;
    }

    Some(DisplayGeometry {
        origin: (i32::try_from(xmin).ok()?, i32::try_from(ymin).ok()?),
        size: (
            u32::try_from(xmax - xmin).ok()?,
            u32::try_from(ymax - ymin).ok()?,
        ),
    })
}

// App State, implements Dispatch event handlers
pub(crate) struct WlrootsEmulation {
    last_flush_failed: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) fn new() -> Result<Self, WlrootsEmulationCreationError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;
        let vkm: VkManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "virtual-keyboard-unstable-v1"))?;
        // xdg-output gives us LogicalSize/LogicalPosition — the
        // coordinate space the compositor actually uses (with
        // software/fractional scaling applied). The capture side
        // already reports bounds in this space, so emulation needs
        // it too or warps land on different proportions than the
        // sender computed. Optional: if the compositor doesn't
        // advertise xdg_output_manager we fall back to wl_output's
        // raw mode dimensions.
        let xdg_output_manager: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();

        // Bind every advertised wl_output so we receive Geometry +
        // Mode events for each one. Used to compute display_bounds.
        let mut outputs = HashMap::new();
        let mut output_order = Vec::new();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                // version 2 is enough for Geometry + Mode events.
                let output: WlOutput =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(2), &qh, global.name);
                let xdg_output = xdg_output_manager
                    .as_ref()
                    .map(|mgr| mgr.get_xdg_output(&output, &qh, global.name));
                output_order.push(global.name);
                outputs.insert(
                    global.name,
                    Output {
                        _output: output,
                        info: OutputInfo {
                            has_xdg_output: xdg_output.is_some(),
                            ..Default::default()
                        },
                        pending: PendingOutputInfo::default(),
                        xdg_output,
                    },
                );
            }
        }

        // Dedicated warp pointer — used only for motion_absolute on
        // Enter, so warp works even when no per-handle virtual
        // pointer is currently active.
        let warp_pointer: Vp = vpm.create_virtual_pointer(None, &qh, ());

        let input_for_client: HashMap<EmulationHandle, VirtualInput> = HashMap::new();

        let mut emulate = WlrootsEmulation {
            last_flush_failed: false,
            state: State {
                keymap: None,
                keymap_keyboard: None,
                input_for_client,
                seat,
                vpm,
                vkm,
                xdg_output_manager,
                qh,
                outputs,
                output_order,
                warp_pointer,
            },
            queue,
        };
        while emulate.state.keymap.is_none() {
            emulate.queue.blocking_dispatch(&mut emulate.state)?;
        }

        // We only need wl_keyboard long enough to obtain a keymap for
        // the virtual keyboards. Release it and wait for the server to
        // process the destructor so future keymap changes cannot queue
        // file descriptors on an event queue this backend does not
        // continuously dispatch.
        if let Some(keyboard) = emulate.state.keymap_keyboard.take() {
            keyboard.release();
            emulate.queue.roundtrip(&mut emulate.state)?;
        }
        // let fd = unsafe { &File::from_raw_fd(emulate.state.keymap.unwrap().1.as_raw_fd()) };
        // let mmap = unsafe { MmapOptions::new().map_copy(fd).unwrap() };
        // log::debug!("{:?}", &mmap[..100]);
        Ok(emulate)
    }

    /// Drain every Wayland event currently available without blocking.
    ///
    /// The emulation backend is request-driven rather than a Stream, so
    /// no external event loop reads this queue for us. In particular,
    /// the service's periodic `display_bounds()` poll used to inspect
    /// the startup snapshot forever: monitor hotplug, resolution, scale,
    /// and position events sat unread on the Wayland socket. Reading here
    /// makes that existing poll the geometry event pump as well.
    fn dispatch_available(&mut self) {
        loop {
            if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
                log::warn!("failed to dispatch pending wlroots display events: {error}");
                return;
            }

            // Registry dispatch can bind a newly-added wl_output and
            // request its xdg-output companion. Send those requests
            // before looking for the geometry events they trigger.
            match self.queue.flush() {
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    log::warn!("failed to flush wlroots display requests: {error}");
                    return;
                }
                Ok(()) => {}
            }

            let Some(read_guard) = self.queue.prepare_read() else {
                // Events raced into the queue after dispatch_pending;
                // loop once more and dispatch them before preparing a
                // socket read.
                continue;
            };
            match read_guard.read() {
                Ok(0) => {
                    // Another reader consumed the available events.
                    // There is normally no other reader for this
                    // connection, but avoid spinning if that changes.
                    break;
                }
                Ok(_) => {
                    // The read only queues events; dispatch them before
                    // attempting another non-blocking read.
                }
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(error) => {
                    log::warn!("failed to read wlroots display events: {error}");
                    break;
                }
            }
        }
    }

    fn queue_absolute_warp(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<(), EmulationError> {
        let now: u32 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let max_x = i32::try_from(width.saturating_sub(1)).unwrap_or(i32::MAX);
        let max_y = i32::try_from(height.saturating_sub(1)).unwrap_or(i32::MAX);
        let cx = x.clamp(0, max_x) as u32;
        let cy = y.clamp(0, max_y) as u32;
        self.state
            .warp_pointer
            .motion_absolute(now, cx, cy, width, height);
        self.state.warp_pointer.frame();
        // A full Wayland socket still retains this warp in the client buffer;
        // later input on the same connection cannot overtake it. Remember the
        // backpressure so consume() pre-flushes (and drops input while still
        // blocked), but only report fatal transport/protocol failures to the
        // atomic handover transaction.
        match self.queue.flush() {
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                self.last_flush_failed = true;
                log::debug!("edge warp queued behind Wayland socket backpressure");
            }
            Err(error) => return Err(error.into()),
            Ok(()) => self.last_flush_failed = false,
        }
        Ok(())
    }
}

impl State {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        if self.outputs.contains_key(&name) {
            return;
        }
        let output: WlOutput = registry.bind(name, version.min(2), qh, name);
        let xdg_output = self
            .xdg_output_manager
            .as_ref()
            .map(|manager| manager.get_xdg_output(&output, qh, name));
        self.output_order.push(name);
        self.outputs.insert(
            name,
            Output {
                _output: output,
                info: OutputInfo {
                    has_xdg_output: xdg_output.is_some(),
                    ..Default::default()
                },
                pending: PendingOutputInfo::default(),
                xdg_output,
            },
        );
        log::debug!("wlroots display added: registry global {name}");
    }

    fn remove_output(&mut self, name: u32) {
        if let Some(output) = self.outputs.remove(&name) {
            self.output_order.retain(|candidate| *candidate != name);
            if let Some(xdg_output) = output.xdg_output {
                xdg_output.destroy();
            }
            log::debug!("wlroots display removed: registry global {name}");
        }
    }

    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());
        let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());

        // TODO: use server side keymap
        if let Some((format, fd, size)) = self.keymap.as_ref() {
            keyboard.keymap(*format, fd.as_fd(), *size);
        } else {
            panic!("no keymap");
        }

        let vinput = VirtualInput {
            pointer,
            keyboard,
            modifiers: Arc::new(Mutex::new(ModifierState::default())),
        };

        self.input_for_client.insert(client, vinput);
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
            input.keyboard.destroy();
        }
    }

    /// Bounding rectangle of every active wl_output in the
    /// compositor's logical coordinate space (with software /
    /// fractional scaling applied). Falls back per-output to raw
    /// mode geometry when xdg-output is unavailable.
    fn display_geometry(&self) -> Option<DisplayGeometry> {
        display_geometry(
            self.output_order
                .iter()
                .filter_map(|name| self.outputs.get(name))
                .map(|output| output.info),
        )
    }

    fn display_layout(&self) -> Option<DisplayLayout> {
        let layout = DisplayLayout::new(
            self.output_order
                .iter()
                .filter_map(|name| self.outputs.get(name))
                .filter_map(|output| {
                    let (x, y, width, height) = output.info.rectangle()?;
                    Some((
                        x,
                        y,
                        u32::try_from(width).ok()?,
                        u32::try_from(height).ok()?,
                    ))
                }),
        );
        (!layout.is_empty()).then_some(layout)
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        /*
                         * outgoing buffer is full - sending more events
                         * will overwhelm the output buffer and leave the
                         * wayland connection in a broken state
                         */
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            let event_debug = format!("{event:?}");
            virtual_input
                .consume_event(event)
                .unwrap_or_else(|_| panic!("failed to convert event: {event_debug}"));
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event_debug}");
                }
                Err(WaylandError::Protocol(e)) => panic!("wayland protocol violation: {e}"),
                Ok(()) => self.last_flush_failed = false,
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn terminate(&mut self) {
        self.state.warp_pointer.destroy();
    }

    fn display_bounds(&mut self) -> Option<(u32, u32)> {
        self.dispatch_available();
        self.state.display_geometry().map(|geometry| geometry.size)
    }

    fn display_layout(&mut self) -> Option<DisplayLayout> {
        self.dispatch_available();
        self.state.display_layout()
    }

    fn supports_edge_warp(&self) -> bool {
        true
    }

    async fn warp_cursor_in_layout(
        &mut self,
        x: i32,
        y: i32,
        layout: &DisplayLayout,
    ) -> Result<(), EmulationError> {
        let (width, height) = layout
            .size()
            .ok_or(EmulationError::DisplayTopologyUnavailable)?;
        self.queue_absolute_warp(x, y, width, height)
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        self.dispatch_available();
        let Some(geometry) = self.state.display_geometry() else {
            return Ok(());
        };
        let (width, height) = geometry.size;
        self.queue_absolute_warp(x, y, width, height)
    }
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
    modifiers: Arc<Mutex<ModifierState>>,
}

impl VirtualInput {
    fn consume_event(&self, event: Event) -> Result<(), ()> {
        let now: u32 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::Motion { time, dx, dy } => self.pointer.motion(time, dx, dy),
                    PointerEvent::Button {
                        time,
                        button,
                        state,
                    } => {
                        let state: ButtonState = state.try_into()?;
                        self.pointer.button(time, button, state);
                    }
                    PointerEvent::Axis { axis, value, .. } => {
                        // wl_pointer requires `axis_source` to be sent
                        // alongside the axis event; without it many
                        // compositors (Hyprland, Sway, …) silently
                        // drop continuous scroll. AxisSource::Finger
                        // matches a Mac trackpad gesture, which is the
                        // typical source for continuous scroll
                        // local `now` timestamp because the upstream
                        // CGEventTap path passes time=0 and some
                        // compositors filter zero-time events.
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer.axis(now, axis, value);
                        self.pointer.axis_source(AxisSource::Finger);
                        self.pointer.frame();
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer
                            .axis_discrete(now, axis, value as f64 / 8., value / 120);
                        self.pointer.axis_source(AxisSource::Wheel);
                        self.pointer.frame();
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { time, key, state } => {
                    self.keyboard.key(time, key, state as u32);
                    if let Ok(mut mods) = self.modifiers.lock() {
                        if mods.update_by_key_event(key, state) {
                            log::trace!("Key triggers modifier change: {mods:?}");
                            self.keyboard.modifiers(
                                mods.mask_pressed().bits(),
                                0,
                                mods.mask_locks().bits(),
                                0,
                            );
                        }
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                } => {
                    // Synchronize internal modifier state, assuming server is authoritative
                    if let Ok(mut mods) = self.modifiers.lock() {
                        mods.update_by_mods_event(e);
                    }
                    self.keyboard
                        .modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            },
            Event::Clipboard(_) => {
                // Clipboard injection is handled by the cross-
                // platform `ClipboardEmulation` sink, not wlroots.
            }
        }
        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: Vk);
delegate_noop!(State: VpManager);
delegate_noop!(State: VkManager);
delegate_noop!(State: ZxdgOutputManagerV1);

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        xdg_output: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending.logical_position = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending.logical_size = Some((width, height));
            }
            zxdg_output_v1::Event::Done if xdg_output.version() < 3 => {
                output.info.commit_logical(&mut output.pending);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut State,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => state.add_output(registry, name, version, qh),
            wl_registry::Event::GlobalRemove { name } => state.remove_output(name),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                output.pending.position = Some((x, y));
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                output.pending.size = Some((width, height));
            }
            wl_output::Event::Done => {
                output.info.commit_raw(&mut output.pending);
                // xdg-output v3 removed its own Done event and commits its
                // logical fields together with wl_output.done. Older versions
                // keep using zxdg_output_v1.done above.
                if output
                    .xdg_output
                    .as_ref()
                    .is_some_and(|xdg_output| xdg_output.version() >= 3)
                {
                    output.info.commit_logical(&mut output.pending);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard)
                && state.keymap.is_none()
                && state.keymap_keyboard.is_none()
            {
                state.keymap_keyboard = Some(seat.get_keyboard(qhandle, ()));
            }
        }
    }
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

#[derive(Debug, Default)]
struct ModifierState {
    masks: XMods,
    pressed_keys: HashSet<scancode::Linux>,
}

impl ModifierState {
    fn pressed_mask_for_key(key: scancode::Linux) -> XMods {
        match key {
            scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
            scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
            scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => XMods::Mod1Mask,
            scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
            _ => XMods::empty(),
        }
    }

    fn locked_mask_for_key(key: scancode::Linux) -> XMods {
        match key {
            scancode::Linux::KeyCapsLock => XMods::LockMask,
            scancode::Linux::KeyNumlock => XMods::Mod2Mask,
            scancode::Linux::KeyScrollLock => XMods::Mod3Mask,
            _ => XMods::empty(),
        }
    }

    fn update_by_mods_event(&mut self, evt: KeyboardEvent) {
        if let KeyboardEvent::Modifiers {
            depressed, locked, ..
        } = evt
        {
            let snapshot = XMods::from_bits_truncate(depressed) | XMods::from_bits_truncate(locked);
            self.masks = snapshot;
            // A snapshot is authoritative for aggregate state but cannot tell
            // us which physical side is held. Preserve known sides only while
            // their aggregate bit remains set, so a later key-up cannot revive
            // modifier state that the snapshot explicitly cleared.
            self.pressed_keys.retain(|key| {
                let mask = Self::pressed_mask_for_key(*key);
                !mask.is_empty() && snapshot.contains(mask)
            });
        }
    }

    fn update_by_key_event(&mut self, key: u32, state: u8) -> bool {
        if let Ok(key) = scancode::Linux::try_from(key) {
            log::trace!("Attempting to process modifier from: {key:#?}");
            let pressed_mask = Self::pressed_mask_for_key(key);
            let locked_mask = Self::locked_mask_for_key(key);

            // unchanged
            if pressed_mask.is_empty() && locked_mask.is_empty() {
                log::trace!("{key:#?} is not a modifier key");
                return false;
            }

            if !pressed_mask.is_empty() {
                match state {
                    1 => {
                        self.pressed_keys.insert(key);
                    }
                    _ => {
                        self.pressed_keys.remove(&key);
                    }
                }
                let still_pressed = self
                    .pressed_keys
                    .iter()
                    .copied()
                    .any(|pressed| Self::pressed_mask_for_key(pressed) == pressed_mask);
                self.masks.set(pressed_mask, still_pressed);
            } else if state != 1 {
                self.masks.toggle(locked_mask);
            }
            true
        } else {
            false
        }
    }

    fn mask_locks(&self) -> XMods {
        self.masks & (XMods::LockMask | XMods::Mod2Mask | XMods::Mod3Mask)
    }

    fn mask_pressed(&self) -> XMods {
        self.masks & (XMods::ShiftMask | XMods::ControlMask | XMods::Mod1Mask | XMods::Mod4Mask)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisplayGeometry, ModifierState, OutputInfo, PendingOutputInfo, XMods, display_geometry,
    };
    use input_event::{KeyboardEvent, scancode::Linux};

    fn update_key(modifiers: &mut ModifierState, key: Linux, pressed: bool) {
        assert!(modifiers.update_by_key_event(key as u32, u8::from(pressed)));
    }

    #[test]
    fn releasing_one_modifier_side_preserves_the_other_side() {
        for (left, right, mask) in [
            (Linux::KeyLeftShift, Linux::KeyRightShift, XMods::ShiftMask),
            (Linux::KeyLeftCtrl, Linux::KeyRightCtrl, XMods::ControlMask),
            (Linux::KeyLeftAlt, Linux::KeyRightalt, XMods::Mod1Mask),
            (Linux::KeyLeftMeta, Linux::KeyRightmeta, XMods::Mod4Mask),
        ] {
            let mut modifiers = ModifierState::default();
            update_key(&mut modifiers, left, true);
            update_key(&mut modifiers, right, true);
            update_key(&mut modifiers, left, false);
            assert_eq!(modifiers.mask_pressed(), mask);

            update_key(&mut modifiers, right, false);
            assert_eq!(modifiers.mask_pressed(), XMods::empty());
        }
    }

    #[test]
    fn explicit_modifier_snapshots_reconcile_tracked_sides() {
        let mut modifiers = ModifierState::default();
        update_key(&mut modifiers, Linux::KeyLeftShift, true);
        modifiers.update_by_mods_event(KeyboardEvent::Modifiers {
            depressed: XMods::ShiftMask.bits(),
            latched: 0,
            locked: XMods::LockMask.bits(),
            group: 0,
        });
        update_key(&mut modifiers, Linux::KeyRightShift, true);
        update_key(&mut modifiers, Linux::KeyLeftShift, false);
        assert_eq!(modifiers.mask_pressed(), XMods::ShiftMask);
        assert_eq!(modifiers.mask_locks(), XMods::LockMask);

        modifiers.update_by_mods_event(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        update_key(&mut modifiers, Linux::KeyRightShift, false);
        assert_eq!(modifiers.mask_pressed(), XMods::empty());
        assert_eq!(modifiers.mask_locks(), XMods::empty());
    }

    #[test]
    fn display_geometry_preserves_negative_origin_and_scaled_layout() {
        // Mirrors the live three-output Hyprland layout that exposed
        // the stale-primary-only regression.
        let outputs = [
            OutputInfo {
                logical_x: Some(0),
                logical_y: Some(0),
                logical_width: Some(3072),
                logical_height: Some(1728),
                ..Default::default()
            },
            OutputInfo {
                logical_x: Some(-1024),
                logical_y: Some(0),
                logical_width: Some(1024),
                logical_height: Some(600),
                ..Default::default()
            },
            OutputInfo {
                logical_x: Some(836),
                logical_y: Some(1728),
                logical_width: Some(1280),
                logical_height: Some(360),
                ..Default::default()
            },
        ];

        assert_eq!(
            display_geometry(outputs.into_iter()),
            Some(DisplayGeometry {
                origin: (-1024, 0),
                size: (4096, 2088),
            })
        );
    }

    #[test]
    fn xdg_output_is_omitted_until_its_logical_rectangle_is_complete() {
        let outputs = [OutputInfo {
            has_xdg_output: true,
            x: 100,
            y: 200,
            width: 1920,
            height: 1080,
            // LogicalPosition may arrive before LogicalSize. Its raw pixel
            // fields must not be combined with other outputs' logical fields.
            logical_x: Some(-960),
            logical_y: Some(0),
            ..Default::default()
        }];

        assert_eq!(display_geometry(outputs.into_iter()), None);
    }

    #[test]
    fn incomplete_scaled_hotplug_does_not_distort_existing_logical_layout() {
        let outputs = [
            OutputInfo {
                has_xdg_output: true,
                logical_x: Some(0),
                logical_y: Some(0),
                logical_width: Some(3072),
                logical_height: Some(1728),
                ..Default::default()
            },
            OutputInfo {
                has_xdg_output: true,
                x: 6144,
                y: 0,
                width: 2560,
                height: 1440,
                logical_x: Some(3072),
                logical_y: Some(0),
                // LogicalSize for this scaled hotplug is still pending.
                ..Default::default()
            },
        ];

        assert_eq!(
            display_geometry(outputs.into_iter()),
            Some(DisplayGeometry {
                origin: (0, 0),
                size: (3072, 1728),
            })
        );
    }

    #[test]
    fn logical_update_generation_is_invisible_until_done() {
        let mut info = OutputInfo {
            has_xdg_output: true,
            logical_x: Some(0),
            logical_y: Some(0),
            logical_width: Some(1920),
            logical_height: Some(1080),
            ..Default::default()
        };
        let mut pending = PendingOutputInfo {
            logical_position: Some((-1280, 200)),
            ..Default::default()
        };

        assert_eq!(info.rectangle(), Some((0, 0, 1920, 1080)));
        pending.logical_size = Some((1280, 720));
        assert_eq!(
            info.rectangle(),
            Some((0, 0, 1920, 1080)),
            "a new position and size must not leak before their done event",
        );

        info.commit_logical(&mut pending);
        assert_eq!(info.rectangle(), Some((-1280, 200, 1280, 720)));
        assert_eq!(pending.logical_position, None);
        assert_eq!(pending.logical_size, None);
    }

    #[test]
    fn raw_update_generation_is_invisible_until_done() {
        let mut info = OutputInfo {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let mut pending = PendingOutputInfo {
            position: Some((1920, -200)),
            size: Some((2560, 1440)),
            ..Default::default()
        };

        assert_eq!(info.rectangle(), Some((0, 0, 1920, 1080)));
        info.commit_raw(&mut pending);
        assert_eq!(info.rectangle(), Some((1920, -200, 2560, 1440)));
    }

    #[test]
    fn compositor_without_xdg_output_uses_raw_geometry() {
        let outputs = [OutputInfo {
            x: 100,
            y: 200,
            width: 1920,
            height: 1080,
            ..Default::default()
        }];

        assert_eq!(
            display_geometry(outputs.into_iter()),
            Some(DisplayGeometry {
                origin: (100, 200),
                size: (1920, 1080),
            })
        );
    }

    #[test]
    fn display_geometry_ignores_outputs_without_usable_dimensions() {
        let outputs = [OutputInfo {
            x: -100,
            y: -200,
            width: 0,
            height: 1080,
            ..Default::default()
        }];

        assert_eq!(display_geometry(outputs.into_iter()), None);
    }
}
