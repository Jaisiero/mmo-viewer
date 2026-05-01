//! Keyboard → GuiCmd translation, called once per macroquad frame.
//!
//! This module is deliberately stateless-ish: the render thread owns a
//! single `InputState` struct and passes a mutable reference in each
//! frame. All the polling goes through macroquad's input API so the
//! behaviour matches exactly what the window manager reports — no
//! crossterm-style tick-based keyup inference.
//!
//! Key map (kept in sync with the terminal client where reasonable):
//!   W / A / S / D    — move fwd / strafe-left / back / strafe-right
//!   Q / E            — yaw (−/+) at 2 rad/s while held
//!   J / K            — light / heavy attack (one-shot)
//!   Space / F        — jump / dodge (one-shot)
//!   L                — block (hold); emits RAISE on press, LOWER on release
//!   + / -            — zoom in / out by 1.25× per press (clamped)
//!   Mouse wheel      — smooth zoom (each notch = 1.1×)
//!   0                — reset zoom to the configured `view_range`
//!   Esc              — quit
//!
//! Action codes match the shard's enum: 1 light, 2 heavy, 3 jump, 4 dodge,
//! 0x10 block_raise, 0x11 block_lower. The viewer doesn't interpret them
//! beyond forwarding; anything state-machinish (client-side prediction
//! FSM, cooldowns) lives in `gameplay.rs` in the CLI and is deliberately
//! out of scope here — a debugger should show what the server reports,
//! not second-guess it.

use macroquad::prelude::*;
use mmo_cli::game_client::{ACTION_BLOCK_LOWER, ACTION_BLOCK_RAISE};

use crate::channels::GuiCmd;

/// Persistent input state between frames.
#[derive(Debug, Default)]
pub struct InputState {
    /// Yaw in radians (CCW from +X), accumulated from Q/E input.
    pub orientation: f32,
    /// Was L held on the previous frame? We only emit block toggles on
    /// the edge — a held key isn't a spam of events.
    pub block_held: bool,
    /// If true, the next poll returns a Quit command exactly once.
    pub quit_requested: bool,
    /// Current world-space view range (passed to the renderer). Starts
    /// at `cfg.view_range` and is mutated by the zoom keys / wheel.
    /// Held here (not in `ViewerConfig`) so the loaded TOML value stays
    /// the "home" the user can snap back to with `0`.
    pub view_range: f32,
    /// Configured starting view range; the `0` key resets `view_range`
    /// to this. Set once on startup.
    pub view_range_home: f32,
}

/// Hard floor / ceiling on `view_range`. The floor stops the view from
/// collapsing to a single pixel; the ceiling keeps the world-space
/// camera from underflowing macroquad's NDC math at extreme zoom-out.
const VIEW_RANGE_MIN: f32 = 4.0;
const VIEW_RANGE_MAX: f32 = 4000.0;
/// Per-press zoom factor for the `+` / `-` keys. 1.25× ≈ 9 presses to
/// halve / double the view from any starting point.
const KEY_ZOOM_FACTOR: f32 = 1.25;
/// Per-wheel-notch zoom factor. Smaller than the keyboard step because
/// wheels are continuous; this gives the user fine control.
const WHEEL_ZOOM_FACTOR: f32 = 1.1;

impl InputState {
    /// Construct with the configured view range as both the current and
    /// the "home" value (the `0` key reset target).
    pub fn with_view_range(initial: f32) -> Self {
        let clamped = initial.clamp(VIEW_RANGE_MIN, VIEW_RANGE_MAX);
        Self {
            view_range: clamped,
            view_range_home: clamped,
            ..Self::default()
        }
    }
}

/// One-shot action codes. Kept as a small set of `u8` constants rather
/// than an enum so the wire encoding is a direct pass-through.
mod action {
    pub const LIGHT: u8 = 1;
    pub const HEAVY: u8 = 2;
    pub const JUMP:  u8 = 3;
    pub const DODGE: u8 = 4;
}

