//! Periodic poll of `WorldCoord/ListShards` for the debug boundary
//! overlay.  Owned by the network thread; the result is shared with
//! the render thread through an `Arc<RwLock<Vec<ShardRegion>>>`.
//!
//! Why a separate gRPC client (and not piggyback on `mmo-cli`'s
//! gateway/auth wires): the viewer is the only place that wants the
//! cluster-wide shard list, and adding a `WorldCoord` client to
//! `mmo-cli`'s public API for one debug feature would be the wrong
//! shape.  The dependency footprint is one extra `tonic` connection
//! per viewer, polled every `INTERVAL`.
//!
//! Failure mode: the world-coord URL might be wrong, or world-coord
//! might be down.  The poll just logs a warn and the overlay shows
//! the previous list (or empty if it's never landed).  No
//! reconnect ceremony — `tonic::transport::Channel` already has
//! built-in lazy reconnect.
//!
//! Toggle from the UI: see `input.rs::B` key (it flips a flag the
//! renderer reads; the network thread keeps polling regardless so a
//! re-toggle has data ready instantly).
//!
//! [`shard_id`] is hashed to a stable colour for the rectangle stroke,
//! so a given shard keeps the same colour across the run even as its
//! region resizes during splits/merges.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tonic::transport::Channel;
use tracing::{debug, warn};

use crate::world_coord_proto::world_coord_client::WorldCoordClient;
use crate::world_coord_proto::ListShardsRequest;

/// One shard's region rectangle, in world coordinates.  Cloned into
/// the renderer every frame, so kept tiny.
#[derive(Debug, Clone)]
pub struct ShardRegion {
    pub shard_id: String,
    pub x_min: f32,
    pub x_max: f32,
    pub z_min: f32,
    pub z_max: f32,
}

/// Shared snapshot the renderer reads.  `RwLock` over `Vec` is fine —
/// reads are once per frame, writes are once per `INTERVAL` (3 s).
pub type SharedRegions = Arc<RwLock<Vec<ShardRegion>>>;

pub fn make_shared_regions() -> SharedRegions {
    Arc::new(RwLock::new(Vec::new()))
}

/// Poll cadence.  3 s is enough to track manual splits/merges in real
/// time without hammering world-coord; the regions don't change
/// faster than orchestrator-driven transitions in any case.
const INTERVAL: Duration = Duration::from_secs(3);
/// Per-call deadline.  If world-coord is genuinely down we still want
/// the next tick to fire promptly rather than pile up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Spawn the polling task.  Returns immediately; task lives as long
/// as the runtime does.
pub fn spawn(world_coord_url: String, regions: SharedRegions) {
    tokio::spawn(async move {
        let endpoint = match Channel::from_shared(world_coord_url.clone()) {
            Ok(e) => e
                .connect_timeout(REQUEST_TIMEOUT)
                .timeout(REQUEST_TIMEOUT),
            Err(e) => {
                warn!(url = %world_coord_url, error = %e, "Invalid world_coord_url; boundary overlay disabled");
                return;
            }
        };
        let channel = endpoint.connect_lazy();
        let mut client = WorldCoordClient::new(channel);
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tick.tick().await;
            match client.list_shards(ListShardsRequest {}).await {
                Ok(resp) => {
                    let resp = resp.into_inner();
                    let new_regions: Vec<ShardRegion> = resp
                        .shards
                        .into_iter()
                        .filter_map(|s| {
                            let r = s.region?;
                            Some(ShardRegion {
                                shard_id: s.shard_id,
                                x_min:    r.x_min,
                                x_max:    r.x_max,
                                z_min:    r.z_min,
                                z_max:    r.z_max,
                            })
                        })
                        .collect();
                    debug!(count = new_regions.len(), "ListShards refreshed");
                    if let Ok(mut w) = regions.write() {
                        *w = new_regions;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "ListShards failed; keeping previous overlay");
                }
            }
        }
    });
}

/// Stable-ish hashing colour: same shard_id → same RGB across runs.
///
/// Hand-picked saturated palette of 12 colours visible on the dark
/// background. Constraints:
///   - No yellow — would collide with the combat halo (`render.rs::
///     draw_entity` historically used a yellow ring to mark
///     `combat_state != 0`; even after the halo colour change, leaving
///     yellow out keeps it free for future UI accents).
///   - No pure red — entities already have red outer ring + inner core,
///     a red shard band would camouflage them.
///   - Hues spread ≥30° apart so two random UUIDs are visually
///     distinguishable however the djb2 hash falls.
///
/// Previous version was `hsv_to_rgb(djb2(uuid) % 360 / 360, 0.65, 0.95)`
/// — fully random hues. Production cluster booted three shards with
/// hues 58°, 315°, 328° (yellow indistinguishable from combat halo,
/// magenta indistinguishable from pink), exactly the "mezclados azules
/// con verdes" symptom users reported. The fixed palette eliminates
/// the dice-roll: any cluster of ≤12 shards is guaranteed
/// distinguishable; >12 wraps the palette and adjacent indices in the
/// wrap still differ noticeably.
const SHARD_PALETTE: [(f32, f32, f32); 12] = [
    (0.00, 0.80, 1.00), // 0  — cyan
    (0.30, 0.90, 0.30), // 1  — lime
    (1.00, 0.20, 0.85), // 2  — magenta
    (1.00, 0.55, 0.00), // 3  — orange
    (0.30, 0.70, 1.00), // 4  — sky blue
    (0.20, 0.85, 0.55), // 5  — mint
    (0.60, 0.50, 1.00), // 6  — lavender
    (1.00, 0.40, 0.70), // 7  — pink
    (0.10, 0.60, 0.65), // 8  — teal
    (0.45, 0.55, 0.95), // 9  — periwinkle
    (0.70, 0.30, 0.70), // 10 — plum
    (0.20, 0.95, 0.85), // 11 — aqua
];

pub fn shard_colour(shard_id: &str) -> (f32, f32, f32) {
    let mut h: u32 = 5381;
    for b in shard_id.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    SHARD_PALETTE[(h as usize) % SHARD_PALETTE.len()]
}
