use ashpd::{
    desktop::{
        Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSessionOptions,
            InputCapture, Region, ReleaseOptions, ZonesChanged,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use input_event::{
    Event,
    display::{DisplayEdge, DisplayLayout, EdgeSegment},
};

use crate::{CaptureEvent, normalize_cursor_in_layout};

use super::{
    Capture as MousehopInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/* There is a bug in xdg-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * GNOME therefore needs a new session when barriers or EIS devices change.
 * Other backends keep one portal/EIS session and update barriers in place. */

/// Minimum time to wait after closing a portal session before opening
/// a new one.
///
/// Some EIS backends release the previous connection asynchronously with
/// respect to `Session::close()`. This cooldown protects the exceptional
/// reconnect paths below. Routine zone and client updates must not reach those
/// paths: repeated recreation can accumulate backend resources and eventually
/// make a new EIS connection fail.
const SESSION_RECREATE_COOLDOWN: Duration = Duration::from_millis(300);

/// Events that change the configured client barriers.
#[derive(Clone, Copy, Debug)]
enum LibeiNotifyEvent {
    Create(Position),
    Destroy(Position),
}

#[allow(dead_code)]
pub struct LibeiInputCapture {
    input_capture: Pin<Box<InputCapture>>,
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    release_capture: Sender<ReleaseRequest>,
    display_layout: Arc<Mutex<Option<DisplayLayout>>>,
    cancellation_token: CancellationToken,
    terminated: bool,
}

#[derive(Debug)]
struct ReleaseRequest {
    warp_target: Option<(i32, i32)>,
    /// Fulfilled only after the portal release call has completed. Dropping
    /// this sender reports a failed/aborted release to the public backend.
    completion: oneshot::Sender<()>,
}

/// Full portal-zone topology in the logical coordinate space used by
/// `Activated.cursor_position`. Retaining the individual output rectangles,
/// rather than only their bounding union, lets both pointer barriers and
/// cursor-return projection follow a stepped multi-monitor contour.
fn portal_display_layout(regions: &[Region]) -> DisplayLayout {
    portal_display_layout_from_rects(regions.iter().map(|region| {
        (
            region.x_offset(),
            region.y_offset(),
            region.width(),
            region.height(),
        )
    }))
}

fn portal_display_layout_from_rects(
    regions: impl IntoIterator<Item = (i32, i32, u32, u32)>,
) -> DisplayLayout {
    DisplayLayout::new(regions)
}

fn portal_cursor_position(cursor: Option<(f32, f32)>) -> Option<(i32, i32)> {
    cursor.and_then(|(x, y)| {
        (x.is_finite() && y.is_finite()).then(|| (x.round() as i32, y.round() as i32))
    })
}

fn display_edge(pos: Position) -> DisplayEdge {
    match pos {
        Position::Left => DisplayEdge::Left,
        Position::Right => DisplayEdge::Right,
        Position::Top => DisplayEdge::Top,
        Position::Bottom => DisplayEdge::Bottom,
    }
}

/// Convert a contained contour segment to the inclusive endpoint convention
/// used by the portal's pointer-barrier protocol. DisplayLayout represents
/// right and bottom edges at their final contained pixel; the portal places
/// those barriers at the rectangle's exclusive coordinate instead.
fn segment_to_barrier(segment: EdgeSegment) -> (i32, i32, i32, i32) {
    match segment.edge {
        DisplayEdge::Left => (
            segment.coordinate,
            segment.start,
            segment.coordinate,
            segment.end - 1,
        ),
        DisplayEdge::Right => {
            let x = segment
                .coordinate
                .checked_add(1)
                .expect("validated display right edge must have an exclusive coordinate");
            (x, segment.start, x, segment.end - 1)
        }
        DisplayEdge::Top => (
            segment.start,
            segment.coordinate,
            segment.end - 1,
            segment.coordinate,
        ),
        DisplayEdge::Bottom => {
            let y = segment
                .coordinate
                .checked_add(1)
                .expect("validated display bottom edge must have an exclusive coordinate");
            (segment.start, y, segment.end - 1, y)
        }
    }
}

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

#[derive(Debug)]
struct BarrierConfiguration {
    barriers: Vec<ICBarrier>,
    pos_for_barrier_id: HashMap<BarrierID, Position>,
    zone_set: u32,
    /// Exact portal-zone topology used to create `barriers`.
    layout: DisplayLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureSessionExit {
    Recreate,
    Terminate,
}

fn select_barriers(
    layout: &DisplayLayout,
    clients: &[Position],
    next_barrier_id: &mut NonZeroU32,
) -> (Vec<ICBarrier>, HashMap<BarrierID, Position>) {
    let mut pos_for_barrier = HashMap::new();
    let mut barriers: Vec<ICBarrier> = vec![];

    for pos in clients {
        let mut client_barriers = layout
            .exposed_segments(display_edge(*pos))
            .into_iter()
            // A barrier must contain more than one point. Transient 1x1
            // wl_output fallbacks occur while Hyprland is enumerating a
            // monitor and would otherwise produce an invalid point barrier.
            .filter(|segment| segment.len() > 1)
            .map(|segment| {
                let id = *next_barrier_id;
                *next_barrier_id = next_barrier_id
                    .checked_add(1)
                    .expect("barrier id out of range");
                let position = segment_to_barrier(segment);
                pos_for_barrier.insert(id, *pos);
                ICBarrier::new(id, position)
            })
            .collect();
        barriers.append(&mut client_barriers);
    }
    (barriers, pos_for_barrier)
}

async fn update_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[Position],
    shared_layout: &Mutex<Option<DisplayLayout>>,
    next_barrier_id: &mut NonZeroU32,
) -> Result<BarrierConfiguration, ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");
    let layout = portal_display_layout(zones.regions());
    if layout.is_empty() {
        *shared_layout.lock().expect("display layout mutex poisoned") = None;
        log::warn!("portal returned no usable display zones; clearing cached display layout");
    } else {
        *shared_layout.lock().expect("display layout mutex poisoned") = Some(layout.clone());
    }

    // Keep IDs unique across replacements. Activated is an asynchronous portal
    // signal; a delayed activation from the previous barrier generation must
    // not alias an ID in the current map.
    let (barriers, id_map) = select_barriers(&layout, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let ashpd_barriers: Vec<Barrier> = barriers.iter().copied().map(|b| b.into()).collect();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok(BarrierConfiguration {
        barriers,
        pos_for_barrier_id: id_map,
        zone_set: zones.zone_set(),
        layout,
    })
}

async fn configure_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[Position],
    display_layout: &Mutex<Option<DisplayLayout>>,
    next_barrier_id: &mut NonZeroU32,
) -> Result<BarrierConfiguration, CaptureError> {
    let configuration = update_barriers(
        input_capture,
        session,
        active_clients,
        display_layout,
        next_barrier_id,
    )
    .await?;

    if active_clients.is_empty() {
        log::debug!("all pointer barriers removed; leaving input capture suspended");
    } else {
        log::debug!("enabling session");
        input_capture.enable(session, Default::default()).await?;
    }

    Ok(configuration)
}

fn apply_client_update(active_clients: &mut Vec<Position>, event: LibeiNotifyEvent) -> bool {
    match event {
        LibeiNotifyEvent::Create(pos) if !active_clients.contains(&pos) => {
            active_clients.push(pos);
            true
        }
        LibeiNotifyEvent::Destroy(pos) if active_clients.contains(&pos) => {
            active_clients.retain(|&active_pos| active_pos != pos);
            true
        }
        _ => false,
    }
}

/// Portal signal streams are shared by all sessions on this D-Bus connection.
/// ashpd keeps the session path private, but serializes a `Session` as that path.
fn session_handle(session: &Session<InputCapture>) -> String {
    serde_json::to_value(session)
        .expect("ashpd Session must serialize as an object path")
        .as_str()
        .expect("ashpd Session object path must serialize as a string")
        .to_owned()
}

fn zone_set_is_current_or_newer(current: u32, invalidated: u32) -> bool {
    invalidated.wrapping_sub(current) < (1 << 31)
}

fn should_reconfigure_zones(
    current_session: &str,
    current_zone_set: u32,
    changed_session: &str,
    invalidated_zone_set: Option<u32>,
) -> bool {
    if current_session != changed_session {
        return false;
    }

    match invalidated_zone_set {
        Some(invalidated) => zone_set_is_current_or_newer(current_zone_set, invalidated),
        None => true,
    }
}

async fn create_session(
    input_capture: &InputCapture,
) -> std::result::Result<(Session<InputCapture>, BitFlags<Capabilities>), ashpd::Error> {
    log::debug!("creating input capture session");
    let create_session_options = CreateSessionOptions::default().set_capabilities(
        Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
    );
    input_capture
        .create_session(None, create_session_options)
        .await
}

async fn connect_to_eis(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("com.mousehop.Mousehop", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(Position, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_pos: Rc<Cell<Option<Position>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_pos.get();
        handle_ei_event(ei_event, client, &context, &event_tx, &release_session).await?;
    }
}

impl LibeiInputCapture {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Box::pin(InputCapture::new().await?);
        let input_capture_ptr = input_capture.as_ref().get_ref() as *const InputCapture;
        let first_session = Some(create_session(unsafe { &*input_capture_ptr }).await?);

        let (event_tx, event_rx) = mpsc::channel(1);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let (release_capture, release_rx) = mpsc::channel(1);
        let display_layout = Arc::new(Mutex::new(None));

        let cancellation_token = CancellationToken::new();

        let capture = do_capture(
            input_capture_ptr,
            notify_rx,
            release_rx,
            first_session,
            event_tx,
            display_layout.clone(),
            cancellation_token.clone(),
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            input_capture,
            event_rx,
            capture_task,
            notify_capture,
            release_capture,
            display_layout,
            cancellation_token,
            terminated: false,
        };

        Ok(producer)
    }
}

async fn do_capture(
    input_capture: *const InputCapture,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    mut release_capture: Receiver<ReleaseRequest>,
    session: Option<(Session<InputCapture>, BitFlags<Capabilities>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    display_layout: Arc<Mutex<Option<DisplayLayout>>>,
    cancellation_token: CancellationToken,
) -> Result<(), CaptureError> {
    let mut session = session.map(|s| s.0);

    /* safety: libei_task does not outlive Self */
    let input_capture = unsafe { &*input_capture };
    let mut active_clients: Vec<Position> = vec![];
    let mut next_barrier_id = NonZeroU32::new(1).expect("barrier id must be non-zero");
    let mut last_session_close: Option<Instant> = None;

    let mut zones_changed = input_capture.receive_zones_changed().await?;

    loop {
        // Delay connecting to EIS until at least one client edge needs a barrier.
        while active_clients.is_empty() {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    log::debug!("capture task cancelled before EIS was connected");
                    if let Some(session) = session.take() {
                        if let Err(error) = session.close().await {
                            log::warn!("session.close(): {error}");
                        }
                    }
                    return Ok(());
                }
                changed = zones_changed.next() => {
                    let Some(changed) = changed else {
                        return Err(CaptureError::ZonesChangedClosed);
                    };
                    log::debug!(
                        "ignoring zone update for inactive capture session {} (zone set {:?})",
                        changed.session_handle(),
                        changed.zone_set(),
                    );
                }
                event = capture_event.recv() => {
                    let event = event.ok_or(CaptureError::CaptureUpdatesClosed)?;
                    log::debug!("capture event: {event:?}");
                    apply_client_update(&mut active_clients, event);
                },
                release = release_capture.recv() => {
                    let release = release.ok_or(CaptureError::CaptureUpdatesClosed)?;
                    log::debug!("discarding release request while capture is inactive");
                    let _ = release.completion.send(());
                }
            }
        }

        let session = match session.take() {
            Some(session) => session,
            None => {
                // Session recreation is reserved for a real EIS/session failure and
                // the GNOME device-change workaround. Zone and client updates stay
                // on the existing session below.
                if let Some(closed_at) = last_session_close {
                    let elapsed = closed_at.elapsed();
                    if elapsed < SESSION_RECREATE_COOLDOWN {
                        let remaining = SESSION_RECREATE_COOLDOWN - elapsed;
                        log::debug!(
                            "session recreate cooldown: waiting {remaining:?} before opening a new EIS session"
                        );
                        tokio::time::sleep(remaining).await;
                    }
                }
                create_session(input_capture).await?.0
            }
        };

        let capture_result = do_capture_session(
            input_capture,
            &session,
            &event_tx,
            &mut active_clients,
            &display_layout,
            &mut next_barrier_id,
            &mut release_capture,
            &mut capture_event,
            &mut zones_changed,
            &cancellation_token,
        )
        .await;

        log::debug!("capture session finished; disabling and closing it");
        if let Err(error) = input_capture.disable(&session, Default::default()).await {
            log::warn!("input_capture.disable(&session) {error}");
        }
        if let Err(error) = session.close().await {
            log::warn!("session.close(): {error}");
        }
        last_session_close = Some(Instant::now());

        match capture_result? {
            CaptureSessionExit::Recreate => {
                log::debug!("recreating input capture session after EIS/session exit");
            }
            CaptureSessionExit::Terminate => break Ok(()),
        }
    }
}

// These borrowed inputs are the complete state of one portal/EIS generation;
// grouping them behind interior mutability would obscure their ownership.
#[allow(clippy::too_many_arguments)]
async fn do_capture_session<Z>(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    event_tx: &Sender<(Position, CaptureEvent)>,
    active_clients: &mut Vec<Position>,
    display_layout: &Mutex<Option<DisplayLayout>>,
    next_barrier_id: &mut NonZeroU32,
    release_capture: &mut Receiver<ReleaseRequest>,
    capture_event: &mut Receiver<LibeiNotifyEvent>,
    zones_changed: &mut Z,
    cancellation_token: &CancellationToken,
) -> Result<CaptureSessionExit, CaptureError>
where
    Z: Stream<Item = ZonesChanged> + Unpin,
{
    let current_pos = Rc::new(Cell::new(None));
    let current_session = session_handle(session);

    // Connect once for this portal session. Barrier updates below deliberately
    // retain this EIS connection across Enable/Disable cycles.
    let (context, _connection, ei_event_stream) = connect_to_eis(input_capture, session).await?;

    let mut barrier_configuration = configure_barriers(
        input_capture,
        session,
        active_clients,
        display_layout,
        next_barrier_id,
    )
    .await?;

    let release_session = Arc::new(Notify::new());
    let event_chan = event_tx.clone();
    let pos = current_pos.clone();
    let release_session_clone = release_session.clone();
    let ei_task = libei_event_handler(
        ei_event_stream,
        context,
        event_chan,
        release_session_clone,
        pos,
    );

    let portal_task = async {
        let mut activated = input_capture.receive_activated().await?;
        let mut active_capture: Option<(Activated, Position)> = None;

        loop {
            tokio::select! {
                activated = activated.next(), if active_capture.is_none() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    if activated.session_handle().as_str() != current_session {
                        log::debug!(
                            "ignoring activation for other session {} (current {current_session})",
                            activated.session_handle(),
                        );
                        continue;
                    }

                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => id,
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => {
                            let Some(cursor_position) = activated.cursor_position() else {
                                log::warn!("ignoring activation without a barrier id or cursor position");
                                release_unmapped_capture(input_capture, session, &activated).await?;
                                continue;
                            };
                            let Some(barrier_id) = find_corresponding_client(
                                &barrier_configuration.barriers,
                                cursor_position,
                            ) else {
                                log::warn!("ignoring activation while no barriers are configured");
                                release_unmapped_capture(input_capture, session, &activated).await?;
                                continue;
                            };
                            barrier_id
                        }
                    };

                    let Some(&pos) = barrier_configuration.pos_for_barrier_id.get(&barrier_id) else {
                        log::warn!("ignoring activation for stale barrier id {barrier_id}");
                        release_unmapped_capture(input_capture, session, &activated).await?;
                        continue;
                    };
                    current_pos.replace(Some(pos));

                    let cursor = portal_cursor_position(activated.cursor_position());
                    let normalized_cursor = cursor.and_then(|cursor| {
                        normalize_cursor_in_layout(&barrier_configuration.layout, cursor)
                    });
                    event_tx
                        .send((pos, CaptureEvent::Begin {
                            cursor,
                            normalized_cursor,
                        }))
                        .await
                        .expect("no channel");
                    active_capture = Some((activated, pos));
                }
                release = release_capture.recv() => {
                    let release = release.ok_or(CaptureError::CaptureUpdatesClosed)?;
                    if active_capture.is_some() {
                        log::debug!("capture release requested");
                        release_active_capture(
                            input_capture,
                            session,
                            &mut active_capture,
                            &current_pos,
                            release.warp_target,
                        ).await?;
                    } else {
                        log::debug!("capture release requested while capture is inactive");
                    }
                    let _ = release.completion.send(());
                }
                _ = release_session.notified() => {
                    log::debug!("EIS device change requires session recreation");
                    let interrupted_pos = active_capture.as_ref().map(|(_, pos)| *pos);
                    if let Err(error) = release_active_capture(
                        input_capture,
                        session,
                        &mut active_capture,
                        &current_pos,
                        None,
                    ).await {
                        log::warn!("failed to release active capture before session recreation: {error}");
                    }
                    emit_forced_auto_release(event_tx, interrupted_pos).await;
                    break Ok(CaptureSessionExit::Recreate);
                }
                _ = cancellation_token.cancelled() => {
                    log::debug!("capture session termination requested");
                    if let Err(error) = release_active_capture(
                        input_capture,
                        session,
                        &mut active_capture,
                        &current_pos,
                        None,
                    ).await {
                        log::warn!("failed to release active capture during shutdown: {error}");
                    }
                    break Ok(CaptureSessionExit::Terminate);
                }
                changed = zones_changed.next() => {
                    let changed = changed.ok_or(CaptureError::ZonesChangedClosed)?;
                    if !should_reconfigure_zones(
                        &current_session,
                        barrier_configuration.zone_set,
                        changed.session_handle().as_str(),
                        changed.zone_set(),
                    ) {
                        log::debug!(
                            "ignoring stale or unrelated zone update: session={}, zone_set={:?}; current session={current_session}, zone_set={}",
                            changed.session_handle(),
                            changed.zone_set(),
                            barrier_configuration.zone_set,
                        );
                        continue;
                    }

                    let interrupted_pos = active_capture.as_ref().map(|(_, pos)| *pos);
                    let release_result = release_active_capture(
                        input_capture,
                        session,
                        &mut active_capture,
                        &current_pos,
                        None,
                    ).await;
                    // A zone rebuild releases the compositor-side capture
                    // without an explicit request from the outer capture
                    // task. Tell it to flush held keys, send Leave, and reset
                    // its active handle even if the portal release failed and
                    // this session is about to be discarded.
                    emit_forced_auto_release(event_tx, interrupted_pos).await;
                    release_result?;
                    if needs_gnome_session_recreate_workaround() {
                        log::debug!("recreating session for GNOME barrier-update workaround");
                        break Ok(CaptureSessionExit::Recreate);
                    }
                    log::debug!(
                        "reconfiguring barriers in-place after zone update {:?}",
                        changed.zone_set(),
                    );
                    barrier_configuration = configure_barriers(
                        input_capture,
                        session,
                        active_clients,
                        display_layout,
                        next_barrier_id,
                    ).await?;
                }
                event = capture_event.recv() => {
                    let event = event.ok_or(CaptureError::CaptureUpdatesClosed)?;
                    log::debug!("capture event: {event:?}");
                    if !apply_client_update(active_clients, event) {
                        log::debug!("capture event did not change the configured client edges");
                        continue;
                    }

                    let interrupted_pos = active_capture.as_ref().map(|(_, pos)| *pos);
                    let release_result = release_active_capture(
                        input_capture,
                        session,
                        &mut active_capture,
                        &current_pos,
                        None,
                    ).await;
                    emit_forced_auto_release(event_tx, interrupted_pos).await;
                    release_result?;
                    if needs_gnome_session_recreate_workaround() {
                        log::debug!("recreating session for GNOME barrier-update workaround");
                        break Ok(CaptureSessionExit::Recreate);
                    }
                    barrier_configuration = configure_barriers(
                        input_capture,
                        session,
                        active_clients,
                        display_layout,
                        next_barrier_id,
                    ).await?;
                }
            }
        }
    };

    tokio::select! {
        result = ei_task => {
            log::warn!("libei exited; recreating capture session: {result:?}");
            // The EIS stream can disappear while the portal still owns an
            // active barrier. Closing the session gives the local pointer
            // back, but the outer task must also end its network epoch.
            emit_forced_auto_release(event_tx, current_pos.take()).await;
            Ok(CaptureSessionExit::Recreate)
        }
        result = portal_task => result,
    }
}

