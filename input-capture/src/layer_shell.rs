use async_trait::async_trait;
use futures_core::Stream;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fmt::{self, Display},
    io::{self, ErrorKind},
    os::fd::{AsFd, RawFd},
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::unix::AsyncFd;

use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::prelude::AsRawFd,
    sync::Arc,
};

use wayland_protocols::{
    wp::{
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region,
        wl_registry::{self, WlRegistry},
        wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use input_event::{
    CrossingModifier, Event, KeyboardEvent, PointerEvent,
    display::{DisplayEdge, DisplayLayout, EdgeSegment},
    scancode,
};

use crate::{CaptureError, CaptureEvent, normalize_cursor_in_layout};

use super::{
    Capture, Position,
    error::{LayerShellCaptureCreationError, WaylandBindError},
};

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Clone, Debug)]
struct Output {
    wl_output: WlOutput,
    xdg_output: ZxdgOutputV1,
    global: Global,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

impl Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(info) = &self.info {
            write!(
                f,
                "{} {}x{} @pos {:?} ({})",
                info.name, info.size.0, info.size.1, info.position, info.description
            )
        } else {
            write!(f, "unknown output")
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

struct State {
    active_positions: HashSet<Position>,
    /// Optional per-edge modifier preflight. An absent entry preserves the
    /// historical immediate-grab path without adding an edge check.
    crossing_modifiers: HashMap<Position, CrossingModifier>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    active_windows: Vec<Arc<Window>>,
    focused: Option<Arc<Window>>,
    /// Pointer focus reached a gated edge, but ownership has not been taken.
    /// We briefly request keyboard focus only to receive Wayland's atomic
    /// held-key snapshot, then either begin capture or leave the edge inert.
    crossing_preflight: Option<CrossingPreflight>,
    global_list: GlobalList,
    globals: Globals,
    wayland_fd: RawFd,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<(Position, CaptureEvent)>,
    outputs: Vec<Output>,
    scroll_discrete_pending: bool,
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.state.wayland_fd
    }
}

pub struct LayerShellInputCapture(AsyncFd<Inner>);

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    pos: Position,
    /// Top-left of this (possibly partial) edge surface in compositor
    /// coordinates. Combined with `wl_pointer::Enter`'s surface-local point
    /// to recover the exact screen-space crossing coordinate.
    surface_origin: (i32, i32),
}

#[derive(Clone, Copy, Debug)]
struct CrossingPreflight {
    pos: Position,
    pointer_serial: u32,
    cursor: (i32, i32),
    normalized_cursor: Option<(f32, f32)>,
}

