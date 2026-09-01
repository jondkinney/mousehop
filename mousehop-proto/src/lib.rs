use input_event::{
    ClipboardEvent, Event as InputEvent, KeyboardEvent, PointerEvent,
    display::{DisplayLayout, DisplayRect},
};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// Defines the maximum size a fixed-buffer encoded event can take up.
/// Clipboard and display-topology events use their dedicated variable-length
/// codecs; every other event fits in this size. Transactional input adds a
/// serial and an inner event tag to the largest legacy input frame.
pub const MAX_EVENT_SIZE: usize =
    size_of::<u8>() + size_of::<u32>() + size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// Maximum total clipboard payload size on the wire (originator
/// fingerprint + content + length prefixes). 4 KiB is conservative
/// against typical UDP MTU.
pub const MAX_CLIPBOARD_SIZE: usize = 4 * 1024;

/// Version of the variable-length [`ProtoEvent::DisplayLayout`] wire payload.
pub const DISPLAY_LAYOUT_VERSION: u8 = 1;

/// Maximum display rectangles carried in one topology datagram.
pub const MAX_DISPLAY_RECTS: usize = 64;

/// Largest encoded topology frame: tag + version + sender epoch + generation + count + 64 rectangles,
/// each represented as `(i32 x, i32 y, u32 width, u32 height)`. At 1039 bytes
/// this remains below a conservative 1200-byte UDP/DTLS payload budget.
pub const MAX_DISPLAY_LAYOUT_SIZE: usize = 15 + MAX_DISPLAY_RECTS * 16;
const _: () = assert!(MAX_DISPLAY_LAYOUT_SIZE < 1200);

/// Legacy/default [`ProtoEvent::Leave`] mode. The peer is expected to
/// take over with its own `Enter` + [`ProtoEvent::CursorPos`], so the
/// receiver must release capture without applying a competing host warp.
///
/// Older mousehop versions always sent zero. Keeping zero's current
/// handover behavior prevents a new receiver from reintroducing the cursor
/// warp race when it is paired with an older sender.
pub const LEAVE_HANDOVER: u32 = 0;

/// Explicit [`ProtoEvent::Leave`] mode for a one-way return. The sender has
/// only an EnterOnly capture at that edge, so no `Enter` +
/// [`ProtoEvent::CursorPos`] will follow. The receiver should apply its
/// modeled host warp when releasing capture.
pub const LEAVE_RELEASE_ONLY: u32 = 1;

/// 8-byte protocol magic identifying a mousehop peer, carried in
/// every [`ProtoEvent::Hello`]. The `Hello` is exchanged right after
/// the DTLS handshake authenticates; a peer that fails to present
/// this exact magic within the handshake window is not a mousehop
/// instance and has its connection refused. mousehop is deliberately
/// **not** wire-compatible with lan-mouse or any other fork — change
/// this magic to force a hard break against a future divergence.
pub const PROTOCOL_MAGIC: [u8; 8] = *b"MOUSEHOP";

/// Peer supports the session-scoped, retransmittable
/// [`ProtoEvent::HandoverEnter`] handshake.
///
/// Capability bits are appended to [`ProtoEvent::Hello`]. An older peer
/// decodes the legacy Hello prefix and ignores this trailing field; a current
/// decoder treats a legacy 17-byte Hello as having zero capabilities.
pub const CAP_ATOMIC_HANDOVER: u32 = 1 << 0;

/// Peer scopes input and cleanup to an atomic-handover serial. This adds
/// retry-safe leave acknowledgement, explicit ownership-loss recovery, and a
/// handover acknowledgement that reports whether the requested cursor warp
/// was actually applied.
pub const CAP_TRANSACTIONAL_HANDOVER: u32 = 1 << 1;

