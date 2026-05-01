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
/// Hue cycles through the wheel; saturation is fixed mid-high so the
/// outlines pop on the dark background but don't drown the entities.
/// Alpha left to the caller — usually low so the lines are an
/// overlay, not a wall.
pub fn shard_colour(shard_id: &str) -> (f32, f32, f32) {
    let mut h: u32 = 5381;
    for b in shard_id.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    let hue = (h % 360) as f32 / 360.0;          // 0..1
    let s   = 0.65;
    let v   = 0.95;
    hsv_to_rgb(hue, s, v)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}
