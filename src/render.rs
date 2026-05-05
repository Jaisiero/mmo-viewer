//! Draw the mirrored world to the macroquad window.
//!
//! Two passes per frame:
//!   1. World pass — world-space camera locked to the player, draws a
//!      reference grid, then every known entity, then the player on top
//!      (so the player is never occluded by neighbours sitting on the
//!      same spot).
//!   2. HUD pass — default screen camera, draws a small text block in
//!      the top-left corner with session + self + RTT + entity counts.
//!
//! We render raw server state (no interpolation, no smoothing, no
//! prediction). The viewer is a debugger — if the shard is janky, we
//! want to _see_ the jank.

use std::time::Instant;

use macroquad::prelude::*;

use crate::boundaries::{shard_colour, SharedRegions};
use crate::world::{Entity, World};

// Combat state id → body colour for the self triangle. Values match
// mmo-shard/src/combat.rs `state_id::*`. Unknown ids fall back to the
// neutral "idle" colour rather than an error — the viewer should keep
// running on protocol additions the user hasn't updated us for yet.
fn combat_state_colour(state: u16) -> Color {
    match state {
        0 => Color::new(0.88, 0.90, 1.00, 1.0), // IDLE          — off-white
        1 => Color::new(1.00, 0.45, 0.25, 1.0), // ATTACKING     — orange-red
        2 => Color::new(0.80, 0.45, 0.95, 1.0), // STAGGERED     — purple
        3 => Color::new(0.30, 0.65, 1.00, 1.0), // BLOCKING      — blue
        4 => Color::new(1.00, 0.80, 0.20, 1.0), // GUARD_BREAK   — amber
        5 => Color::new(0.40, 0.90, 1.00, 1.0), // BLOCKING_HIT  — cyan
        _ => Color::new(0.88, 0.90, 1.00, 1.0),
    }
}

/// How long each action's visual "I just pressed this button" flash
/// lasts. Deliberately shorter than the server-side recovery window:
/// the flash is just kinesthetic acknowledgement, not a prediction of
/// the full animation.
fn action_flash_ms(action_type: u8) -> u32 {
    match action_type {
        1 => 200,  // light
        2 => 350,  // heavy
        3 => 300,  // jump
        4 => 250,  // dodge
        0x10 | 0x11 => 120, // block raise/lower — just the toggle tick
        _ => 180,
    }
}

/// Colour for the reference grid lines. Deliberately dim so entities
/// and the player dominate the eye.
const GRID_COLOUR: Color = Color::new(0.12, 0.12, 0.15, 1.0);
const GRID_STEP: f32 = 10.0;
/// Colour for grid lines that fall on an "axis" (multiples of 100
/// units). Slightly brighter — a rough "you are here" anchor.
const GRID_AXIS_COLOUR: Color = Color::new(0.25, 0.25, 0.30, 1.0);

