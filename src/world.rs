//! Mirrored world state, updated from `NetEvent` and read by `render.rs`.
//!
//! This is intentionally a plain struct-with-maps, not an ECS. We're
//! debugging a network client — total entity count will stay well below
//! a few hundred (AOI radius on the shard is small) and the renderer
//! iterates the map once per frame. Any more sophistication would just
//! obscure the "what arrived from the wire?" question the viewer is
//! supposed to answer at a glance.

use std::collections::HashMap;
use std::time::Instant;

use crate::channels::NetEvent;

/// A single entity the viewer knows about. All fields come directly from
/// protocol events; nothing is interpolated — the renderer uses these
/// raw values so we can see the server's truth, not a smoothed picture.
///
/// The local player is stored here too, keyed by `persistent_id`, with
/// `is_self = true`. Storing self alongside ghosts means handoff
/// transitions don't have to drop+recreate any entity record (the
/// previous "wipe self_x/y/z + repopulate" cycle is gone), and the old
/// session-vs-persistent ID confusion in the render filter — which was
/// silently hiding any bot whose persistent ID happened to collide with
/// the freshly-issued session id, then unhiding it on the next handoff
/// — disappears at the source.
#[derive(Debug, Clone)]
pub struct Entity {
    pub id: u32,
    /// World-space X (cross-shard coord, i.e. origin-relative).
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub orientation: f32,
    pub hp: u32,
    pub max_hp: u32,
    /// Combat state id as defined by the game (we don't decode the enum;
    /// the renderer just tints by id so you can eyeball transitions).
    pub combat_state: u16,
    pub combat_param: u32,
    /// Last wall-clock time we received any event for this entity. Used
    /// to age out dead entities after a timeout — the shard doesn't send
    /// an explicit "despawn" for AOI exits, so this is the only cue.
    pub last_seen: Instant,
    /// True for the local player's own record (the one populated from
    /// `StateAck`). The renderer draws self as a triangle and others as
    /// circles; pruning skips self so a few hundred milliseconds without
    /// a `StateAck` (handoff window) doesn't wipe the camera target.
    pub is_self: bool,
    /// True once we've received an `EntityMoved` (or `StateAck` for
    /// self) that placed this entity at a real world position. Other
    /// updates (`EntityHealth`, `EntityStateChanged`, `HitConfirm`)
    /// can arrive before the first move on a fresh shard — for a
    /// reconnect after handoff, the destination's first batch may
    /// emit Health or State for an entity in a different order than
    /// Move, causing a one-frame flash at (0, 0, 0) which the user
    /// observes as the "bots disappear and reappear during handoff"
    /// glitch (the flash is off-screen if you're not at the world
    /// origin). The renderer skips entities whose position is not
    /// yet set; the missed Health/State update gets stamped onto the
    /// entity once the position update lands and any subsequent
    /// Health/State delta from the server.
    pub has_position: bool,
}

impl Entity {
    fn new(id: u32) -> Self {
        Self {
            id,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            orientation: 0.0,
            hp: 0,
            max_hp: 1,
            combat_state: 0,
            combat_param: 0,
            last_seen: Instant::now(),
            is_self: false,
            has_position: false,
        }
    }

    /// HP as a fraction in [0, 1] for tinting. Guards against zero max_hp
    /// (freshly created entities where only a Move has arrived so far).
    pub fn hp_frac(&self) -> f32 {
        if self.max_hp == 0 {
            0.0
        } else {
            (self.hp as f32 / self.max_hp as f32).clamp(0.0, 1.0)
        }
    }
}

/// Everything the renderer needs. The `entities` map holds **all**
/// players we know about, including the local one (keyed by
/// `persistent_id`, with `is_self = true`). HUD-only scalars
/// (`self_x/y/z/hp/stamina`) shadow the local entity's position +
/// vitals so the HUD and camera don't have to do a HashMap lookup
/// every frame; they're refreshed from the same `StateAck` that
/// updates the entity record, so they never drift.
#[derive(Debug)]
pub struct World {
    pub connected: bool,
    pub status: String,
    pub session_open: bool,
    pub player_id: u32,
    pub persistent_id: u32,
    pub shard_id: u32,
    pub origin_x: f64,
    pub origin_z: f64,
    pub server_tick: u32,