impl Window {
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        pos: Position,
        output_pos: (i32, i32),
        segment: EdgeSegment,
    ) -> Window {
        log::debug!("creating window output: {output:?}, segment: {segment:?}");
        let g = &state.globals;

        let (width, height, surface_origin, anchor, top_margin, left_margin) = match pos {
            Position::Left => (
                1,
                segment.len(),
                (segment.coordinate, segment.start),
                Anchor::Left | Anchor::Top,
                segment.start - output_pos.1,
                0,
            ),
            Position::Right => (
                1,
                segment.len(),
                (segment.coordinate, segment.start),
                Anchor::Right | Anchor::Top,
                segment.start - output_pos.1,
                0,
            ),
            Position::Top => (
                segment.len(),
                1,
                (segment.start, segment.coordinate),
                Anchor::Top | Anchor::Left,
                0,
                segment.start - output_pos.0,
            ),
            Position::Bottom => (
                segment.len(),
                1,
                (segment.start, segment.coordinate),
                Anchor::Bottom | Anchor::Left,
                0,
                segment.start - output_pos.0,
            ),
        };
        let mut file = tempfile::tempfile().unwrap();
        draw(&mut file, (width, height));
        let pool = g
            .shm
            .create_pool(file.as_fd(), (width * height * 4) as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        let surface = g.compositor.create_surface(qh, ());

        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Overlay,
            "Mousehop Sharing".into(),
            qh,
            (),
        );
        layer_surface.set_anchor(anchor);
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_margin(top_margin, 0, 0, left_margin);
        surface.set_input_region(None);
        surface.commit();
        Window {
            pos,
            buffer,
            surface,
            layer_surface,
            surface_origin,
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        log::debug!("destroying window!");
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

/// Translate `wl_pointer.enter` surface-local coords into the host's
/// compositor coordinate space, using the layer-surface's anchor edge
/// and the output it's attached to. Layer surfaces here are 1 px on
/// the on-axis dimension and span the cross-axis, so the surface-local
/// cross-axis coord is the screen offset directly.
fn surface_to_screen(window: &Window, surface_x: f64, surface_y: f64) -> (i32, i32) {
    let (origin_x, origin_y) = window.surface_origin;
    (origin_x + surface_x as i32, origin_y + surface_y as i32)
}

/// Translate a host screen-space warp target into the coordinate system of
/// the layer surface that owns the active pointer lock. The hint may be
/// outside the 1 px barrier surface: the pointer-constraints protocol uses
/// it as the compositor's desired global landing point when the lock ends.
fn screen_to_surface(surface_origin: (i32, i32), screen: (i32, i32)) -> (f64, f64) {
    let (surface_x, surface_y) = surface_origin;
    (
        f64::from(screen.0) - f64::from(surface_x),
        f64::from(screen.1) - f64::from(surface_y),
    )
}

fn display_edge(pos: Position) -> DisplayEdge {
    match pos {
        Position::Left => DisplayEdge::Left,
        Position::Right => DisplayEdge::Right,
        Position::Top => DisplayEdge::Top,
        Position::Bottom => DisplayEdge::Bottom,
    }
}

fn output_layout(outputs: &[Output]) -> DisplayLayout {
    // Feed one tuple per wl_output, including an invalid placeholder while
    // xdg-output information is pending. DisplayLayout retains tuple indices,
    // so EdgeSegment::rect_index continues to address `outputs` directly.
    DisplayLayout::new(outputs.iter().map(|output| {
        output.info.as_ref().map_or((0, 0, 0, 0), |info| {
            (
                info.position.0,
                info.position.1,
                u32::try_from(info.size.0).unwrap_or(0),
                u32::try_from(info.size.1).unwrap_or(0),
            )
        })
    }))
}

fn lost_active_seat_device(
    had_pointer: bool,
    had_keyboard: bool,
    has_pointer: bool,
    has_keyboard: bool,
) -> bool {
    (had_pointer && !has_pointer) || (had_keyboard && !has_keyboard)
}

fn get_output_configuration(state: &State, pos: Position) -> Vec<(Output, EdgeSegment)> {
    let layout = output_layout(&state.outputs);
    layout
        .exposed_segments(display_edge(pos))
        .into_iter()
        .filter_map(|segment| {
            state
                .outputs
                .get(segment.rect_index)
                .cloned()
                .map(|output| (output, segment))
        })
        .collect()
}

fn draw(f: &mut File, (width, height): (u32, u32)) {
    let mut buf = BufWriter::new(f);
    for _ in 0..height {
        for _ in 0..width {
            if env::var("LM_DEBUG_LAYER_SHELL").ok().is_some() {
                // AARRGGBB
                buf.write_all(&0xff11d116u32.to_ne_bytes()).unwrap();
            } else {
                // AARRGGBB
                buf.write_all(&0x00000000u32.to_ne_bytes()).unwrap();
            }
        }
    }
}

impl LayerShellInputCapture {
    pub fn new() -> std::result::Result<Self, LayerShellCaptureCreationError> {
        let conn = Connection::connect_to_env()?;
        let (global_list, mut queue) = registry_queue_init::<State>(&conn)?;

        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = global_list
            .bind(&qh, 4..=5, ())
            .map_err(|e| WaylandBindError::new(e, "wl_compositor 4..=5"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = global_list
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;
        let shm: wl_shm::WlShm = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wl_shm"))?;
        let layer_shell: ZwlrLayerShellV1 = global_list
            .bind(&qh, 3..=4, ())
            .map_err(|e| WaylandBindError::new(e, "wlr_layer_shell 3..=4"))?;
        let seat: wl_seat::WlSeat = global_list
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let pointer_constraints: ZwpPointerConstraintsV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_pointer_constraints_v1"))?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_relative_pointer_manager_v1"))?;
        let shortcut_inhibit_manager: Result<
            ZwpKeyboardShortcutsInhibitManagerV1,
            WaylandBindError,
        > = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_keyboard_shortcuts_inhibit_manager_v1"));
        // layer-shell backend still works without this protocol so we make it an optional dependency
        if let Err(e) = &shortcut_inhibit_manager {
            log::warn!("shortcut_inhibit_manager not supported: {e}\nkeybinds handled by the compositor will not be passed
                to the client");
        }
        let shortcut_inhibit_manager = shortcut_inhibit_manager.ok();

        let mut state = State {
            active_positions: Default::default(),
            crossing_modifiers: Default::default(),
            pointer: None,
            keyboard: None,
            global_list,
            globals: Globals {
                compositor,
                shm,
                layer_shell,
                seat,
                pointer_constraints,
                relative_pointer_manager,
                shortcut_inhibit_manager,
                xdg_output_manager,
            },
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            active_windows: Vec::new(),
            focused: None,
            crossing_preflight: None,
            qh,
            wayland_fd: queue.as_fd().as_raw_fd(),
            read_guard: None,
            pending_events: VecDeque::new(),
            outputs: vec![],
            scroll_discrete_pending: false,
        };

        for global in state.global_list.contents().clone_list() {
            state.register_global(global);
        }

        // flush outgoing events
        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                    continue;
                }
            }
        };
        state.read_guard = Some(read_guard);

        let inner = AsyncFd::new(Inner { queue, state })?;

        Ok(LayerShellInputCapture(inner))
    }

    fn add_client(&mut self, pos: Position) {
        self.0.get_mut().state.add_client(pos);
    }

    fn delete_client(&mut self, pos: Position) {
        let inner = self.0.get_mut();
        inner.state.active_positions.remove(&pos);
        inner.state.crossing_modifiers.remove(&pos);

        // A single edge can be removed while another edge owns the live
        // pointer/keyboard grab. Preserve that unrelated focus. If this edge
        // does own the grab, tear it down while its Window is still alive so
        // `ungrab` can reset Exclusive keyboard interactivity and the wrapper
        // receives an AutoRelease for the interrupted capture.
        let focused_pos = inner.state.focused.as_ref().map(|window| window.pos);
        if deleting_position_interrupts_focus(focused_pos, pos) {
            inner.state.lose_focus();
        }
        inner
            .state
            .active_windows
            .retain(|window| window.pos != pos);
    }
}