/// Draw one frame. `view_range` is the world-space width covered by
/// the shorter screen dimension; the longer dimension gets
/// proportionally more world visible. The viewer's input layer owns
/// the live value (mouse wheel / `+` / `-` / `0` mutate it), so the
/// renderer takes it explicitly rather than reading the loaded
/// configured value out of `ViewerConfig`.
///
/// `regions` is the shared snapshot of shard rectangles refreshed
/// every few seconds by the boundary-overlay polling task; drawn
/// when `show_boundaries` is on.
pub fn draw(
    world: &World,
    view_range: f32,
    regions: &SharedRegions,
    show_boundaries: bool,
    move_to_target: Option<(f32, f32)>,
) {
    clear_background(Color::new(0.05, 0.05, 0.07, 1.0));

    // During the brief handoff window the server's StateAck stops
    // arriving for ~15-30 ms. `predicted_self_pos` returns the
    // authoritative `self_x/self_z` outside that window and a
    // dead-reckoned extrapolation while it's open, so the camera
    // doesn't visibly freeze mid-step.
    let (self_world_x, self_world_z) = world.predicted_self_pos();
    let self_x = self_world_x as f32;
    let self_z = self_world_z as f32;

    // ── World-space camera ───────────────────────────────────────────
    // Zoom expresses world → NDC: a world length of `range` maps to 2
    // (full NDC span). In macroquad 0.4 a positive y-zoom means "world
    // +y appears lower on screen" (the default 2D convention where the
    // origin is top-left and y grows downward), so to get a top-down
    // map with +z = north = screen-up we need to negate zoom.y. The
    // first version of this file had that sign inverted and W/S moved
    // the grid the wrong way; don't "fix" it back.
    let aspect = screen_width() / screen_height().max(1.0);
    let range_x = view_range * aspect;
    let range_y = view_range;
    let camera = Camera2D {
        target: vec2(self_x, self_z),
        zoom: vec2(2.0 / range_x, -2.0 / range_y),
        ..Default::default()
    };
    set_camera(&camera);

    draw_grid(self_x, self_z, range_x, range_y);
    if show_boundaries {
        draw_shard_boundaries(regions, view_range);
    }
    // Single iteration over the unified entity store. The local player
    // lives in here too (with `is_self = true`, keyed by
    // `persistent_id`), so handoff transitions don't drop+recreate any
    // record and the previous `e.id == world.player_id` filter — which
    // silently hid bots whose persistent IDs collided with the
    // freshly-issued *session* id, then unhid them on the next handoff
    // — is gone.
    for e in world.entities.values() {
        // Skip entities whose position hasn't been confirmed by an
        // EntityMoved (or StateAck for self) yet. Health / state
        // events can land before the first move on a freshly
        // re-broadcast set of entities (the destination shard's
        // first batch after handoff isn't guaranteed to put Move
        // strictly first), and rendering the default (0, 0, 0)
        // would flash the entity off-screen for a frame — the
        // visible "bots disappear and reappear during handoff"
        // effect, since the user is rarely standing at the world
        // origin.
        if !e.has_position {
            continue;
        }
        if e.is_self {
            if world.session_open {
                // Use the dead-reckoned position (same as the camera
                // target) so the triangle and the camera stay locked
                // together during the ~25 ms handoff window. Outside
                // that window `self_x/self_z` are the same as
                // `e.x/e.z` (both updated by the same StateAck).
                draw_self(
                    self_x,
                    self_z,
                    world.self_orientation,
                    e.combat_state,
                    world.self_action_flash,
                );
            }
        } else {
            // During the handoff window, dead-reckon the ghost too
            // so remote bots don't visibly freeze for ~25 ms while
            // the local UDP session swaps shards. `predicted_entity_pos`
            // is identity outside the window — debugger philosophy
            // is preserved for normal frames.
            let (px, pz) = world.predicted_entity_pos(e);
            // Resolve the shard whose region currently contains this
            // entity, so `draw_entity` can paint a coloured outer ring
            // matching the boundary overlay's hue. The lookup is the
            // same `regions` snapshot the boundary draw uses, so the
            // ring colour and the rectangle outline colour stay in
            // sync without any extra protocol field.
            let owning = locate_shard(regions, px, pz);
            draw_entity(e, px, pz, owning.as_deref());
        }
    }

    // Click-to-move target marker. Drawn after entities so it sits on
    // top, before HUD swap so it lives in world space (the line scales
    // with the view, the dot keeps a consistent screen-pixel size). A
    // line from the player to the destination plus a ring at the
    // destination is enough visual feedback to confirm "yes, the click
    // registered, and your bot is heading there"; we deliberately don't
    // path-plan or draw a curve since the server walks straight-line.
    if let Some((tx, tz)) = move_to_target {
        let stroke = (view_range / 200.0).clamp(0.5, 4.0);
        let ring_r = (view_range / 60.0).clamp(0.6, 5.0);
        let target_col = Color::new(0.30, 0.95, 0.55, 0.85); // bright green
        draw_line(self_x, self_z, tx, tz, stroke, target_col);
        draw_circle_lines(tx, tz, ring_r, stroke, target_col);
        draw_circle(tx, tz, ring_r * 0.25, target_col);
    }

    // ── HUD pass ─────────────────────────────────────────────────────
    set_default_camera();
    draw_hud(world, view_range);
}