/// Capabilities implemented completely by this protocol build.
pub const PROTOCOL_CAPABILITIES: u32 = CAP_ATOMIC_HANDOVER | CAP_TRANSACTIONAL_HANDOVER;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// host-input state does not exist
    #[error("invalid host-input state: `{0}`")]
    InvalidHostInputState(#[from] TryFromPrimitiveError<HostInputState>),
    /// transactional handover acknowledgement has an unknown warp result
    #[error("invalid handover warp status: `{0}`")]
    InvalidHandoverWarpStatus(#[from] TryFromPrimitiveError<HandoverWarpStatus>),
    /// clipboard payload exceeds [`MAX_CLIPBOARD_SIZE`]
    #[error("clipboard payload too large: {0} bytes")]
    ClipboardTooLarge(usize),
    /// clipboard text is not valid UTF-8
    #[error("invalid UTF-8 in clipboard payload")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    /// display-layout payload uses a wire version this build cannot decode
    #[error("unsupported display-layout wire version: {0}")]
    UnsupportedDisplayLayoutVersion(u8),
    /// display-layout payload claims more rectangles than the bounded codec
    #[error("invalid display-layout rectangle count: {0}")]
    InvalidDisplayRectCount(usize),
    /// display-layout payload contains an empty or overflowing rectangle
    #[error("invalid display-layout rectangle at index {0}")]
    InvalidDisplayRectangle(usize),
    /// display-layout payload exceeds [`MAX_DISPLAY_LAYOUT_SIZE`]
    #[error("display-layout payload too large: {0} bytes")]
    DisplayLayoutTooLarge(usize),
    /// display-layout payload has bytes beyond the declared rectangle count
    #[error("display-layout payload has trailing data")]
    DisplayLayoutTrailingData,
    /// fixed-size event has a non-canonical or truncated wire length
    #[error("invalid {event_type:?} event length: got {actual}, expected {expected}")]
    InvalidEventLength {
        event_type: EventType,
        actual: usize,
        expected: &'static str,
    },
    /// atomic handover serial zero is reserved for the legacy Enter/Ack flow
    #[error("atomic handover serial must be non-zero")]
    InvalidHandoverSerial,
    /// optional-value discriminator is neither zero nor one
    #[error("invalid optional-value flag: {0}")]
    InvalidOptionalFlag(u8),
    /// normalized handover landing point is not finite and inside 0..=1
    #[error("invalid handover cross-axis fraction: {0}")]
    InvalidCrossFraction(f32),
    /// transactional input contains a control-plane event instead of input
    #[error("invalid transactional input event type: {0:?}")]
    InvalidTransactionalInput(EventType),
    /// not enough bytes left in the buffer
    #[error("buffer too small for protocol payload")]
    BufferTooSmall,
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

/// Confirmed input availability of the capturing host. This is sent only for
/// OS state the sender can verify; connection loss is represented locally by
/// the receiver as "unavailable", never guessed on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum HostInputState {
    Unlocked = 0,
    Locked = 1,
}

/// Result of the optional cursor warp in a transactional handover.
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum HandoverWarpStatus {
    NotRequested = 0,
    Applied = 1,
    Unsupported = 2,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main mousehop protocol event type
#[derive(Clone, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// Notify a client that the cursor left its region. The payload is one
    /// of [`LEAVE_HANDOVER`] or [`LEAVE_RELEASE_ONLY`]. Unknown values are
    /// reserved for future release modes.
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Display geometry of the receiving device. Sent by the
    /// emulation side immediately before the [`ProtoEvent::Ack`] of
    /// an [`ProtoEvent::Enter`] so the capturing peer can model the
    /// guest cursor's position along the entry axis. Width and
    /// height are in pixels of the union of all displays on the
    /// emulating device.
    Bounds { width: u32, height: u32 },
    /// Absolute cursor warp on the receiving device. Sent by the
    /// capturing peer after [`ProtoEvent::Enter`] so the guest's
    /// cursor lands at the position that visually corresponds to
    /// where the user's physical cursor was at the moment of
    /// crossing. `x` and `y` are pixel coordinates in the receiver's
    /// screen space, computed by the capturing peer using its own
    /// display bounds and the receiver-supplied [`ProtoEvent::Bounds`]
    /// from a prior Enter.
    MotionAbsolute { x: i32, y: i32 },
    /// Self-sufficient counterpart to [`ProtoEvent::MotionAbsolute`].
    /// Carries the host's cursor position normalized to the host's
    /// own display bounds (0..1 along each axis) plus the entry
    /// side from the receiver's frame. The receiver scales nx/ny
    /// against its own bounds and pins the on-axis dimension to
    /// the entry edge, eliminating the bootstrap problem where
    /// MotionAbsolute couldn't be sent on the first crossing
    /// because the host had no cached peer geometry.
    CursorPos { pos: Position, nx: f32, ny: f32 },
    /// Protocol handshake. Sent by the connect side immediately
    /// after the DTLS connection authenticates — retransmitted until
    /// the peer echoes one back — and mirrored by the listen side.
    /// `magic` must equal [`PROTOCOL_MAGIC`]; a peer that does not
    /// present a valid `Hello` within the handshake window has its
    /// connection refused. This is the deliberate hard cut-over that
    /// keeps mousehop from silently half-interoperating with
    /// lan-mouse. `commit` is the 8-byte ASCII short commit hash
    /// from `shadow_rs`'s `SHORT_COMMIT`, surfaced in the GUI as the
    /// peer's build. Construct via [`ProtoEvent::hello`].
    Hello {
        magic: [u8; 8],
        commit: [u8; 8],
        /// Feature bitmap. Appended after the legacy 17-byte Hello prefix.
        capabilities: u32,
    },
    /// The receiver's per-pair motion-sensitivity multiplier.
    /// Sent by the emulating peer immediately before the
    /// [`ProtoEvent::Ack`] of an [`ProtoEvent::Enter`] so the
    /// capturing peer can scale its wall-press auto-release model
    /// to match. Without this, a sensitivity multiplier below 1.0
    /// would make the host's model accumulate "wall pressure"
    /// faster than the receiver's actual cursor moves, firing
    /// AutoRelease before the cursor has reached the edge. Old
    /// peers that don't recognize the event type silently skip it
    /// per the existing forward-compat handling.
    ReceiverSensitivity { mouse_sensitivity: f64 },
    /// Clipboard text content propagated from the originating peer.
    /// `from_fingerprint` is the TLS fingerprint of the peer that
    /// originally read the clipboard (not necessarily the sender —
    /// intermediate peers preserve the originator field when they
    /// fan-out to other peers). The receiver uses it to short-circuit
    /// the N-peer forwarding loop along with a recent-content cache.
    /// `content` is the clipboard text. Encoded with the variable-
    /// length [`encode_clipboard_event`] / [`decode_clipboard_event`]
    /// helpers; the fixed-buffer codec panics on this variant.
    Clipboard {
        from_fingerprint: String,
        content: String,
    },
    /// Confirmed lock/unlock state of the capturing host. A locked sender has
    /// already released capture; an unlocked event is informational and does
    /// not resume forwarding without a fresh edge crossing.
    HostInputState {
        state: HostInputState,
        generation: u32,
    },
    /// Receiver confirmation that a [`ProtoEvent::HostInputState`] datagram
    /// was processed. DTLS authenticates the peer but remains unreliable over
    /// UDP, so the sender retries recovery state until this arrives.
    HostInputStateAck {
        state: HostInputState,
        generation: u32,
    },
    /// Full logical monitor topology of the emulating device. Sent after
    /// legacy [`ProtoEvent::Bounds`] and before [`ProtoEvent::Ack`] so the
    /// capturing peer initializes its real-contour cursor model before
    /// Ack-gated input is released. The variable-length frame is bounded by
    /// [`MAX_DISPLAY_RECTS`] and carries an explicit format `version` plus a
    /// sender-process `epoch` and wrapping monotonic `generation` used to
    /// reject reordered hotplug snapshots across both reconnects and peer
    /// restarts.
    DisplayLayout {
        version: u8,
        epoch: u64,
        generation: u32,
        layout: DisplayLayout,
    },
    /// Atomic, retry-safe ownership transfer. `serial` is non-zero and unique
    /// within the authenticated DTLS session; [`ProtoEvent::Ack`] carrying the
    /// same value confirms that the receiver released local capture and
    /// applied the optional cursor landing. `cross_fraction` is normalized
    /// along the entry edge. `None` explicitly means that the capture backend
    /// could not report a cursor and no warp should occur.
    HandoverEnter {
        serial: u32,
        pos: Position,
        cross_fraction: Option<f32>,
    },
    /// Input owned by one completed atomic handover. A receiver accepts it
    /// only while the exact `serial` owns this authenticated transport.
    HandoverInput { serial: u32, event: InputEvent },
    /// Transactional counterpart to [`ProtoEvent::Ack`]. `warp` reports
    /// whether the receiver applied, did not need, or could not support the
    /// requested absolute warp.
    HandoverAck {
        serial: u32,
        warp: HandoverWarpStatus,
    },
    /// Retry-safe, serial-scoped cleanup. `mode` has the same meaning as the
    /// payload of legacy [`ProtoEvent::Leave`].
    HandoverLeave { serial: u32, mode: u32 },
    /// Confirms that a [`ProtoEvent::HandoverLeave`] was applied or found to be
    /// stale relative to a newer handover.
    HandoverLeaveAck { serial: u32 },
    /// The receiver no longer owns `serial` (for example after its heartbeat
    /// watchdog released held keys). The sender must release an exactly
    /// matching active capture instead of continuing to send into a void.
    OwnershipLost { serial: u32 },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Bounds { width, height } => write!(f, "Bounds({width}x{height})"),
            ProtoEvent::MotionAbsolute { x, y } => write!(f, "MotionAbsolute({x}, {y})"),
            ProtoEvent::CursorPos { pos, nx, ny } => {
                write!(f, "CursorPos({pos}, {nx:.4}, {ny:.4})")
            }
            ProtoEvent::ReceiverSensitivity { mouse_sensitivity } => {
                write!(f, "ReceiverSensitivity({mouse_sensitivity:.2})")
            }
            ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                if *magic == PROTOCOL_MAGIC {
                    write!(f, "Hello({s}, capabilities=0x{capabilities:08x})")
                } else {
                    write!(f, "Hello(foreign:{s}, capabilities=0x{capabilities:08x})")
                }
            }
            ProtoEvent::Clipboard {
                from_fingerprint,
                content,
            } => {
                let head: String = content.chars().take(40).collect();
                let preview = if head.len() < content.len() {
                    format!("{head}…")
                } else {
                    head
                };
                write!(
                    f,
                    "Clipboard(from={}…, {}b: {preview})",
                    &from_fingerprint[..from_fingerprint.len().min(8)],
                    content.len(),
                )
            }
            ProtoEvent::HostInputState { state, generation } => {
                write!(f, "HostInputState({state:?}, generation={generation})")
            }
            ProtoEvent::HostInputStateAck { state, generation } => {
                write!(f, "HostInputStateAck({state:?}, generation={generation})")
            }
            ProtoEvent::DisplayLayout {
                version,
                epoch,
                generation,
                layout,
            } => {
                write!(
                    f,
                    "DisplayLayout(v{version}, epoch={epoch}, generation={generation}, {} rects)",
                    layout.len()
                )
            }
            ProtoEvent::HandoverEnter {
                serial,
                pos,
                cross_fraction,
            } => match cross_fraction {
                Some(fraction) => {
                    write!(f, "HandoverEnter({serial}, {pos}, {fraction:.4})")
                }
                None => write!(f, "HandoverEnter({serial}, {pos}, no-warp)"),
            },
            ProtoEvent::HandoverInput { serial, event } => {
                write!(f, "HandoverInput({serial}, {event})")
            }
            ProtoEvent::HandoverAck { serial, warp } => {
                write!(f, "HandoverAck({serial}, warp={warp:?})")
            }
            ProtoEvent::HandoverLeave { serial, mode } => {
                write!(f, "HandoverLeave({serial}, mode={mode})")
            }
            ProtoEvent::HandoverLeaveAck { serial } => {
                write!(f, "HandoverLeaveAck({serial})")
            }
            ProtoEvent::OwnershipLost { serial } => write!(f, "OwnershipLost({serial})"),
        }
    }
}