fn deleting_position_interrupts_focus(focused: Option<Position>, deleted: Position) -> bool {
    focused == Some(deleted)
}

fn surface_leave_matches_focus<T: PartialEq>(
    focused_surface: Option<&T>,
    leaving_surface: &T,
) -> bool {
    focused_surface.is_some_and(|surface| surface == leaving_surface)
}

impl State {
    fn update_output_info(&mut self, name: u32) {
        let Some(output) = self.outputs.iter_mut().find(|o| o.global.name == name) else {
            log::debug!("ignoring update for removed output {name}");
            return;
        };
        if output.has_xdg_info {
            output.info.replace(output.pending_info.clone());
            self.update_windows();
        }
    }

    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() == "wl_output" {
            log::debug!("new output global: wl_output {}", global.name);
            let wl_output = self.global_list.registry().bind::<WlOutput, _, _>(
                global.name,
                4,
                &self.qh,
                global.name,
            );
            let xdg_output =
                self.globals
                    .xdg_output_manager
                    .get_xdg_output(&wl_output, &self.qh, global.name);
            self.outputs.push(Output {
                wl_output,
                xdg_output,
                global,
                info: None,
                has_xdg_info: false,
                pending_info: Default::default(),
            })
        }
    }

    fn deregister_global(&mut self, name: u32) {
        let previous_len = self.outputs.len();
        self.outputs.retain(|o| {
            if o.global.name == name {
                log::debug!("{o} (global {:?}) removed", o.global);
                o.xdg_output.destroy();
                o.wl_output.release();
                false
            } else {
                true
            }
        });
        if self.outputs.len() != previous_len {
            self.update_windows();
        }
    }

    fn grab(
        &mut self,
        surface: &WlSurface,
        pointer: &WlPointer,
        serial: u32,
        qh: &QueueHandle<State>,
    ) {
        let window = self.focused.as_ref().unwrap();

        // hide the cursor
        pointer.set_cursor(serial, None, 0, 0);

        // capture input
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();

        // lock pointer
        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.globals.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                qh,
                (),
            ));
        }

        // request relative input
        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(self.globals.relative_pointer_manager.get_relative_pointer(
                pointer,
                qh,
                (),
            ));
        }

        // capture modifier keys
        if let Some(shortcut_inhibit_manager) = &self.globals.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor = Some(shortcut_inhibit_manager.inhibit_shortcuts(
                    surface,
                    &self.globals.seat,
                    qh,
                    (),
                ));
            }
        }
    }

    fn start_crossing_preflight(
        &mut self,
        pointer_serial: u32,
        cursor: (i32, i32),
        normalized_cursor: Option<(f32, f32)>,
    ) {
        let window = self.focused.as_ref().expect("focused edge");
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();
        self.crossing_preflight = Some(CrossingPreflight {
            pos: window.pos,
            pointer_serial,
            cursor,
            normalized_cursor,
        });
    }

    fn reject_crossing_preflight(&mut self) {
        if let Some(window) = self.focused.as_ref() {
            window
                .layer_surface
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            window.surface.commit();
        }
        self.crossing_preflight = None;
    }

    fn ungrab(&mut self, warp_target: Option<(i32, i32)>) {
        // Only the keyboard-interactivity reset and the release warp
        // need a focused window; the teardown below must run
        // regardless. `focused` is cleared by a pointer `Leave` and by
        // output reconfiguration, either of which can race a release —
        // and returning early there stranded the pointer lock. Because
        // that lock is `Lifetime::Persistent`, a stranded one pins the
        // compositor's cursor for good: warps are ignored, focus never
        // follows the mouse again, and nothing short of restarting the
        // daemon frees it.
        if let Some(window) = self.focused.as_ref() {
            // Restore normal keyboard focus. If the caller modeled a host
            // landing point, attach it to the still-live pointer lock before
            // committing and destroying the lock. Cursor position hints are
            // double-buffered Wayland surface state; without this commit,
            // Hyprland unlocks at the stale pre-capture cursor position.
            window
                .layer_surface
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            if let (Some(pointer_lock), Some(warp_target)) = (&self.pointer_lock, warp_target) {
                let (surface_x, surface_y) = screen_to_surface(window.surface_origin, warp_target);
                log::info!(
                    "[release-warp] layer-shell screen target {warp_target:?} -> surface hint ({surface_x:.1}, {surface_y:.1})"
                );
                pointer_lock.set_cursor_position_hint(surface_x, surface_y);
            }
            window.surface.commit();
        } else if self.pointer_lock.is_some() {
            log::warn!(
                "ungrab with no focused window — tearing down a pointer lock that would \
                 otherwise strand the cursor"
            );
        }

        // destroy pointer lock
        if let Some(pointer_lock) = &self.pointer_lock {
            pointer_lock.destroy();
            self.pointer_lock = None;
        }

        // destroy relative input
        if let Some(rel_pointer) = &self.rel_pointer {
            rel_pointer.destroy();
            self.rel_pointer = None;
        }

        // destroy shortcut inhibitor
        if let Some(shortcut_inhibitor) = &self.shortcut_inhibitor {
            shortcut_inhibitor.destroy();
            self.shortcut_inhibitor = None;
        }
        self.crossing_preflight = None;
    }

    /// Tear down a compositor-side grab whose surface disappeared or lost
    /// pointer focus. Notify the higher capture task only when a live pointer
    /// lock proves that a remote-control interval was actually interrupted.
    fn lose_focus(&mut self) {
        let interrupted_position = self
            .pointer_lock
            .as_ref()
            .and_then(|_| self.focused.as_ref().map(|window| window.pos));
        self.ungrab(None);
        self.focused = None;
        if let Some(position) = interrupted_position {
            self.pending_events
                .push_back((position, CaptureEvent::AutoRelease));
        }
    }

    fn add_client(&mut self, pos: Position) {
        self.active_positions.insert(pos);
        let outputs = get_output_configuration(self, pos);

        log::info!(
            "adding capture for position {pos} - using output segments: {:?}",
            outputs
                .iter()
                .map(|(output, segment)| (
                    output
                        .info
                        .as_ref()
                        .map(|i| i.name.to_owned())
                        .unwrap_or("unknown output".to_owned()),
                    segment
                ))
                .collect::<Vec<_>>()
        );
        outputs.iter().for_each(|(output, segment)| {
            if let Some(info) = output.info.as_ref() {
                let window = Window::new(
                    self,
                    &self.qh,
                    &output.wl_output,
                    pos,
                    info.position,
                    *segment,
                );
                let window = Arc::new(window);
                self.active_windows.push(window);
            }
        });
    }

    fn update_windows(&mut self) {
        log::info!("active outputs: ");
        for output in self.outputs.iter().filter(|o| o.info.is_some()) {
            log::info!(" * {output}");
        }

        if self.focused.is_some() {
            self.lose_focus();
        }
        self.active_windows.clear();

        let active_positions = self.active_positions.iter().cloned().collect::<Vec<_>>();
        for pos in active_positions {
            self.add_client(pos);
        }
    }
}