/// Render each shard region as a rectangle outline in world-space.
/// The line thickness is scaled inversely with `view_range` so the
/// strokes look the same on screen at any zoom level.  Each shard
/// gets a stable hue derived from its `shard_id`; a faint fill is
/// painted inside the rectangle so over-zoomed views still
/// communicate "you are inside shard X".
fn draw_shard_boundaries(regions: &SharedRegions, view_range: f32) {
    // RwLock read failure (poisoned) means the writer panicked — nothing
    // sane to do but skip this frame's overlay; the next refresh will
    // either succeed or stay empty.
    let snapshot = match regions.read() {
        Ok(g) => g,
        Err(_) => return,
    };
    if snapshot.is_empty() {
        return;
    }
    // Scale the stroke so it's ~2 logical pixels wide regardless of
    // the camera's current zoom. The macroquad camera maps `range_y`
    // world units to the window height, so 1 px ≈ range_y /
    // screen_height world units.
    let stroke = (view_range / screen_height().max(1.0)) * 2.0;
    for r in snapshot.iter() {
        let (cr, cg, cb) = shard_colour(&r.shard_id);
        let outline_col = Color::new(cr, cg, cb, 0.85);
        let fill_col    = Color::new(cr, cg, cb, 0.05);
        let w = r.x_max - r.x_min;
        let h = r.z_max - r.z_min;
        // Faint tinted fill — useful when one shard's region is tiny
        // and the outline alone is hard to spot.
        draw_rectangle(r.x_min, r.z_min, w, h, fill_col);
        // The outline is what the user actually reads.
        draw_rectangle_lines(r.x_min, r.z_min, w, h, stroke, outline_col);
    }
}

fn draw_grid(cx: f32, cz: f32, range_x: f32, range_y: f32) {
    // Snap the visible window to the grid so lines stay stable while
    // the camera pans — otherwise lines would jitter by subpixel amounts
    // every frame and the eye catches it immediately.
    let half_x = range_x * 0.5;
    let half_y = range_y * 0.5;
    let x0 = ((cx - half_x) / GRID_STEP).floor() * GRID_STEP;
    let x1 = ((cx + half_x) / GRID_STEP).ceil() * GRID_STEP;
    let z0 = ((cz - half_y) / GRID_STEP).floor() * GRID_STEP;
    let z1 = ((cz + half_y) / GRID_STEP).ceil() * GRID_STEP;

    // Vertical lines (constant x).
    let mut x = x0;
    while x <= x1 {
        let col = if (x.rem_euclid(100.0)).abs() < 0.001 {
            GRID_AXIS_COLOUR
        } else {
            GRID_COLOUR
        };
        draw_line(x, z0, x, z1, 0.05, col);
        x += GRID_STEP;
    }
    // Horizontal lines (constant z).
    let mut z = z0;
    while z <= z1 {
        let col = if (z.rem_euclid(100.0)).abs() < 0.001 {
            GRID_AXIS_COLOUR
        } else {
            GRID_COLOUR
        };
        draw_line(x0, z, x1, z, 0.05, col);
        z += GRID_STEP;
    }
}

/// Resolve the shard whose region currently contains `(x, z)`.
/// Returns `None` if no region matches (entity is outside every
/// known region — shouldn't happen in steady state, but can during
/// a brief window mid-cutover when the regions list is stale).
///
/// Linear scan over the regions vec; cluster size is small (handful
/// of shards) so a `find` is fine and avoids a per-shard hash map.
fn locate_shard(regions: &SharedRegions, x: f32, z: f32) -> Option<String> {
    let regions = regions.read().ok()?;
    regions
        .iter()
        .find(|r| x >= r.x_min && x < r.x_max && z >= r.z_min && z < r.z_max)
        .map(|r| r.shard_id.clone())
}