/// Produce the list of GuiCmds generated this frame. Always includes a
/// Move (the net thread rate-limits that to input_hz, so emitting one
/// per frame is fine), followed by zero or more Actions and optionally
/// a Quit.
pub fn poll(state: &mut InputState) -> Vec<GuiCmd> {
    let mut cmds = Vec::with_capacity(4);

    // ── Orientation: accumulate at a fixed angular rate while held ────
    // 2 rad/s feels natural in a top-down debugger and sidesteps the
    // "one keystroke = one 0.05 rad nudge" oddness the terminal client
    // inherited from crossterm's event-driven model.
    let dt = get_frame_time();
    const YAW_SPEED: f32 = 2.0;
    if is_key_down(KeyCode::Q) {
        state.orientation -= YAW_SPEED * dt;
    }
    if is_key_down(KeyCode::E) {
        state.orientation += YAW_SPEED * dt;
    }
    // Keep orientation inside (-π, π] to avoid unbounded growth on long
    // runs; both server and viewer only care about direction, not winding.
    while state.orientation > std::f32::consts::PI {
        state.orientation -= std::f32::consts::TAU;
    }
    while state.orientation < -std::f32::consts::PI {
        state.orientation += std::f32::consts::TAU;
    }

    // ── WASD → world-space (x, z) input vector ────────────────────────
    let mut move_x = 0.0f32;
    let mut move_z = 0.0f32;
    if is_key_down(KeyCode::A) { move_x -= 1.0; }
    if is_key_down(KeyCode::D) { move_x += 1.0; }
    if is_key_down(KeyCode::W) { move_z += 1.0; }
    if is_key_down(KeyCode::S) { move_z -= 1.0; }
    // Diagonals should not double speed — normalise so all eight
    // directions have magnitude 1.
    let len = (move_x * move_x + move_z * move_z).sqrt();
    if len > 0.0 {
        move_x /= len;
        move_z /= len;
    }

    cmds.push(GuiCmd::Move {
        move_x,
        move_z,
        orientation: state.orientation,
        buttons: 0,
    });

    // ── Block: edge-triggered on key state change ─────────────────────
    let block_now = is_key_down(KeyCode::L);
    if block_now && !state.block_held {
        cmds.push(GuiCmd::Action { action_type: ACTION_BLOCK_RAISE });
    } else if !block_now && state.block_held {
        cmds.push(GuiCmd::Action { action_type: ACTION_BLOCK_LOWER });
    }
    state.block_held = block_now;

    // ── One-shot actions: press edge, not held ────────────────────────
    if is_key_pressed(KeyCode::J)     { cmds.push(GuiCmd::Action { action_type: action::LIGHT }); }
    if is_key_pressed(KeyCode::K)     { cmds.push(GuiCmd::Action { action_type: action::HEAVY }); }
    if is_key_pressed(KeyCode::Space) { cmds.push(GuiCmd::Action { action_type: action::JUMP }); }
    if is_key_pressed(KeyCode::F)     { cmds.push(GuiCmd::Action { action_type: action::DODGE }); }

    // ── Zoom: mouse wheel + keyboard. Compose factors first so pressing
    // both `-` and scrolling-down in the same frame still ends up at a
    // sane place; clamp once at the end to keep the window bounded
    // even if `view_range` happens to be out of range from a previous
    // frame's clamp.
    let (_wheel_x, wheel_y) = mouse_wheel();
    let mut zoom_factor = 1.0f32;
    // Wheel up (positive y) zooms IN — view_range shrinks.
    if wheel_y > 0.0 {
        zoom_factor /= WHEEL_ZOOM_FACTOR;
    } else if wheel_y < 0.0 {
        zoom_factor *= WHEEL_ZOOM_FACTOR;
    }
    // `+` lives on KpAdd / Equal (with shift) on most layouts; accept
    // both. Same for `-`. `KeyCode::Equal` covers the unshifted `=`
    // because macroquad reports the physical key, not the produced char.
    if is_key_pressed(KeyCode::KpAdd)
        || is_key_pressed(KeyCode::Equal)
    {
        zoom_factor /= KEY_ZOOM_FACTOR;
    }
    if is_key_pressed(KeyCode::KpSubtract)
        || is_key_pressed(KeyCode::Minus)
    {
        zoom_factor *= KEY_ZOOM_FACTOR;
    }
    if zoom_factor != 1.0 {
        state.view_range = (state.view_range * zoom_factor)
            .clamp(VIEW_RANGE_MIN, VIEW_RANGE_MAX);
    }
    if is_key_pressed(KeyCode::Key0) || is_key_pressed(KeyCode::Kp0) {
        state.view_range = state.view_range_home;
    }

    if is_key_pressed(KeyCode::Escape) || state.quit_requested {
        state.quit_requested = false;
        cmds.push(GuiCmd::Quit);
    }

    cmds
}