#[derive(Clone, Copy, TryFromPrimitive, IntoPrimitive, Debug)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Bounds,
    MotionAbsolute,
    CursorPos,
    Hello,
    ReceiverSensitivity,
    /// Variable-length clipboard frame; not decodable through the
    /// fixed-size [`MAX_EVENT_SIZE`] buffer path. See
    /// [`decode_clipboard_event`].
    Clipboard,
    /// Fixed-size host lock/unlock state. Appended after Clipboard so every
    /// existing event tag remains wire-stable for older peers.
    HostInputState,
    /// Acknowledgement for [`EventType::HostInputState`].
    HostInputStateAck,
    /// Variable-length full monitor topology. Appended so existing event tags
    /// remain wire-stable and old peers simply skip the unknown datagram.
    DisplayLayout,
    /// Atomic Enter + optional cursor landing handshake. Appended so all
    /// legacy tags remain wire-stable.
    HandoverEnter,
    /// Serial-scoped input frame carrying one nested legacy input payload.
    HandoverInput,
    /// Atomic handover acknowledgement with cursor-warp outcome.
    HandoverAck,
    /// Serial-scoped, retry-safe leave.
    HandoverLeave,
    /// Acknowledgement for [`EventType::HandoverLeave`].
    HandoverLeaveAck,
    /// Receiver reports that a handover serial no longer owns input.
    OwnershipLost,
}

fn input_event_wire_len(event_type: EventType) -> Option<usize> {
    match event_type {
        EventType::PointerMotion => Some(21),
        EventType::PointerButton => Some(13),
        EventType::PointerAxis => Some(15),
        EventType::PointerAxisValue120 => Some(6),
        EventType::KeyboardKey => Some(10),
        EventType::KeyboardModifiers => Some(17),
        _ => None,
    }
}

fn decode_input_payload(
    event_type: EventType,
    buf: &mut &[u8],
) -> Result<InputEvent, ProtocolError> {
    match event_type {
        EventType::PointerMotion => Ok(InputEvent::Pointer(PointerEvent::Motion {
            time: decode_u32(buf)?,
            dx: decode_f64(buf)?,
            dy: decode_f64(buf)?,
        })),
        EventType::PointerButton => Ok(InputEvent::Pointer(PointerEvent::Button {
            time: decode_u32(buf)?,
            button: decode_u32(buf)?,
            state: decode_u32(buf)?,
        })),
        EventType::PointerAxis => Ok(InputEvent::Pointer(PointerEvent::Axis {
            time: decode_u32(buf)?,
            axis: decode_u8(buf)?,
            value: decode_f64(buf)?,
            momentum: decode_u8(buf)? != 0,
        })),
        EventType::PointerAxisValue120 => Ok(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
            axis: decode_u8(buf)?,
            value: decode_i32(buf)?,
        })),
        EventType::KeyboardKey => Ok(InputEvent::Keyboard(KeyboardEvent::Key {
            time: decode_u32(buf)?,
            key: decode_u32(buf)?,
            state: decode_u8(buf)?,
        })),
        EventType::KeyboardModifiers => Ok(InputEvent::Keyboard(KeyboardEvent::Modifiers {
            depressed: decode_u32(buf)?,
            latched: decode_u32(buf)?,
            locked: decode_u32(buf)?,
            group: decode_u32(buf)?,
        })),
        other => Err(ProtocolError::InvalidTransactionalInput(other)),
    }
}

impl ProtoEvent {
    /// Construct a [`ProtoEvent::Hello`] stamped with this build's
    /// [`PROTOCOL_MAGIC`] and the given short commit hash.
    pub fn hello(commit: [u8; 8]) -> Self {
        ProtoEvent::Hello {
            magic: PROTOCOL_MAGIC,
            commit,
            capabilities: PROTOCOL_CAPABILITIES,
        }
    }

    /// Construct a topology event using this build's supported wire version.
    pub fn display_layout(layout: DisplayLayout) -> Self {
        Self::display_layout_generation(layout, 0, 0)
    }