async fn emit_forced_auto_release(
    event_tx: &Sender<(Position, CaptureEvent)>,
    interrupted_pos: Option<Position>,
) {
    if let Some(pos) = interrupted_pos {
        log::debug!("capture at {pos} was interrupted by portal reconfiguration");
        if event_tx
            .send((pos, CaptureEvent::AutoRelease))
            .await
            .is_err()
        {
            log::debug!("outer capture task exited before forced release was delivered");
        }
    }
}

async fn release_active_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_capture: &mut Option<(Activated, Position)>,
    current_pos: &Cell<Option<Position>>,
    warp_target: Option<(i32, i32)>,
) -> Result<(), CaptureError> {
    let Some((activated, pos)) = active_capture.take() else {
        return Ok(());
    };

    current_pos.set(None);
    release_capture(input_capture, session, activated, pos, warp_target).await
}

async fn release_unmapped_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: &Activated,
) -> Result<(), CaptureError> {
    let cursor_position = activated
        .cursor_position()
        .map(|(x, y)| (x as f64, y as f64));
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(cursor_position);
    input_capture.release(session, release_options).await?;
    Ok(())
}

async fn release_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: Activated,
    current_pos: Position,
    warp_target: Option<(i32, i32)>,
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let fallback_position = activated.cursor_position().map(|(x, y)| {
        log::debug!("client entered @ ({x}, {y})");
        let (dx, dy) = match current_pos {
            // Offset the cursor so it does not immediately enter again.
            Position::Left => (1., 0.),
            Position::Right => (-1., 0.),
            Position::Top => (0., 1.),
            Position::Bottom => (0., -1.),
        };
        (x as f64 + dx, y as f64 + dy)
    });
    let cursor_position = warp_target
        .map(|(x, y)| (f64::from(x), f64::from(y)))
        .or(fallback_position);
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(cursor_position);
    input_capture.release(session, release_options).await?;
    Ok(())
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> Option<BarrierID> {
    barriers
        .iter()
        .copied()
        .min_by_key(|b| {
            let (x1, y1, x2, y2) = b.position;
            let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            distance_to_line(((x1, y1), (x2, y2)), pos) as i32
        })
        .map(|barrier| barrier.barrier_id)
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

/// Whether the running portal backend needs the full session-recreate dance
/// described in the GNOME/mutter note at the top of this file.
///
/// Hyprland routinely adds and removes captured devices as part of normal EIS
/// lifecycle, so applying this workaround there creates a resource-heavy loop.
/// `XDG_CURRENT_DESKTOP` may be colon-separated (for example `ubuntu:GNOME`).
fn needs_gnome_session_recreate_workaround() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_ascii_lowercase().split(':').any(|s| s == "gnome"))
            .unwrap_or(false)
    })
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<Position>,
    context: &ei::Context,
    event_tx: &Sender<(Position, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Touch
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) => {
            // Unlike an individual device replacement, a removed seat
            // invalidates the capability binding itself. No future keyboard
            // or pointer can be attached to it, so every compositor needs a
            // fresh portal/EIS generation here.
            log::debug!("EIS seat removed; recreating capture session: {ei_event:?}");
            // There is exactly one portal-session consumer. `notify_one`
            // retains a permit if it is temporarily busy releasing/rebuilding
            // barriers; `notify_waiters` would lose this lifecycle edge when
            // no `notified()` future is currently inside the select.
            release_session.notify_one();
        }
        /* EiEvent::DeviceAdded(_) | */
        EiEvent::DeviceRemoved(_) => {
            if needs_gnome_session_recreate_workaround() {
                log::debug!(
                    "releasing session (GNOME/mutter device-change workaround): {ei_event:?}"
                );
                release_session.notify_one();
            } else {
                // wlroots/Hyprland/etc.: a device coming or going is
                // normal EIS lifecycle, not a reason to rebuild the
                // whole session. Recreating here would loop and leak
                // fds in the compositor. Keep the session as-is.
                log::debug!("ignoring device change (no session recreate needed): {ei_event:?}");
            }
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)));
        }
        _ => {
            if let Some(pos) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    event_tx
                        .send((pos, CaptureEvent::Input(event)))
                        .await
                        .expect("no channel");
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl MousehopInputCapture for LibeiInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Create(pos))
            .await;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        let _ = self
            .notify_capture
            .send(LibeiNotifyEvent::Destroy(pos))
            .await;
        Ok(())
    }

    async fn release(&mut self, warp_target: Option<(i32, i32)>) -> Result<(), CaptureError> {
        let (completion, completed) = oneshot::channel();
        self.release_capture
            .send(ReleaseRequest {
                warp_target,
                completion,
            })
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)?;
        completed
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        let task = &mut self.capture_task;
        log::debug!("waiting for capture to terminate...");
        let res = if !task.is_finished() {
            task.await.expect("libei task panic")
        } else {
            Ok(())
        };
        self.terminated = true;
        log::debug!("done!");
        res
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
        self.display_layout
            .lock()
            .expect("display layout mutex poisoned")
            .clone()
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            /* this workaround is needed until async drop is stabilized */
            panic!("LibeiInputCapture dropped without being terminated!");
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.capture_task.poll_unpin(cx) {
            Poll::Ready(r) => match r.expect("failed to join") {
                Ok(()) => Poll::Ready(None),
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            Poll::Pending => self.event_rx.poll_recv(cx).map(|e| e.map(Result::Ok)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "/org/freedesktop/portal/desktop/session/1_1/current";
    const OTHER_SESSION: &str = "/org/freedesktop/portal/desktop/session/1_1/other";

    #[test]
    fn zone_updates_only_reconfigure_the_current_session() {
        assert!(should_reconfigure_zones(SESSION, 10, SESSION, Some(10)));
        assert!(should_reconfigure_zones(SESSION, 10, SESSION, Some(11)));
        assert!(should_reconfigure_zones(SESSION, 10, SESSION, None));

        assert!(!should_reconfigure_zones(
            SESSION,
            10,
            OTHER_SESSION,
            Some(10),
        ));
        assert!(!should_reconfigure_zones(SESSION, 10, SESSION, Some(9)));
    }

    #[test]
    fn zone_set_ordering_handles_wraparound() {
        assert!(zone_set_is_current_or_newer(u32::MAX - 1, 0));
        assert!(!zone_set_is_current_or_newer(1, u32::MAX));
    }

    #[test]
    fn duplicate_client_updates_do_not_reconfigure_barriers() {
        let mut active_clients = vec![Position::Right];

        assert!(!apply_client_update(
            &mut active_clients,
            LibeiNotifyEvent::Create(Position::Right),
        ));
        assert!(apply_client_update(
            &mut active_clients,
            LibeiNotifyEvent::Create(Position::Left),
        ));
        assert!(apply_client_update(
            &mut active_clients,
            LibeiNotifyEvent::Destroy(Position::Right),
        ));
        assert!(!apply_client_update(
            &mut active_clients,
            LibeiNotifyEvent::Destroy(Position::Right),
        ));
        assert_eq!(active_clients, vec![Position::Left]);
    }

    const LINUX_PORTAL_REGIONS: [(i32, i32, u32, u32); 3] = [
        (0, 0, 3072, 1728),
        (-1024, 0, 1024, 600),
        (836, 1728, 1280, 360),
    ];

    #[test]
    fn portal_layout_preserves_exact_stepped_linux_topology() {
        let layout = portal_display_layout_from_rects(LINUX_PORTAL_REGIONS);

        assert_eq!(layout.origin(), Some((-1024, 0)));
        assert_eq!(layout.size(), Some((4096, 2088)));
        assert_eq!(
            layout
                .rectangles()
                .map(|(index, rect)| (index, rect.origin(), rect.size()))
                .collect::<Vec<_>>(),
            vec![
                (0, (0, 0), (3072, 1728)),
                (1, (-1024, 0), (1024, 600)),
                (2, (836, 1728), (1280, 360)),
            ]
        );
    }

    #[test]
    fn portal_barriers_follow_exposed_stepped_linux_contour() {
        let layout = portal_display_layout_from_rects(LINUX_PORTAL_REGIONS);
        let clients = [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ];
        let mut next_barrier_id = NonZeroU32::new(1).expect("non-zero id");
        let (barriers, positions_by_id) = select_barriers(&layout, &clients, &mut next_barrier_id);

        assert_eq!(
            barriers
                .iter()
                .map(|barrier| barrier.position)
                .collect::<Vec<_>>(),
            vec![
                // Left contour: left display, primary, then lower display.
                (-1024, 0, -1024, 599),
                (0, 600, 0, 1727),
                (836, 1728, 836, 2087),
                // Right and bottom use the portal's exclusive coordinate.
                (3072, 0, 3072, 1727),
                (2116, 1728, 2116, 2087),
                // Top contour is split at the output-owner boundary.
                (-1024, 0, -1, 0),
                (0, 0, 3071, 0),
                // Bottom follows both steps around the lower display.
                (-1024, 600, -1, 600),
                (0, 1728, 835, 1728),
                (836, 2088, 2115, 2088),
                (2116, 1728, 3071, 1728),
            ]
        );

        let expected_positions = [
            Position::Left,
            Position::Left,
            Position::Left,
            Position::Right,
            Position::Right,
            Position::Top,
            Position::Top,
            Position::Bottom,
            Position::Bottom,
            Position::Bottom,
            Position::Bottom,
        ];
        assert_eq!(barriers.len(), expected_positions.len());
        for (barrier, expected_position) in barriers.iter().zip(expected_positions) {
            assert_eq!(
                positions_by_id.get(&barrier.barrier_id),
                Some(&expected_position)
            );
        }
    }

    #[test]
    fn portal_layout_ignores_empty_regions_and_point_barriers() {
        let regions = [
            (0, 0, 3072, 1728),
            (0, 0, 0, 1080),
            (0, 0, 1920, 0),
            (5000, 5000, 1, 1),
        ];
        let layout = portal_display_layout_from_rects(regions);

        assert_eq!(layout.len(), 2);
        let mut next_barrier_id = NonZeroU32::new(1).expect("non-zero id");
        let (barriers, _) = select_barriers(
            &DisplayLayout::new([(5000, 5000, 1, 1)]),
            &[
                Position::Left,
                Position::Right,
                Position::Top,
                Position::Bottom,
            ],
            &mut next_barrier_id,
        );
        assert!(barriers.is_empty());
    }

    #[test]
    fn barrier_ids_do_not_alias_across_reconfigurations() {
        let layout = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let mut next_barrier_id = NonZeroU32::new(1).expect("non-zero id");

        let (first, _) = select_barriers(&layout, &[Position::Right], &mut next_barrier_id);
        let (second, _) = select_barriers(&layout, &[Position::Right], &mut next_barrier_id);

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].barrier_id, second[0].barrier_id);
    }

    #[tokio::test]
    async fn active_zone_reconfiguration_notifies_outer_capture() {
        let (event_tx, mut event_rx) = mpsc::channel(1);

        emit_forced_auto_release(&event_tx, Some(Position::Right)).await;

        let (position, event) = event_rx.recv().await.expect("forced release event");
        assert_eq!(position, Position::Right);
        assert!(matches!(event, CaptureEvent::AutoRelease));
    }

    #[tokio::test]
    async fn inactive_zone_reconfiguration_emits_nothing() {
        let (event_tx, mut event_rx) = mpsc::channel(1);

        emit_forced_auto_release(&event_tx, None).await;

        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn device_recreation_notification_survives_before_waiter_registration() {
        let release_session = Notify::new();

        release_session.notify_one();

        tokio::time::timeout(Duration::from_millis(10), release_session.notified())
            .await
            .expect("stored recreation permit");
    }

    #[test]
    fn activated_cursor_is_rounded_and_rejects_non_finite_values() {
        assert_eq!(
            portal_cursor_position(Some((3072.4, 603.6))),
            Some((3072, 604))
        );
        assert_eq!(portal_cursor_position(Some((f32::NAN, 10.0))), None);
        assert_eq!(portal_cursor_position(None), None);
    }
}