    /// Our own authoritative position (from StateAck). Mirrored from
    /// the `is_self` entity in `entities` for the HUD and camera —
    /// see `apply` for the StateAck handler that keeps both in sync.
    pub self_x: f64,
    pub self_y: f64,
    pub self_z: f64,
    pub self_hp: u32,
    pub self_stamina: f64,
    /// HUD-only: most recent RTT in ms.
    pub rtt_ms: u32,
    pub last_rejection: Option<u8>,

    /// Client-tracked inputs for the local player. Written every frame
    /// from `InputState`, read by the renderer. Not part of the wire
    /// protocol: the server doesn't echo our orientation back in a way
    /// the viewer can reliably pick up (StateAck omits it), and we know
    /// exactly what we sent, so we trust the client for drawing.
    pub self_orientation: f32,

    /// Most recent PlayerAction we emitted (code, when). Mirror of the
    /// `is_self` entity's `action_flash`, kept on `World` for the input
    /// loop's convenience; the renderer reads from the entity record.
    pub self_action_flash: Option<(u8, Instant)>,

    pub entities: HashMap<u32, Entity>,
}

impl Default for World {
    fn default() -> Self {
        Self {
            connected: false,
            status: String::from("booting"),
            session_open: false,
            player_id: 0,
            persistent_id: 0,
            shard_id: 0,
            origin_x: 0.0,
            origin_z: 0.0,
            server_tick: 0,
            self_x: 0.0,
            self_y: 0.0,
            self_z: 0.0,
            self_hp: 0,
            self_stamina: 0.0,
            rtt_ms: 0,
            last_rejection: None,
            self_orientation: 0.0,
            self_action_flash: None,
            entities: HashMap::new(),
        }
    }
}