    /// Construct a topology event with a wrapping monotonic generation.
    pub fn display_layout_generation(layout: DisplayLayout, epoch: u64, generation: u32) -> Self {
        Self::DisplayLayout {
            version: DISPLAY_LAYOUT_VERSION,
            epoch,
            generation,
            layout,
        }
    }

    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
                InputEvent::Clipboard(c) => match c {
                    ClipboardEvent::Text(_) => EventType::Clipboard,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Bounds { .. } => EventType::Bounds,
            ProtoEvent::MotionAbsolute { .. } => EventType::MotionAbsolute,
            ProtoEvent::CursorPos { .. } => EventType::CursorPos,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::ReceiverSensitivity { .. } => EventType::ReceiverSensitivity,
            ProtoEvent::Clipboard { .. } => EventType::Clipboard,
            ProtoEvent::HostInputState { .. } => EventType::HostInputState,
            ProtoEvent::HostInputStateAck { .. } => EventType::HostInputStateAck,
            ProtoEvent::DisplayLayout { .. } => EventType::DisplayLayout,
            ProtoEvent::HandoverEnter { .. } => EventType::HandoverEnter,
            ProtoEvent::HandoverInput { .. } => EventType::HandoverInput,
            ProtoEvent::HandoverAck { .. } => EventType::HandoverAck,
            ProtoEvent::HandoverLeave { .. } => EventType::HandoverLeave,
            ProtoEvent::HandoverLeaveAck { .. } => EventType::HandoverLeaveAck,
            ProtoEvent::OwnershipLost { .. } => EventType::OwnershipLost,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            event_type @ (EventType::PointerMotion
            | EventType::PointerButton
            | EventType::PointerAxis
            | EventType::PointerAxisValue120
            | EventType::KeyboardKey
            | EventType::KeyboardModifiers) => {
                Ok(Self::Input(decode_input_payload(event_type, &mut buf)?))
            }
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Bounds => Ok(Self::Bounds {
                width: decode_u32(&mut buf)?,
                height: decode_u32(&mut buf)?,
            }),
            EventType::MotionAbsolute => Ok(Self::MotionAbsolute {
                x: decode_i32(&mut buf)?,
                y: decode_i32(&mut buf)?,
            }),
            EventType::CursorPos => Ok(Self::CursorPos {
                pos: decode_u8(&mut buf)?.try_into()?,
                nx: decode_f32(&mut buf)?,
                ny: decode_f32(&mut buf)?,
            }),
            EventType::Hello => {
                let mut magic = [0u8; 8];
                for b in magic.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello {
                    magic,
                    commit,
                    capabilities: decode_u32(&mut buf)?,
                })
            }
            EventType::ReceiverSensitivity => Ok(Self::ReceiverSensitivity {
                mouse_sensitivity: decode_f64(&mut buf)?,
            }),
            // Clipboard frames are variable-length and never arrive
            // through the fixed-size buffer path; the connect/listen
            // layer routes them through `decode_clipboard_event`.
            EventType::Clipboard => Err(ProtocolError::BufferTooSmall),
            EventType::HostInputState => Ok(Self::HostInputState {
                state: decode_u8(&mut buf)?.try_into()?,
                generation: decode_u32(&mut buf)?,
            }),
            EventType::HostInputStateAck => Ok(Self::HostInputStateAck {
                state: decode_u8(&mut buf)?.try_into()?,
                generation: decode_u32(&mut buf)?,
            }),
            // DisplayLayout frames are variable-length and never arrive
            // through the fixed-size buffer path.
            EventType::DisplayLayout => Err(ProtocolError::BufferTooSmall),
            EventType::HandoverEnter => {
                let serial = decode_u32(&mut buf)?;
                if serial == 0 {
                    return Err(ProtocolError::InvalidHandoverSerial);
                }
                let pos = decode_u8(&mut buf)?.try_into()?;
                let present = decode_u8(&mut buf)?;
                let fraction = decode_f32(&mut buf)?;
                let cross_fraction = match present {
                    0 => None,
                    1 if fraction.is_finite() && (0.0..=1.0).contains(&fraction) => Some(fraction),
                    1 => return Err(ProtocolError::InvalidCrossFraction(fraction)),
                    flag => return Err(ProtocolError::InvalidOptionalFlag(flag)),
                };
                Ok(Self::HandoverEnter {
                    serial,
                    pos,
                    cross_fraction,
                })
            }
            EventType::HandoverInput => {
                let serial = decode_nonzero_handover_serial(&mut buf)?;
                let input_type = EventType::try_from(decode_u8(&mut buf)?)?;
                Ok(Self::HandoverInput {
                    serial,
                    event: decode_input_payload(input_type, &mut buf)?,
                })
            }
            EventType::HandoverAck => Ok(Self::HandoverAck {
                serial: decode_nonzero_handover_serial(&mut buf)?,
                warp: decode_u8(&mut buf)?.try_into()?,
            }),
            EventType::HandoverLeave => Ok(Self::HandoverLeave {
                serial: decode_nonzero_handover_serial(&mut buf)?,
                mode: decode_u32(&mut buf)?,
            }),
            EventType::HandoverLeaveAck => Ok(Self::HandoverLeaveAck {
                serial: decode_nonzero_handover_serial(&mut buf)?,
            }),
            EventType::OwnershipLost => Ok(Self::OwnershipLost {
                serial: decode_nonzero_handover_serial(&mut buf)?,
            }),
        }
    }
}