/// Draw a neighbour entity: a filled red dot in the centre with a
/// larger ring around it whose colour matches the shard claiming
/// authority over the entity (= the shard whose region contains the
/// entity's `(x, z)`). Stable shard→hue mapping comes from
/// `boundaries::shard_colour`, which the boundary-overlay rectangles
/// already use, so the ring matches the rectangle outline colour and
/// the user can identify shard membership at a glance.
///
/// `x` / `z` are passed explicitly so the caller can substitute a
/// dead-reckoned position during the handoff window. Outside the
/// window they're just `e.x` / `e.z`.
///
/// `owning_shard_id` is `None` while the entity sits outside every
/// known region (transient mid-cutover or boundary-poll lag); we
/// fall back to a neutral grey ring so the entity is still visible
/// without falsely claiming any shard.
fn draw_entity(e: &Entity, x: f32, z: f32, owning_shard_id: Option<&str>) {
    /// Inner red dot — kept small so the shard-coloured ring around
    /// it dominates visually while the dot still anchors the entity
    /// position.
    const CORE_R: f32 = 0.5;
    /// Outer shard ring. Bigger than the previous body circle so the
    /// hue is unmistakable even at the bench-D-style densities where
    /// 480 bots share a 30-metre column.
    const RING_R: f32 = 1.1;

    // Inner red dot — kept solid red regardless of HP so the shard
    // ring colour is the primary visual signal. A future iteration
    // could re-encode HP as the dot's saturation if needed; for the
    // current "which shard owns this bot" question, redundancy
    // between dot colour and ring colour is more confusing than
    // helpful.
    let core_col = Color::new(0.95, 0.20, 0.20, 1.0);
    draw_circle(x, z, CORE_R, core_col);

    // Outer ring tinted by the owning shard. `boundaries::shard_colour`
    // is the same hue the boundary-overlay rectangles use, so the
    // ring matches the rectangle stroke for the region the entity
    // sits in. Grey fallback for entities outside every known region.
    let ring_col = match owning_shard_id {
        Some(id) => {
            let (r, g, b) = shard_colour(id);
            Color::new(r, g, b, 1.0)
        }
        None => Color::new(0.55, 0.55, 0.55, 1.0),
    };
    // Ring is a thick stroked circle — the line thickness scales with
    // RING_R so the colour band is wide enough to read at the typical
    // bench-D zoom level (~50-200 unit view_range).
    draw_circle_lines(x, z, RING_R, 0.18, ring_col);

    // Combat-state halo: state id 0 is "idle"; anything non-zero gets
    // a yellow inner stroke just inside the shard ring so state
    // changes still pop without overriding the shard colour.
    if e.combat_state != 0 {
        draw_circle_lines(x, z, CORE_R + 0.15, 0.08, Color::new(1.0, 0.9, 0.2, 1.0));
    }

    // Orientation tick: short line from centre in the facing direction.
    let tx = x + e.orientation.cos() * (RING_R * 1.1);
    let tz = z + e.orientation.sin() * (RING_R * 1.1);
    draw_line(x, z, tx, tz, 0.08, WHITE);
}