impl Inner {
    fn read(&mut self) -> bool {
        match self.state.read_guard.take().unwrap().read() {
            Ok(_) => true,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => false,
            Err(WaylandError::Io(e)) => {
                log::error!("error reading from wayland socket: {e}");
                false
            }
            Err(WaylandError::Protocol(e)) => {
                panic!("wayland protocol violation: {e}")
            }
        }
    }

    fn prepare_read(&mut self) -> io::Result<()> {
        loop {
            match self.queue.prepare_read() {
                None => match self.queue.dispatch_pending(&mut self.state) {
                    Ok(_) => continue,
                    Err(DispatchError::Backend(WaylandError::Io(e))) => return Err(e),
                    Err(e) => panic!("failed to dispatch wayland events: {e}"),
                },
                Some(r) => {
                    self.state.read_guard = Some(r);
                    break Ok(());
                }
            }
        }
    }

    fn dispatch_events(&mut self) {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => {}
            Err(DispatchError::Backend(WaylandError::Io(e))) => {
                log::error!("Wayland Error: {e}");
            }
            Err(DispatchError::Backend(e)) => {
                panic!("backend error: {e}");
            }
            Err(DispatchError::BadMessage {
                sender_id,
                interface,
                opcode,
            }) => {
                panic!("bad message {sender_id}, {interface} , {opcode}");
            }
        }
    }

    fn flush_events(&mut self) -> io::Result<()> {
        // flush outgoing events
        match self.queue.flush() {
            Ok(_) => (),
            Err(e) => match e {
                WaylandError::Io(e) => {
                    return Err(e);
                }
                WaylandError::Protocol(e) => {
                    panic!("wayland protocol violation: {e}")
                }
            },
        }
        Ok(())
    }
}