/// Decode one canonical fixed-size protocol datagram.
///
/// Unlike [`TryFrom<[u8; MAX_EVENT_SIZE]>`], this entry point retains the
/// datagram's received length and rejects both truncated and overlong frames.
/// A legacy 17-byte [`ProtoEvent::Hello`] is the sole exception: its omitted
/// trailing capability bitmap is decoded as zero. Variable-length Clipboard
/// and DisplayLayout frames must use their dedicated decoders.
pub fn decode_fixed_event(bytes: &[u8]) -> Result<ProtoEvent, ProtocolError> {
    let Some(&tag) = bytes.first() else {
        return Err(ProtocolError::BufferTooSmall);
    };
    let event_type = EventType::try_from(tag)?;
    let (length_is_valid, expected) = match event_type {
        EventType::PointerMotion => (bytes.len() == 21, "21 bytes"),
        EventType::PointerButton => (bytes.len() == 13, "13 bytes"),
        EventType::PointerAxis => (bytes.len() == 15, "15 bytes"),
        EventType::PointerAxisValue120 => (bytes.len() == 6, "6 bytes"),
        EventType::KeyboardKey => (bytes.len() == 10, "10 bytes"),
        EventType::KeyboardModifiers => (bytes.len() == 17, "17 bytes"),
        EventType::Ping => (bytes.len() == 1, "1 byte"),
        EventType::Pong | EventType::Enter => (bytes.len() == 2, "2 bytes"),
        EventType::Leave | EventType::Ack => (bytes.len() == 5, "5 bytes"),
        EventType::Bounds | EventType::MotionAbsolute | EventType::ReceiverSensitivity => {
            (bytes.len() == 9, "9 bytes")
        }
        EventType::CursorPos => (bytes.len() == 10, "10 bytes"),
        EventType::Hello => (matches!(bytes.len(), 17 | 21), "17 or 21 bytes"),
        EventType::HostInputState | EventType::HostInputStateAck => (bytes.len() == 6, "6 bytes"),
        EventType::HandoverEnter => (bytes.len() == 11, "11 bytes"),
        EventType::HandoverInput => {
            let valid = bytes
                .get(5)
                .and_then(|tag| EventType::try_from(*tag).ok())
                .and_then(input_event_wire_len)
                .is_some_and(|input_len| bytes.len() == 5 + input_len);
            (
                valid,
                "a 5-byte transaction header plus one canonical input frame",
            )
        }
        EventType::HandoverAck => (bytes.len() == 6, "6 bytes"),
        EventType::HandoverLeave => (bytes.len() == 9, "9 bytes"),
        EventType::HandoverLeaveAck | EventType::OwnershipLost => (bytes.len() == 5, "5 bytes"),
        EventType::Clipboard | EventType::DisplayLayout => {
            (false, "a dedicated variable-length frame")
        }
    };
    if !length_is_valid {
        return Err(ProtocolError::InvalidEventLength {
            event_type,
            actual: bytes.len(),
            expected,
        });
    }

    let mut padded = [0u8; MAX_EVENT_SIZE];
    padded[..bytes.len()].copy_from_slice(bytes);
    padded.try_into()
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis {
                            time,
                            axis,
                            value,
                            momentum,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                            encode_u8(buf, len, momentum as u8);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                    InputEvent::Clipboard(_) => {
                        panic!(
                            "ProtoEvent::Input(Clipboard) cannot use the fixed-buffer \
                             encoder; route via encode_clipboard_event"
                        );
                    }
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Bounds { width, height } => {
                    encode_u32(buf, len, width);
                    encode_u32(buf, len, height);
                }
                ProtoEvent::MotionAbsolute { x, y } => {
                    encode_i32(buf, len, x);
                    encode_i32(buf, len, y);
                }
                ProtoEvent::CursorPos { pos, nx, ny } => {
                    encode_u8(buf, len, pos as u8);
                    encode_f32(buf, len, nx);
                    encode_f32(buf, len, ny);
                }
                ProtoEvent::Hello {
                    magic,
                    commit,
                    capabilities,
                } => {
                    for b in magic.iter() {
                        encode_u8(buf, len, *b);
                    }
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                    encode_u32(buf, len, capabilities);
                }
                ProtoEvent::ReceiverSensitivity { mouse_sensitivity } => {
                    encode_f64(buf, len, mouse_sensitivity);
                }
                ProtoEvent::Clipboard { .. } => {
                    panic!(
                        "ProtoEvent::Clipboard cannot use the fixed-buffer encoder; \
                         route via encode_clipboard_event"
                    );
                }
                ProtoEvent::HostInputState { state, generation }
                | ProtoEvent::HostInputStateAck { state, generation } => {
                    encode_u8(buf, len, state as u8);
                    encode_u32(buf, len, generation);
                }
                ProtoEvent::DisplayLayout { .. } => {
                    panic!(
                        "ProtoEvent::DisplayLayout cannot use the fixed-buffer encoder; \
                         route via encode_display_layout_event"
                    );
                }
                ProtoEvent::HandoverEnter {
                    serial,
                    pos,
                    cross_fraction,
                } => {
                    encode_u32(buf, len, serial);
                    encode_u8(buf, len, pos as u8);
                    encode_u8(buf, len, cross_fraction.is_some() as u8);
                    encode_f32(buf, len, cross_fraction.unwrap_or(0.0));
                }
                ProtoEvent::HandoverInput { serial, event } => {
                    encode_u32(buf, len, serial);
                    let (inner, inner_len): ([u8; MAX_EVENT_SIZE], usize) =
                        ProtoEvent::Input(event).into();
                    for byte in &inner[..inner_len] {
                        encode_u8(buf, len, *byte);
                    }
                }
                ProtoEvent::HandoverAck { serial, warp } => {
                    encode_u32(buf, len, serial);
                    encode_u8(buf, len, warp as u8);
                }
                ProtoEvent::HandoverLeave { serial, mode } => {
                    encode_u32(buf, len, serial);
                    encode_u32(buf, len, mode);
                }
                ProtoEvent::HandoverLeaveAck { serial } | ProtoEvent::OwnershipLost { serial } => {
                    encode_u32(buf, len, serial)
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f32);
decode_impl!(f64);

fn decode_nonzero_handover_serial(data: &mut &[u8]) -> Result<u32, ProtocolError> {
    let serial = decode_u32(data)?;
    if serial == 0 {
        Err(ProtocolError::InvalidHandoverSerial)
    } else {
        Ok(serial)
    }
}

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f32);
encode_impl!(f64);

/// Encode a bounded full-display topology frame.
///
/// Wire format:
/// `[event_type: u8][version: u8][epoch: u64][generation: u32][count: u8][rects: count * 16 bytes]`,
/// where each rectangle is `(i32 x, i32 y, u32 width, u32 height)` in
/// big-endian order. Layouts larger than [`MAX_DISPLAY_RECTS`] are rejected so
/// callers can retain the complete rectangular `Bounds` fallback rather than
/// advertise a partial, incorrect contour.
pub fn encode_display_layout_event(event: &ProtoEvent) -> Result<Vec<u8>, ProtocolError> {
    let (version, epoch, generation, layout) = match event {
        ProtoEvent::DisplayLayout {
            version,
            epoch,
            generation,
            layout,
        } => (*version, *epoch, *generation, layout),
        _ => panic!("encode_display_layout_event called on non-display-layout event"),
    };
    if version != DISPLAY_LAYOUT_VERSION {
        return Err(ProtocolError::UnsupportedDisplayLayoutVersion(version));
    }
    if layout.len() > MAX_DISPLAY_RECTS {
        return Err(ProtocolError::InvalidDisplayRectCount(layout.len()));
    }

    let rects: Vec<DisplayRect> = layout.rectangles().map(|(_, rect)| rect).collect();
    let mut buf = Vec::with_capacity(15 + rects.len() * 16);
    buf.push(EventType::DisplayLayout as u8);
    buf.push(version);
    buf.extend_from_slice(&epoch.to_be_bytes());
    buf.extend_from_slice(&generation.to_be_bytes());
    buf.push(rects.len() as u8);
    for rect in rects {
        let (x, y) = rect.origin();
        let (width, height) = rect.size();
        buf.extend_from_slice(&x.to_be_bytes());
        buf.extend_from_slice(&y.to_be_bytes());
        buf.extend_from_slice(&width.to_be_bytes());
        buf.extend_from_slice(&height.to_be_bytes());
    }
    debug_assert!(buf.len() <= MAX_DISPLAY_LAYOUT_SIZE);
    Ok(buf)
}

/// Decode a topology frame produced by [`encode_display_layout_event`].
/// Unknown versions, excessive counts, invalid rectangles, truncation, and
/// trailing bytes are rejected without allocating from untrusted sizes.
pub fn decode_display_layout_event(buf: &[u8]) -> Result<ProtoEvent, ProtocolError> {
    if buf.len() > MAX_DISPLAY_LAYOUT_SIZE {
        return Err(ProtocolError::DisplayLayoutTooLarge(buf.len()));
    }
    if buf.len() < 15 {
        return Err(ProtocolError::BufferTooSmall);
    }
    if !matches!(EventType::try_from(buf[0])?, EventType::DisplayLayout) {
        return Err(ProtocolError::BufferTooSmall);
    }
    let version = buf[1];
    if version != DISPLAY_LAYOUT_VERSION {
        return Err(ProtocolError::UnsupportedDisplayLayoutVersion(version));
    }
    let epoch = u64::from_be_bytes(buf[2..10].try_into().expect("eight bytes"));
    let generation = u32::from_be_bytes(buf[10..14].try_into().expect("four bytes"));
    let count = usize::from(buf[14]);
    if count > MAX_DISPLAY_RECTS {
        return Err(ProtocolError::InvalidDisplayRectCount(count));
    }
    let expected_len = 15 + count * 16;
    if buf.len() < expected_len {
        return Err(ProtocolError::BufferTooSmall);
    }
    if buf.len() > expected_len {
        return Err(ProtocolError::DisplayLayoutTrailingData);
    }

    let mut data = &buf[15..];
    let mut rects = Vec::with_capacity(count);
    for index in 0..count {
        let x = decode_layout_i32(&mut data)?;
        let y = decode_layout_i32(&mut data)?;
        let width = decode_layout_u32(&mut data)?;
        let height = decode_layout_u32(&mut data)?;
        let rect = DisplayRect::new(x, y, width, height)
            .ok_or(ProtocolError::InvalidDisplayRectangle(index))?;
        rects.push(rect);
    }
    Ok(ProtoEvent::DisplayLayout {
        version,
        epoch,
        generation,
        layout: DisplayLayout::from_rects(rects),
    })
}

fn decode_layout_i32(data: &mut &[u8]) -> Result<i32, ProtocolError> {
    if data.len() < size_of::<i32>() {
        return Err(ProtocolError::BufferTooSmall);
    }
    let (bytes, rest) = data.split_at(size_of::<i32>());
    *data = rest;
    Ok(i32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

fn decode_layout_u32(data: &mut &[u8]) -> Result<u32, ProtocolError> {
    if data.len() < size_of::<u32>() {
        return Err(ProtocolError::BufferTooSmall);
    }
    let (bytes, rest) = data.split_at(size_of::<u32>());
    *data = rest;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

/// Wire format for clipboard frames:
/// `[event_type: u8][fp_len: u32 BE][fp: utf8][text_len: u32 BE][text: utf8]`
///
/// Returns the encoded bytes ready for transmission. The total
/// length is bounded by [`MAX_CLIPBOARD_SIZE`].
pub fn encode_clipboard_event(event: &ProtoEvent) -> Result<Vec<u8>, ProtocolError> {
    let (from_fingerprint, content) = match event {
        ProtoEvent::Clipboard {
            from_fingerprint,
            content,
        } => (from_fingerprint.as_str(), content.as_str()),
        ProtoEvent::Input(InputEvent::Clipboard(ClipboardEvent::Text(content))) => {
            // Convenience: capture-side callers carry only the text;
            // the originator fingerprint is empty until the service
            // layer stamps it in. Phase 2 wires the stamp.
            ("", content.as_str())
        }
        _ => panic!("encode_clipboard_event called on non-clipboard event"),
    };
    let fp_bytes = from_fingerprint.as_bytes();
    let text_bytes = content.as_bytes();
    let total = 1 + 4 + fp_bytes.len() + 4 + text_bytes.len();
    if total > MAX_CLIPBOARD_SIZE {
        return Err(ProtocolError::ClipboardTooLarge(total));
    }
    let mut buf = Vec::with_capacity(total);
    buf.push(EventType::Clipboard as u8);
    buf.extend_from_slice(&(fp_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(fp_bytes);
    buf.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(text_bytes);
    Ok(buf)
}

/// Decode a clipboard frame produced by [`encode_clipboard_event`].
pub fn decode_clipboard_event(buf: &[u8]) -> Result<ProtoEvent, ProtocolError> {
    if buf.len() > MAX_CLIPBOARD_SIZE {
        return Err(ProtocolError::ClipboardTooLarge(buf.len()));
    }
    if buf.is_empty() {
        return Err(ProtocolError::BufferTooSmall);
    }
    let tag = buf[0];
    let event_type = EventType::try_from(tag)?;
    if !matches!(event_type, EventType::Clipboard) {
        // Wrong-type tag in the clipboard channel — treat as a buffer
        // mismatch rather than silently producing some other variant.
        return Err(ProtocolError::BufferTooSmall);
    }
    let mut cursor = 1usize;
    if buf.len() < cursor + 4 {
        return Err(ProtocolError::BufferTooSmall);
    }
    let fp_len = u32::from_be_bytes([
        buf[cursor],
        buf[cursor + 1],
        buf[cursor + 2],
        buf[cursor + 3],
    ]) as usize;
    cursor += 4;
    if buf.len() < cursor + fp_len + 4 {
        return Err(ProtocolError::BufferTooSmall);
    }
    let from_fingerprint = String::from_utf8(buf[cursor..cursor + fp_len].to_vec())?;
    cursor += fp_len;
    let text_len = u32::from_be_bytes([
        buf[cursor],
        buf[cursor + 1],
        buf[cursor + 2],
        buf[cursor + 3],
    ]) as usize;
    cursor += 4;
    if buf.len() < cursor + text_len {
        return Err(ProtocolError::BufferTooSmall);
    }
    let content = String::from_utf8(buf[cursor..cursor + text_len].to_vec())?;
    Ok(ProtoEvent::Clipboard {
        from_fingerprint,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_round_trip() {
        let event = ProtoEvent::Clipboard {
            from_fingerprint: "abcd1234".into(),
            content: "hello, world".into(),
        };
        let bytes = encode_clipboard_event(&event).expect("encode");
        let decoded = decode_clipboard_event(&bytes).expect("decode");
        match decoded {
            ProtoEvent::Clipboard {
                from_fingerprint,
                content,
            } => {
                assert_eq!(from_fingerprint, "abcd1234");
                assert_eq!(content, "hello, world");
            }
            other => panic!("expected Clipboard, got {other}"),
        }
    }

    #[test]
    fn clipboard_too_large_rejected() {
        let event = ProtoEvent::Clipboard {
            from_fingerprint: "fp".into(),
            content: "x".repeat(MAX_CLIPBOARD_SIZE),
        };
        assert!(matches!(
            encode_clipboard_event(&event),
            Err(ProtocolError::ClipboardTooLarge(_))
        ));
    }

    #[test]
    fn clipboard_decode_truncated() {
        // Encode then truncate the trailing content bytes; decoder
        // must surface BufferTooSmall instead of returning a bogus
        // string with random capture from the underlying memory.
        let event = ProtoEvent::Clipboard {
            from_fingerprint: "fp".into(),
            content: "some text".into(),
        };
        let bytes = encode_clipboard_event(&event).expect("encode");
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            decode_clipboard_event(truncated),
            Err(ProtocolError::BufferTooSmall)
        ));
    }

    #[test]
    fn hello_round_trip_carries_magic_and_capabilities() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::hello(*b"abcd1234").into();
        assert_eq!(len, 21);
        match decode_fixed_event(&buf[..len]).expect("decode") {
            ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            } => {
                assert_eq!(magic, PROTOCOL_MAGIC);
                assert_eq!(&commit, b"abcd1234");
                assert_eq!(capabilities, PROTOCOL_CAPABILITIES);
            }
            other => panic!("expected Hello, got {other}"),
        }
    }

    #[test]
    fn hello_wire_prefix_remains_legacy_compatible() {
        let commit = *b"abcd1234";
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::hello(commit).into();

        // Legacy decoders consume only this unchanged 17-byte prefix and
        // ignore the newly appended capability bitmap.
        assert_eq!(len, 21);
        assert_eq!(buf[0], EventType::Hello as u8);
        assert_eq!(&buf[1..9], &PROTOCOL_MAGIC);
        assert_eq!(&buf[9..17], &commit);
    }

    #[test]
    fn legacy_hello_decodes_with_no_capabilities() {
        let (buf, _): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::hello(*b"abcd1234").into();
        match decode_fixed_event(&buf[..17]).expect("decode legacy Hello") {
            ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            } => {
                assert_eq!(magic, PROTOCOL_MAGIC);
                assert_eq!(&commit, b"abcd1234");
                assert_eq!(capabilities, 0);
            }
            other => panic!("expected Hello, got {other}"),
        }
    }

    #[test]
    fn foreign_hello_decodes_but_magic_mismatches() {
        // A Hello from a non-mousehop peer still decodes — the
        // connection layer is what rejects it, on the magic.
        let foreign = ProtoEvent::Hello {
            magic: *b"LAN-MOUS",
            commit: *b"00000000",
            capabilities: 0,
        };
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = foreign.into();
        let decoded = decode_fixed_event(&buf[..len]).expect("decode");
        assert!(!matches!(
            decoded,
            ProtoEvent::Hello { magic, .. } if magic == PROTOCOL_MAGIC
        ));
    }

    #[test]
    fn handover_enter_round_trips_with_and_without_cursor_landing() {
        assert_eq!(EventType::HandoverEnter as u8, 20);
        for cross_fraction in [Some(0.625), None] {
            let event = ProtoEvent::HandoverEnter {
                serial: 47,
                pos: Position::Left,
                cross_fraction,
            };
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
            assert_eq!(len, 11);
            assert_eq!(buf[0], EventType::HandoverEnter as u8);
            assert!(matches!(
                decode_fixed_event(&buf[..len]).expect("decode"),
                ProtoEvent::HandoverEnter {
                    serial: 47,
                    pos: Position::Left,
                    cross_fraction: decoded,
                } if decoded == cross_fraction
            ));
        }
    }

    #[test]
    fn transactional_handover_frames_round_trip_without_moving_legacy_tags() {
        assert_eq!(EventType::HandoverEnter as u8, 20);
        assert_eq!(EventType::HandoverInput as u8, 21);
        assert_eq!(EventType::HandoverAck as u8, 22);
        assert_eq!(EventType::HandoverLeave as u8, 23);
        assert_eq!(EventType::HandoverLeaveAck as u8, 24);
        assert_eq!(EventType::OwnershipLost as u8, 25);
        assert_eq!(
            PROTOCOL_CAPABILITIES & CAP_TRANSACTIONAL_HANDOVER,
            CAP_TRANSACTIONAL_HANDOVER
        );

        let inputs = [
            InputEvent::Pointer(PointerEvent::Motion {
                time: 7,
                dx: -12.5,
                dy: 3.25,
            }),
            InputEvent::Keyboard(KeyboardEvent::Key {
                time: 8,
                key: 30,
                state: 1,
            }),
            InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 1,
                latched: 2,
                locked: 4,
                group: 3,
            }),
        ];
        for expected in inputs {
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::HandoverInput {
                serial: 47,
                event: expected.clone(),
            }
            .into();
            assert!(matches!(
                decode_fixed_event(&buf[..len]).expect("decode transactional input"),
                ProtoEvent::HandoverInput { serial: 47, event } if event == expected
            ));
        }

        for warp in [
            HandoverWarpStatus::NotRequested,
            HandoverWarpStatus::Applied,
            HandoverWarpStatus::Unsupported,
        ] {
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) =
                ProtoEvent::HandoverAck { serial: 47, warp }.into();
            assert!(matches!(
                decode_fixed_event(&buf[..len]).expect("decode transactional Ack"),
                ProtoEvent::HandoverAck { serial: 47, warp: decoded } if decoded == warp
            ));
        }

        for event in [
            ProtoEvent::HandoverLeave {
                serial: 47,
                mode: LEAVE_RELEASE_ONLY,
            },
            ProtoEvent::HandoverLeaveAck { serial: 47 },
            ProtoEvent::OwnershipLost { serial: 47 },
        ] {
            let expected = format!("{event}");
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
            assert_eq!(
                format!(
                    "{}",
                    decode_fixed_event(&buf[..len]).expect("decode transaction control")
                ),
                expected
            );
        }
    }

    #[test]
    fn transactional_frames_reject_zero_serial_and_invalid_warp_status() {
        let event = ProtoEvent::HandoverAck {
            serial: 47,
            warp: HandoverWarpStatus::Applied,
        };
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();

        let mut invalid = buf;
        invalid[1..5].copy_from_slice(&0u32.to_be_bytes());
        assert!(matches!(
            decode_fixed_event(&invalid[..len]),
            Err(ProtocolError::InvalidHandoverSerial)
        ));

        let mut invalid = buf;
        invalid[5] = 3;
        assert!(matches!(
            decode_fixed_event(&invalid[..len]),
            Err(ProtocolError::InvalidHandoverWarpStatus(_))
        ));
    }

    #[test]
    fn handover_enter_rejects_invalid_serial_flag_and_fraction() {
        let event = ProtoEvent::HandoverEnter {
            serial: 47,
            pos: Position::Bottom,
            cross_fraction: Some(0.25),
        };
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();

        let mut invalid = buf;
        invalid[1..5].copy_from_slice(&0u32.to_be_bytes());
        assert!(matches!(
            decode_fixed_event(&invalid[..len]),
            Err(ProtocolError::InvalidHandoverSerial)
        ));

        let mut invalid = buf;
        invalid[6] = 2;
        assert!(matches!(
            decode_fixed_event(&invalid[..len]),
            Err(ProtocolError::InvalidOptionalFlag(2))
        ));

        for fraction in [f32::NAN, -0.01, 1.01] {
            let mut invalid = buf;
            invalid[7..11].copy_from_slice(&fraction.to_be_bytes());
            assert!(matches!(
                decode_fixed_event(&invalid[..len]),
                Err(ProtocolError::InvalidCrossFraction(decoded))
                    if decoded.to_bits() == fraction.to_bits()
            ));
        }
    }

    #[test]
    fn fixed_decoder_rejects_truncated_and_overlong_frames() {
        let (ack, ack_len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ack(7).into();
        assert!(matches!(
            decode_fixed_event(&ack[..ack_len - 1]),
            Err(ProtocolError::InvalidEventLength {
                event_type: EventType::Ack,
                actual: 4,
                ..
            })
        ));
        assert!(matches!(
            decode_fixed_event(&ack[..ack_len + 1]),
            Err(ProtocolError::InvalidEventLength {
                event_type: EventType::Ack,
                actual: 6,
                ..
            })
        ));

        let (handover, handover_len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::HandoverEnter {
            serial: 8,
            pos: Position::Top,
            cross_fraction: None,
        }
        .into();
        assert!(matches!(
            decode_fixed_event(&handover[..handover_len - 1]),
            Err(ProtocolError::InvalidEventLength {
                event_type: EventType::HandoverEnter,
                actual: 10,
                ..
            })
        ));

        let (hello, hello_len): ([u8; MAX_EVENT_SIZE], usize) =
            ProtoEvent::hello(*b"abcd1234").into();
        for invalid_len in [18, 19, 20] {
            assert!(matches!(
                decode_fixed_event(&hello[..invalid_len]),
                Err(ProtocolError::InvalidEventLength {
                    event_type: EventType::Hello,
                    actual,
                    ..
                }) if actual == invalid_len
            ));
        }
        let mut overlong_hello = hello[..hello_len].to_vec();
        overlong_hello.push(0);
        assert!(matches!(
            decode_fixed_event(&overlong_hello),
            Err(ProtocolError::InvalidEventLength {
                event_type: EventType::Hello,
                actual: 22,
                ..
            })
        ));
    }

    #[test]
    fn leave_modes_round_trip() {
        for mode in [LEAVE_HANDOVER, LEAVE_RELEASE_ONLY] {
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Leave(mode).into();
            assert_eq!(len, 1 + size_of::<u32>());
            assert!(matches!(
                buf.try_into().expect("decode"),
                ProtoEvent::Leave(decoded_mode) if decoded_mode == mode
            ));
        }
    }

    #[test]
    fn host_input_states_round_trip_without_shifting_existing_tags() {
        assert_eq!(EventType::Clipboard as u8, 16);
        assert_eq!(EventType::HostInputState as u8, 17);
        assert_eq!(EventType::HostInputStateAck as u8, 18);

        for state in [HostInputState::Unlocked, HostInputState::Locked] {
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::HostInputState {
                state,
                generation: 17,
            }
            .into();
            assert_eq!(len, 1 + size_of::<u8>() + size_of::<u32>());
            assert!(matches!(
                buf.try_into().expect("decode"),
                ProtoEvent::HostInputState { state: decoded, generation: 17 }
                    if decoded == state
            ));
        }
    }

    #[test]
    fn host_input_state_ack_round_trips() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::HostInputStateAck {
            state: HostInputState::Unlocked,
            generation: 23,
        }
        .into();
        assert_eq!(len, 1 + size_of::<u8>() + size_of::<u32>());
        assert!(matches!(
            buf.try_into().expect("decode"),
            ProtoEvent::HostInputStateAck {
                state: HostInputState::Unlocked,
                generation: 23,
            }
        ));
    }

    #[test]
    fn display_layout_round_trip_preserves_negative_origins() {
        assert_eq!(EventType::DisplayLayout as u8, 19);
        let layout = DisplayLayout::new([
            (-1728, 0, 1728, 1117),
            (0, 0, 3072, 1728),
            (836, 1728, 1280, 360),
        ]);
        let event = ProtoEvent::display_layout_generation(layout.clone(), 99, 42);
        let bytes = encode_display_layout_event(&event).expect("encode");
        assert_eq!(bytes.len(), 15 + 3 * 16);

        match decode_display_layout_event(&bytes).expect("decode") {
            ProtoEvent::DisplayLayout {
                version,
                epoch,
                generation,
                layout: decoded,
            } => {
                assert_eq!(version, DISPLAY_LAYOUT_VERSION);
                assert_eq!(epoch, 99);
                assert_eq!(generation, 42);
                assert_eq!(decoded, layout);
            }
            other => panic!("expected DisplayLayout, got {other}"),
        }
    }

    #[test]
    fn display_layout_encoder_rejects_partial_topology() {
        let layout =
            DisplayLayout::new((0..(MAX_DISPLAY_RECTS + 6)).map(|x| (x as i32 * 10, -20, 10, 20)));
        assert!(matches!(
            encode_display_layout_event(&ProtoEvent::display_layout(layout)),
            Err(ProtocolError::InvalidDisplayRectCount(count))
                if count == MAX_DISPLAY_RECTS + 6
        ));
    }

    #[test]
    fn display_layout_decoder_rejects_unknown_version_and_excessive_count() {
        let tag = EventType::DisplayLayout as u8;
        let header = |version, count| [tag, version, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 1, count];
        assert!(matches!(
            decode_display_layout_event(&header(DISPLAY_LAYOUT_VERSION + 1, 0)),
            Err(ProtocolError::UnsupportedDisplayLayoutVersion(_))
        ));
        assert!(matches!(
            decode_display_layout_event(&header(DISPLAY_LAYOUT_VERSION, 65)),
            Err(ProtocolError::InvalidDisplayRectCount(65))
        ));
    }

    #[test]
    fn display_layout_decoder_rejects_malformed_count_and_rectangle() {
        let tag = EventType::DisplayLayout as u8;
        assert!(matches!(
            decode_display_layout_event(&[
                tag,
                DISPLAY_LAYOUT_VERSION,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                9,
                0,
                0,
                0,
                1,
                1,
            ]),
            Err(ProtocolError::BufferTooSmall)
        ));

        let mut invalid_rect = vec![
            tag,
            DISPLAY_LAYOUT_VERSION,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            9,
            0,
            0,
            0,
            1,
            1,
        ];
        invalid_rect.extend_from_slice(&(-10i32).to_be_bytes());
        invalid_rect.extend_from_slice(&(-20i32).to_be_bytes());
        invalid_rect.extend_from_slice(&0u32.to_be_bytes());
        invalid_rect.extend_from_slice(&100u32.to_be_bytes());
        assert!(matches!(
            decode_display_layout_event(&invalid_rect),
            Err(ProtocolError::InvalidDisplayRectangle(0))
        ));
    }
}
