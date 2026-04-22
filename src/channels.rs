//! Wire between the macroquad render thread and the tokio network thread.
//!
//! macroquad owns the OS main thread and runs its own single-threaded
//! executor, so tokio can't live alongside it — we spawn a dedicated OS
//! thread with its own `tokio::runtime::Runtime` and talk to it through
//! two crossbeam channels:
//!
//!   GUI  --GuiCmd-->  NET
//!   GUI  <-NetEvent-- NET
//!
//! Both are unbounded to keep the API trivially non-blocking on both
//! sides; the render loop drains events with `try_recv` each frame and
//! the net loop does the same for commands in between polling the UDP
//! transport.

use crossbeam_channel::{unbounded, Receiver, Sender};

/// Messages flowing from the network thread to the render thread. These
/// are a deliberately narrow projection of `GameClient`'s event stream —
/// only the fields we actually visualise survive the boundary, so the
/// renderer stays ignorant of protocol details.
///
/// `dead_code` is allowed at the enum level: some fields (notably the
/// `damage` in `HitConfirm`) aren't consumed by the current renderer but
/// are forwarded verbatim from the wire so future HUD work can show
/// them without another round-trip through `net.rs`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// Connection attempt progress (visible in the HUD while we wait).
    Status(String),

    /// The shard accepted our JWT; we are now "in game".
    SessionOpened {
        player_id: u32,
        persistent_id: u32,
        origin_x: f64,
        origin_z: f64,
        shard_id: u32,
    },

    /// The shard rejected our auth. Render thread shows the code + quits.
    AuthFailed { reason: u8 },

    /// Periodic state ack for our own player (position, hp, stamina).
    StateAck {
        server_tick: u32,
        x: f64,
        y: f64,
        z: f64,
        hp: u32,
        stamina: f64,
    },

    /// A neighbour entity moved.
    EntityMoved {
        entity_id: u32,
        x: f32,
        y: f32,
        z: f32,
        orientation: f32,
    },

    /// Damage landed on some entity (us or another).
    HitConfirm {
        target_id: u32,
        damage: u32,
        target_hp: u32,
    },

    /// Full hp broadcast (max hp known, good for bar rendering).
    EntityHealth {
        entity_id: u32,
        hp: u32,
        max_hp: u32,
    },

    /// Combat state change (idle / windup / active / blocking / ...).
    EntityStateChanged {
        entity_id: u32,
        state_id: u16,
        param_a: u32,
    },

    /// Server rejected an action (out of stamina, out of range, ...).
    ActionRejected { reason: u8 },

    /// UDP channel closed by the shard.
    Disconnected,

    /// Viewer-internal: round-trip time sample in milliseconds.
    RttSample(u32),
}

/// Messages flowing from the render thread to the network thread.
#[derive(Debug, Clone)]
pub enum GuiCmd {
    /// Movement input expressed as a unit vector in world space plus
    /// current facing (radians, CCW from +X) and a button bitmask. The
    /// net thread packs this into a `PlayerMove` at input_hz cadence.
    Move {
        move_x: f32,
        move_z: f32,
        orientation: f32,
        buttons: u32,
    },

    /// One-shot action (light attack, heavy attack, jump, dodge,
    /// block_raise, block_lower). Codes match the protocol constants.
    Action { action_type: u8 },

    /// Viewer is shutting down. Net thread should issue a clean Stop
    /// to the shard so we don't linger in the session table until
    /// timeout.
    Quit,
}

/// Handles returned from `make_channels()`. Owned by the render thread;
/// `tx` is cloned into the net thread via `NetChannels`.
pub struct GuiChannels {
    pub net_events: Receiver<NetEvent>,
    pub gui_cmds: Sender<GuiCmd>,
}

/// Handles owned by the net thread.
pub struct NetChannels {
    pub net_events: Sender<NetEvent>,
    pub gui_cmds: Receiver<GuiCmd>,
}

pub fn make_channels() -> (GuiChannels, NetChannels) {
    let (ev_tx, ev_rx) = unbounded::<NetEvent>();
    let (cmd_tx, cmd_rx) = unbounded::<GuiCmd>();
    (
        GuiChannels {
            net_events: ev_rx,
            gui_cmds: cmd_tx,
        },
        NetChannels {
            net_events: ev_tx,
            gui_cmds: cmd_rx,
        },
    )
}