#[async_trait]
impl Capture for LayerShellInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.add_client(pos);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.delete_client(pos);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn release(&mut self, warp_target: Option<(i32, i32)>) -> Result<(), CaptureError> {
        log::debug!("releasing pointer");
        let inner = self.0.get_mut();
        inner.state.ungrab(warp_target);
        Ok(inner.flush_events()?)
    }

    async fn set_crossing_modifier(
        &mut self,
        pos: Position,
        modifier: Option<CrossingModifier>,
    ) -> Result<(), CaptureError> {
        let inner = self.0.get_mut();
        if let Some(modifier) = modifier {
            inner.state.crossing_modifiers.insert(pos, modifier);
        } else {
            inner.state.crossing_modifiers.remove(&pos);
        }
        Ok(inner.flush_events()?)
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn display_bounds(&self) -> Option<(u32, u32)> {
        self.display_layout()?.size()
    }

    fn display_origin(&self) -> (i32, i32) {
        self.display_layout()
            .and_then(|layout| layout.origin())
            .unwrap_or((0, 0))
    }

    fn display_layout(&self) -> Option<DisplayLayout> {
        let layout = output_layout(&self.0.get_ref().state.outputs);
        (!layout.is_empty()).then_some(layout)
    }
}

impl Stream for LayerShellInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.0.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            let mut guard = ready!(self.0.poll_read_ready_mut(cx))?;

            {
                let inner = guard.get_inner_mut();

                // read events
                while inner.read() {
                    // prepare next read
                    match inner.prepare_read() {
                        Ok(_) => {}
                        Err(e) => return Poll::Ready(Some(Err(e.into()))),
                    }
                }

                // dispatch the events
                inner.dispatch_events();

                // flush outgoing events
                if let Err(e) = inner.flush_events() {
                    if e.kind() != ErrorKind::WouldBlock {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                }

                // prepare for the next read
                match inner.prepare_read() {
                    Ok(_) => {}
                    Err(e) => return Poll::Ready(Some(Err(e.into()))),
                }
            }

            // clear read readiness for tokio read guard
            // guard.clear_ready_matching(Ready::READABLE);
            guard.clear_ready();

            // if an event has been queued during dispatch_events() we return it
            match guard.get_inner_mut().state.pending_events.pop_front() {
                Some(event) => return Poll::Ready(Some(Ok(event))),
                None => continue,
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            let has_pointer = capabilities.contains(wl_seat::Capability::Pointer);
            let has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
            if lost_active_seat_device(
                state.pointer.is_some(),
                state.keyboard.is_some(),
                has_pointer,
                has_keyboard,
            ) && (state.focused.is_some() || state.pointer_lock.is_some())
            {
                log::warn!("seat lost an input capability during capture");
                state.lose_focus();
            }

            if has_pointer {
                if state.pointer.is_none() {
                    state.pointer.replace(seat.get_pointer(qh, ()));
                }
            } else if let Some(pointer) = state.pointer.take() {
                pointer.release();
            }
            if has_keyboard {
                if state.keyboard.is_none() {
                    state.keyboard.replace(seat.get_keyboard(qh, ()));
                }
            } else if let Some(keyboard) = state.keyboard.take() {
                keyboard.release();
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        app: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                let Some(window) = app
                    .active_windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .cloned()
                else {
                    return;
                };
                app.focused = Some(window.clone());
                let cursor = surface_to_screen(&window, surface_x, surface_y);
                let layout = output_layout(&app.outputs);
                let normalized_cursor = normalize_cursor_in_layout(&layout, cursor);
                if app.crossing_modifiers.contains_key(&window.pos) {
                    app.start_crossing_preflight(serial, cursor, normalized_cursor);
                } else {
                    app.grab(&surface, pointer, serial, qh);
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Begin {
                            cursor: Some(cursor),
                            normalized_cursor,
                        },
                    ));
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                /* There are rare cases, where when a window is opened in
                 * just the wrong moment, the pointer is released, while
                 * still grabbed.
                 * In that case, the pointer must be ungrabbed, otherwise
                 * it is impossible to grab it again (since the pointer
                 * lock, relative pointer,... objects are still in place)
                 */
                if surface_leave_matches_focus(
                    app.focused.as_ref().map(|window| &window.surface),
                    &surface,
                ) {
                    if app.pointer_lock.is_some() {
                        log::warn!("compositor released mouse");
                    }
                    app.lose_focus();
                } else {
                    log::debug!("ignoring pointer Leave for a stale layer surface");
                }
            }
            wl_pointer::Event::Button {
                serial: _,
                time,
                button,
                state,
            } => {
                if app.pointer_lock.is_none() {
                    return;
                }
                let Some(window) = app.focused.as_ref() else {
                    log::debug!("dropping pointer button queued after capture lost focus");
                    return;
                };
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                        time,
                        button,
                        state: u32::from(state),
                    })),
                ));
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                if app.pointer_lock.is_none() {
                    app.scroll_discrete_pending = false;
                    return;
                }
                let Some(window) = app.focused.as_ref() else {
                    app.scroll_discrete_pending = false;
                    log::debug!("dropping pointer axis queued after capture lost focus");
                    return;
                };
                if app.scroll_discrete_pending {
                    // each axisvalue120 event is coupled with
                    // a corresponding axis event, which needs to
                    // be ignored to not duplicate the scrolling
                    app.scroll_discrete_pending = false;
                } else {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                            time,
                            axis: u32::from(axis) as u8,
                            value,
                            momentum: false,
                        })),
                    ));
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                if app.pointer_lock.is_none() {
                    app.scroll_discrete_pending = false;
                    return;
                }
                let Some(window) = app.focused.as_ref() else {
                    log::debug!("dropping discrete pointer axis queued after capture lost focus");
                    return;
                };
                app.scroll_discrete_pending = true;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: u32::from(axis) as u8,
                        value: value120,
                    })),
                ));
            }
            wl_pointer::Event::Frame => {
                // TODO properly handle frame events
                // we simply insert a frame event on the client side
                // after each event for now
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                serial: _,
                time,
                key,
                state,
            } => {
                if app.pointer_lock.is_some() {
                    if let Some(window) = &app.focused {
                        app.pending_events.push_back((
                            window.pos,
                            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                                time,
                                key,
                                state: u32::from(state) as u8,
                            })),
                        ));
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if app.pointer_lock.is_some() {
                    if let Some(window) = &app.focused {
                        app.pending_events.push_back((
                            window.pos,
                            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                                depressed: mods_depressed,
                                latched: mods_latched,
                                locked: mods_locked,
                                group,
                            })),
                        ));
                    }
                }
            }
            wl_keyboard::Event::Enter { keys, .. } => {
                let Some(window) = app.focused.clone() else {
                    return;
                };
                // `keys` is a native-endian wl_array of evdev u32s held
                // before this layer surface gained keyboard focus. Replay
                // only momentary modifiers: they must work immediately on
                // the peer and participate in release-bind/pressed-key
                // cleanup. Replaying characters or toggle locks here would
                // type phantom text or invert Caps/Num/Scroll on every
                // boundary crossing.
                let held_modifiers = held_modifiers_on_enter(&keys);

                if let Some(preflight) = app.crossing_preflight.take() {
                    if preflight.pos != window.pos {
                        log::debug!(
                            "discarding stale crossing preflight for {:?}",
                            preflight.pos
                        );
                        app.reject_crossing_preflight();
                        return;
                    }
                    let required = app.crossing_modifiers.get(&window.pos).copied();
                    if crossing_preflight_allows(required, &held_modifiers) {
                        let Some(pointer) = app.pointer.clone() else {
                            app.reject_crossing_preflight();
                            return;
                        };
                        app.grab(&window.surface, &pointer, preflight.pointer_serial, qh);
                        app.pending_events.push_back((
                            window.pos,
                            CaptureEvent::Begin {
                                cursor: Some(preflight.cursor),
                                normalized_cursor: preflight.normalized_cursor,
                            },
                        ));
                        queue_modifier_snapshot(app, window.pos, &held_modifiers);
                    } else {
                        log::debug!(
                            "crossing preflight blocked {}: {:?} is not held",
                            window.pos,
                            required.expect("checked above")
                        );
                        app.reject_crossing_preflight();
                    }
                } else if app.pointer_lock.is_some() {
                    queue_modifier_snapshot(app, window.pos, &held_modifiers);
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                let matches_focus = surface_leave_matches_focus(
                    app.focused.as_ref().map(|window| &window.surface),
                    &surface,
                );
                if matches_focus {
                    if app.pointer_lock.is_some() {
                        log::warn!("compositor released keyboard focus during capture");
                        app.lose_focus();
                    } else {
                        // A rejected preflight deliberately gives keyboard
                        // focus back while pointer focus stays on the inert
                        // 1 px edge. Keep that pointer focus until wl_pointer
                        // Leave so resting at the edge cannot retrigger checks.
                        app.crossing_preflight = None;
                    }
                } else {
                    // Output reconfiguration or a rapid re-entry can leave a
                    // Leave for an old surface queued behind focus on another
                    // edge. It must not tear down that newer live grab.
                    log::debug!("ignoring keyboard Leave for a stale layer surface");
                }
            }
            _ => (),
        }
    }
}