/// Draw the player as a triangle with its apex in the facing direction.
/// Bigger than neighbour circles so you can always spot yourself.
///
/// * `combat_state` — server-reported state id (0 idle, 1 attacking, …).
///   Colours the triangle body so block / stagger / attack are legible
///   at a glance without reading the HUD.
/// * `flash` — optional (action_code, started_at). If the action is
///   still within its per-type flash window we overlay an outer pulsing
///   ring so actions that don't change `combat_state` (jump, dodge)
///   still get visible feedback.
fn draw_self(
    x: f32,
    z: f32,
    orientation: f32,
    combat_state: u16,
    flash: Option<(u8, Instant)>,
) {
    const L: f32 = 1.5;
    const W: f32 = 1.0;
    let c = orientation.cos();
    let s = orientation.sin();

    // Apex: L units ahead of the player in the facing direction.
    let p0 = vec2(x + c * L, z + s * L);
    // Rear-left and rear-right — offset by ±π/2 from facing.
    let p1 = vec2(
        x - c * (L * 0.4) - s * W,
        z - s * (L * 0.4) + c * W,
    );
    let p2 = vec2(
        x - c * (L * 0.4) + s * W,
        z - s * (L * 0.4) - c * W,
    );

    let body = combat_state_colour(combat_state);
    draw_triangle(p0, p1, p2, body);
    draw_triangle_lines(p0, p1, p2, 0.15, WHITE);

    // ── Action flash ─────────────────────────────────────────────────
    // Ring whose radius grows and alpha fades over the flash window.
    // Colour keyed to the action type so light vs heavy vs dodge vs
    // jump stay distinguishable even on the same frame they're sent.
    if let Some((action, started)) = flash {
        let dur_ms = action_flash_ms(action) as f32;
        let elapsed_ms = started.elapsed().as_secs_f32() * 1000.0;
        if elapsed_ms < dur_ms {
            let t = elapsed_ms / dur_ms; // 0 → 1
            let radius = L * (1.0 + t * 1.5);
            let alpha = 1.0 - t;
            let ring_col = match action {
                1 => Color::new(1.00, 0.45, 0.25, alpha), // light
                2 => Color::new(1.00, 0.15, 0.15, alpha), // heavy
                3 => Color::new(0.70, 1.00, 0.40, alpha), // jump
                4 => Color::new(0.50, 0.85, 1.00, alpha), // dodge
                0x10 => Color::new(0.30, 0.65, 1.00, alpha), // block raise
                0x11 => Color::new(0.30, 0.45, 0.80, alpha), // block lower
                _    => Color::new(1.00, 1.00, 1.00, alpha),
            };
            draw_circle_lines(x, z, radius, 0.12, ring_col);
        }
    }
}

fn draw_hud(world: &World, view_range: f32) {
    const PAD: f32 = 8.0;
    const LINE: f32 = 18.0;
    const FONT: f32 = 16.0;

    let status_line = format!("status: {}", world.status);
    let session_line = if world.session_open {
        format!(
            "shard {}  player {}  tick {}  rtt {} ms",
            world.shard_id, world.player_id, world.server_tick, world.rtt_ms
        )
    } else {
        String::from("not in session")
    };
    let pos_line = if world.session_open {
        format!(
            "pos ({:.1}, {:.1}, {:.1})   origin ({:.1}, {:.1})",
            world.self_x, world.self_y, world.self_z, world.origin_x, world.origin_z
        )
    } else {
        String::new()
    };
    let vitals_line = if world.session_open {
        format!(
            "hp {}   stamina {:.1}   entities {}",
            world.self_hp,
            world.self_stamina,
            world.entities.len()
        )
    } else {
        String::new()
    };
    let fps_line = format!(
        "fps {:>3}   view_range {:.0}",
        get_fps(),
        view_range
    );
    let rejection_line = world
        .last_rejection
        .map(|r| format!("last action rejected: reason={r}"))
        .unwrap_or_default();

    let mut y = PAD + FONT;
    for line in [
        status_line.as_str(),
        session_line.as_str(),
        pos_line.as_str(),
        vitals_line.as_str(),
        fps_line.as_str(),
        rejection_line.as_str(),
    ] {
        if line.is_empty() {
            continue;
        }
        draw_text(line, PAD, y, FONT, Color::new(0.85, 0.9, 1.0, 1.0));
        y += LINE;
    }

    // Keybind reference in the bottom-left, dim grey.
    let hints = [
        "W/A/S/D move   Q/E yaw",
        "J light  K heavy  Space jump  F dodge",
        "L block (hold)   Esc quit",
    ];
    let mut hy = screen_height() - PAD - LINE * (hints.len() as f32 - 1.0) - FONT;
    for h in hints {
        draw_text(h, PAD, hy, FONT, Color::new(0.4, 0.45, 0.55, 1.0));
        hy += LINE;
    }
}
