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

    if is_key_pressed(KeyCode::Escape) || state.quit_requested {
        state.quit_requested = false;
        cmds.push(GuiCmd::Quit);
    }

    cmds
}