fn held_modifiers_on_enter(keys: &[u8]) -> Vec<u32> {
    keys.chunks_exact(size_of::<u32>())
        .filter_map(|bytes| {
            let key = u32::from_ne_bytes(bytes.try_into().expect("exact u32 chunk"));
            scancode::Linux::try_from(key)
                .ok()
                .filter(|key| {
                    matches!(
                        key,
                        scancode::Linux::KeyLeftShift
                            | scancode::Linux::KeyRightShift
                            | scancode::Linux::KeyLeftCtrl
                            | scancode::Linux::KeyRightCtrl
                            | scancode::Linux::KeyLeftAlt
                            | scancode::Linux::KeyRightalt
                            | scancode::Linux::KeyLeftMeta
                            | scancode::Linux::KeyRightmeta
                    )
                })
                .map(|key| key as u32)
        })
        .collect()
}

fn held_modifiers_contain(keys: &[u32], modifier: CrossingModifier) -> bool {
    let matches = |key| match modifier {
        CrossingModifier::Control => matches!(
            key,
            scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl
        ),
        CrossingModifier::Alt => matches!(
            key,
            scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt
        ),
        CrossingModifier::Shift => matches!(
            key,
            scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift
        ),
        CrossingModifier::Super => matches!(
            key,
            scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta
        ),
    };
    keys.iter()
        .any(|key| scancode::Linux::try_from(*key).ok().is_some_and(matches))
}