impl World {
    /// Apply one event from the network thread. Keeps all decode logic
    /// in one place so `render.rs` never touches `NetEvent` directly.
    pub fn apply(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Status(s) => self.status = s,

            NetEvent::SessionOpened {
                player_id,
                persistent_id,
                origin_x,
                origin_z,
                shard_id,
            } => {
                self.connected = true;
                self.session_open = true;
                self.player_id = player_id;
                self.persistent_id = persistent_id;
                self.origin_x = origin_x;
                self.origin_z = origin_z;
                self.shard_id = shard_id;
                self.status = format!("session open — shard {shard_id}");

                // Defensive: clear any stale `is_self` flag from a
                // previous identity. Normal handoffs preserve
                // `persistent_id` so this is a no-op, but if we ever
                // reconnect as a different user (or the wire payload
                // disagrees), the old self record reverts to being
                // drawn as a regular entity until pruning catches it.
                for (id, e) in self.entities.iter_mut() {
                    if e.is_self && *id != persistent_id {
                        e.is_self = false;
                    }
                }
            }

            NetEvent::AuthFailed { reason } => {
                self.status = format!("auth failed (reason={reason})");
                self.session_open = false;
            }

            NetEvent::StateAck {
                server_tick,
                x,
                y,
                z,
                hp,
                stamina,
            } => {
                // StateAck and EntityMove arrive in *wire space* — coordinates
                // relative to the current shard's origin (see
                // `entanglement-server::session::world_to_wire`). Each shard
                // has its own origin; transforming to world space here means
                // the renderer can use a single coordinate frame across
                // handoffs (the alternative is to retransform every entity
                // every frame, or live with a `dest.origin - src.origin`
                // jump at every shard boundary).
                self.server_tick = server_tick;
                let world_x = x + self.origin_x;
                let world_y = y;
                let world_z = z + self.origin_z;
                self.self_x = world_x;
                self.self_y = world_y;
                self.self_z = world_z;
                self.self_hp = hp;
                self.self_stamina = stamina;

                // Mirror the local player into the unified `entities`
                // store keyed by `persistent_id`. This is the change
                // that kills the "stationary bots flicker on/off across
                // handoffs" bug: the previous renderer filtered out any
                // entity whose id matched `world.player_id` (the
                // *session* id, ephemeral and reissued each shard), so
                // bots whose persistent IDs happened to collide with
                // the freshly-issued session id were silently hidden
                // until the next handoff swapped the colliding id. With
                // self stored as a normal entity (and the filter gone
                // in `render.rs`), no other entity is ever skipped.
                //
                // We only insert once we actually have a `persistent_id`
                // (set by SessionOpened). Before that there's nothing
                // to key on; the `self_*` scalars cover the bootstrap
                // window for the HUD.
                if self.persistent_id != 0 {
                    let e = self.entities
                        .entry(self.persistent_id)
                        .or_insert_with(|| Entity::new(self.persistent_id));
                    e.is_self = true;
                    e.x = world_x as f32;
                    e.y = world_y as f32;
                    e.z = world_z as f32;
                    e.hp = hp;
                    e.max_hp = e.max_hp.max(hp).max(1);
                    e.last_seen = Instant::now();
                    e.has_position = true;
                }
            }

            NetEvent::EntityMoved {
                entity_id,
                x,
                y,
                z,
                orientation,
            } => {
                // The local player's authoritative position lives on
                // `StateAck`; if the server happens to also broadcast
                // self via EntityMove (some configs do), don't let it
                // double-write here — the two sources update at
                // slightly different cadences and would race. We still
                // refresh `last_seen` so pruning doesn't misfire.
                if entity_id == self.persistent_id {
                    if let Some(e) = self.entities.get_mut(&entity_id) {
                        e.last_seen = Instant::now();
                    }
                    return;
                }
                let e = self
                    .entities
                    .entry(entity_id)
                    .or_insert_with(|| Entity::new(entity_id));
                // Wire-to-world transform: see the StateAck branch.
                // Entity fields are f32 (render-friendly); origin is
                // f64 (matches the wire / session). Cast happens here
                // once per update; precision loss is irrelevant at the
                // sub-metre scale we render.
                e.x = x + self.origin_x as f32;
                e.y = y;
                e.z = z + self.origin_z as f32;
                e.orientation = orientation;
                e.last_seen = Instant::now();
                e.has_position = true;
            }

            NetEvent::EntityHealth {
                entity_id,
                hp,
                max_hp,
            } => {
                let e = self
                    .entities
                    .entry(entity_id)
                    .or_insert_with(|| Entity::new(entity_id));
                e.hp = hp;
                e.max_hp = max_hp.max(1);
                e.last_seen = Instant::now();
            }

            NetEvent::EntityStateChanged {
                entity_id,
                state_id,
                param_a,
            } => {
                let e = self
                    .entities
                    .entry(entity_id)
                    .or_insert_with(|| Entity::new(entity_id));
                e.combat_state = state_id;
                e.combat_param = param_a;
                e.last_seen = Instant::now();
            }

            NetEvent::HitConfirm {
                target_id,
                damage: _,
                target_hp,
            } => {
                if let Some(e) = self.entities.get_mut(&target_id) {
                    e.hp = target_hp;
                    e.last_seen = Instant::now();
                }
                // Wire `target_id` is `entity_id` (persistent_id), NOT
                // the per-shard session id. Comparing against
                // `world.player_id` (the session id) was the
                // long-standing bug that left `self_hp` stuck on the
                // initial value across hits.
                if target_id == self.persistent_id {
                    self.self_hp = target_hp;
                }
            }

            NetEvent::ActionRejected { reason } => {
                self.last_rejection = Some(reason);
            }

            NetEvent::Disconnected => {
                self.connected = false;
                self.session_open = false;
                self.status = String::from("disconnected");
            }

            NetEvent::RttSample(ms) => self.rtt_ms = ms,
        }
    }

    /// Server-reported combat state for the local player, or 0 (idle)
    /// if we haven't received an EntityState for ourselves yet. Kept
    /// out of the raw `self_*` fields so we only have one source of
    /// truth: the entities map, which is what the renderer already
    /// iterates for neighbours.
    pub fn self_combat_state(&self) -> u16 {
        // Look up the local player's `is_self` record (keyed by
        // `persistent_id`, not the per-shard `player_id`). The previous
        // version keyed on `self.player_id` and silently returned 0
        // because `entities` is keyed on `entity_id` (persistent_id) —
        // session ids and persistent ids are different namespaces and
        // collide only by accident.
        self.entities
            .get(&self.persistent_id)
            .map(|e| e.combat_state)
            .unwrap_or(0)
    }

    /// Drop entities we haven't heard from in `stale_secs`. Called every
    /// frame; cheap because the set is small.
    ///
    /// `is_self` records are explicitly preserved: a 100 ms handoff
    /// window briefly stops both `StateAck` and `EntityMove` for the
    /// local player while the new UDP session opens, and pruning self
    /// would wipe the camera target right when the user is mid-cross.
    /// The local player record is owned by `SessionOpened` (insert) and
    /// never expires implicitly.
    pub fn prune_stale(&mut self, stale_secs: f32) {
        let now = Instant::now();
        self.entities.retain(|_, e| {
            e.is_self
                || now.duration_since(e.last_seen).as_secs_f32() < stale_secs
        });
    }
}