fn crossing_preflight_allows(required: Option<CrossingModifier>, held_modifiers: &[u32]) -> bool {
    required.is_none_or(|modifier| held_modifiers_contain(held_modifiers, modifier))
}

fn queue_modifier_snapshot(app: &mut State, pos: Position, held_modifiers: &[u32]) {
    for &key in held_modifiers {
        app.pending_events.push_back((
            pos,
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state: 1,
            })),
        ));
    }
    // Always terminate the held-key snapshot with an aggregate state,
    // including zero, so the higher-level safety gate has a conclusive
    // decision on every backend-generated Begin.
    app.pending_events.push_back((
        pos,
        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: modifier_mask_for_keys(held_modifiers),
            latched: 0,
            locked: 0,
            group: 0,
        })),
    ));
}

fn modifier_mask_for_keys(keys: &[u32]) -> u32 {
    const SHIFT_MASK: u32 = 1 << 0;
    const CONTROL_MASK: u32 = 1 << 2;
    const ALT_MASK: u32 = 1 << 3;
    const SUPER_MASK: u32 = 1 << 6;

    keys.iter().fold(0, |mask, key| {
        mask | match scancode::Linux::try_from(*key) {
            Ok(scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift) => SHIFT_MASK,
            Ok(scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl) => CONTROL_MASK,
            Ok(scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt) => ALT_MASK,
            Ok(scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta) => SUPER_MASK,
            _ => 0,
        }
    })
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: <ZwpRelativePointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if let Some(window) = &app.focused {
                let time = ((((utime_hi as u64) << 32) | utime_lo as u64) / 1000) as u32;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })),
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        app: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = app
                .active_windows
                .iter()
                .find(|w| &w.layer_surface == layer_surface)
            {
                // client corresponding to the layer_surface
                let surface = &window.surface;
                let buffer = &window.buffer;
                surface.attach(Some(buffer), 0, 0);
                layer_surface.ack_configure(serial);
                surface.commit();
            }
        }
    }
}

// delegate wl_registry events to App itself
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.deregister_global(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.iter_mut().find(|o| o.global.name == *name) else {
            log::debug!("ignoring xdg-output event for removed output {name}");
            return;
        };

        log::debug!("xdg_output {name} - {event:?}");
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Done => {
                log::warn!("Use of deprecated xdg-output event \"done\"");
                state.update_output_info(*name);
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { description } => {
                output.pending_info.description = description;
                output.has_xdg_info = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _wl_output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::debug!("wl_output {name} - {event:?}");
        if let wl_output::Event::Done = event {
            state.update_output_info(*name);
        }
    }
}

// don't emit any events
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

// ignore events
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);

#[cfg(test)]
mod tests {
    use super::{
        crossing_preflight_allows, deleting_position_interrupts_focus, held_modifiers_on_enter,
        lost_active_seat_device, modifier_mask_for_keys, screen_to_surface,
        surface_leave_matches_focus,
    };
    use crate::Position;
    use input_event::CrossingModifier;
    use input_event::scancode::Linux::{
        KeyA, KeyCapsLock, KeyLeftAlt, KeyLeftMeta, KeyLeftShift, KeyNumlock, KeyRightCtrl,
        KeyRightShift, KeyScrollLock,
    };

    fn native_keys(keys: &[input_event::scancode::Linux]) -> Vec<u8> {
        keys.iter()
            .flat_map(|key| (*key as u32).to_ne_bytes())
            .collect()
    }

    #[test]
    fn screen_warp_target_is_relative_to_each_anchored_surface() {
        assert_eq!(screen_to_surface((1920, -240), (1920, 719)), (0.0, 959.0));
        assert_eq!(screen_to_surface((2559, -240), (2559, 719)), (0.0, 959.0));
        assert_eq!(screen_to_surface((1920, -240), (2559, -240)), (639.0, 0.0));
        assert_eq!(screen_to_surface((1920, 719), (2559, 719)), (639.0, 0.0));
    }

    #[test]
    fn an_active_seat_device_loss_interrupts_capture() {
        assert!(lost_active_seat_device(true, true, false, true));
        assert!(lost_active_seat_device(true, true, true, false));
        assert!(!lost_active_seat_device(true, true, true, true));
        assert!(!lost_active_seat_device(false, false, false, false));
    }

    #[test]
    fn keyboard_leave_only_matches_the_current_surface() {
        let focused_surface = 17_u32;
        let stale_surface = 23_u32;

        assert!(surface_leave_matches_focus(
            Some(&focused_surface),
            &focused_surface
        ));
        assert!(!surface_leave_matches_focus(
            Some(&focused_surface),
            &stale_surface
        ));
        assert!(!surface_leave_matches_focus::<u32>(None, &focused_surface));
    }

    #[test]
    fn deleting_an_edge_only_interrupts_focus_owned_by_that_edge() {
        assert!(deleting_position_interrupts_focus(
            Some(Position::Right),
            Position::Right
        ));
        assert!(!deleting_position_interrupts_focus(
            Some(Position::Right),
            Position::Left
        ));
        assert!(!deleting_position_interrupts_focus(None, Position::Right));
    }

    #[test]
    fn keyboard_enter_forwards_only_preheld_momentary_modifiers() {
        let keys = native_keys(&[
            KeyLeftShift,
            KeyA,
            KeyCapsLock,
            KeyNumlock,
            KeyScrollLock,
            KeyRightCtrl,
        ]);

        assert_eq!(
            held_modifiers_on_enter(&keys),
            vec![KeyLeftShift as u32, KeyRightCtrl as u32],
        );
    }

    #[test]
    fn keyboard_enter_ignores_trailing_partial_keycode() {
        let mut keys = native_keys(&[KeyLeftShift]);
        keys.extend_from_slice(&[0xaa, 0xbb]);

        assert_eq!(held_modifiers_on_enter(&keys), vec![KeyLeftShift as u32]);
    }

    #[test]
    fn keyboard_enter_snapshot_includes_aggregate_modifier_mask() {
        assert_eq!(
            modifier_mask_for_keys(&[KeyLeftShift as u32, KeyRightCtrl as u32]),
            (1 << 0) | (1 << 2)
        );
        assert_eq!(modifier_mask_for_keys(&[]), 0);
    }

    #[test]
    fn crossing_preflight_only_checks_when_the_gate_is_enabled() {
        assert!(crossing_preflight_allows(None, &[]));
        assert!(!crossing_preflight_allows(
            Some(CrossingModifier::Control),
            &[]
        ));
        assert!(crossing_preflight_allows(
            Some(CrossingModifier::Control),
            &[KeyRightCtrl as u32]
        ));
    }

    #[test]
    fn crossing_preflight_accepts_either_side_of_each_modifier_family() {
        assert!(crossing_preflight_allows(
            Some(CrossingModifier::Alt),
            &[KeyLeftAlt as u32]
        ));
        assert!(crossing_preflight_allows(
            Some(CrossingModifier::Shift),
            &[KeyRightShift as u32]
        ));
        assert!(crossing_preflight_allows(
            Some(CrossingModifier::Super),
            &[KeyLeftMeta as u32]
        ));
        assert!(!crossing_preflight_allows(
            Some(CrossingModifier::Alt),
            &[KeyLeftShift as u32]
        ));
    }
}
