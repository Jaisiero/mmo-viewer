//! Network driver for the viewer.
//!
//! Runs on a dedicated OS thread with its own `tokio::runtime`. The main
//! (macroquad) thread talks to this one via crossbeam channels:
//!
//!   GUI → NET: `GuiCmd`    (latest move input + one-shot actions + quit)
//!   NET → GUI: `NetEvent`  (status + session + per-entity updates)
//!
//! The bootstrap sequence is the same one `mmo-cli` uses (auth-service
//! login → gateway player_connect → GameClient::connect + wait_for_open),
//! and during the hot loop the viewer also handles `SHARD_HANDOFF` the
//! same way the CLI does in `gameplay.rs`: when one arrives we tear down
//! the current `GameClient` and reconnect to the new shard via
//! `connect_with_handoff_auth`. If that target is itself draining we
//! bounce through the gateway with the last known position. There's also
//! a 250 ms grace period on a bare `Disconnected` to catch a late
//! handoff message — same trick the CLI uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mmo_cli::auth_client::AuthServiceClient;
use mmo_cli::game_client::{AuthError, GameClient, GameEvent};
use mmo_cli::gateway_client::GatewayClient;

use tokio::runtime::Builder;

use crate::boundaries::{self, SharedRegions};
use crate::channels::{GuiCmd, NetChannels, NetEvent};
use crate::config::ViewerConfig;

/// Spawn the net thread. Consumes the `NetChannels` end of the bridge;
/// the caller keeps the matching `GuiChannels` for the render loop. The
/// join handle is returned so `main` can wait for a clean shutdown after
/// macroquad's window closes, but in practice we drop it and let the
/// thread exit when its channels hang up.
pub fn spawn(
    cfg:     ViewerConfig,
    ch:      NetChannels,
    regions: SharedRegions,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("mmo-viewer-net".into())
        .spawn(move || {
            let rt = match Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = ch
                        .net_events
                        .send(NetEvent::Status(format!("tokio runtime build failed: {e}")));
                    return;
                }
            };
            rt.block_on(run(cfg, ch, regions));
        })
        .expect("spawn mmo-viewer-net thread")
}

async fn run(cfg: ViewerConfig, ch: NetChannels, regions: SharedRegions) {
    // Boundary-overlay polling task lives on this same runtime: cheap
    // gRPC every 3 s, no need to burn a second OS thread for it. If the
    // URL is empty the user opted out and we just leave the snapshot
    // empty (the renderer will draw nothing for the overlay).
    if !cfg.world_coord_url.is_empty() {
        boundaries::spawn(cfg.world_coord_url.clone(), regions);
    }
    let NetChannels { net_events, gui_cmds } = ch;
    // Small helper to ship a status message without having to clone the
    // sender each time; we use it at every error path so the HUD never
    // goes silent.
    let status = |s: &str| {
        let _ = net_events.send(NetEvent::Status(s.to_string()));
    };

    status("connecting to auth-service…");
    let mut auth = match AuthServiceClient::connect(&cfg.client.auth_service_url).await {
        Ok(c) => c,
        Err(e) => {
            status(&format!("auth-service unreachable: {e}"));
            return;
        }
    };

    // Best-effort register (may already exist) followed by login. Matches
    // what the bench bots do — keeps one-off viewer startup zero-friction.
    let _ = auth.register(&cfg.username, &cfg.password).await;
    status("logging in…");
    let jwt = match auth.login(&cfg.username, &cfg.password).await {
        Ok(t) => t,
        Err(e) => {
            status(&format!("login failed: {e}"));
            return;
        }
    };

    // Prefer the saved spawn from auth-service; fall back to config start.
    let (start_x, start_z) = match auth.load_player_state(&jwt).await {
        Ok(s) if s.found => (s.x as f32, s.z as f32),
        _ => (cfg.start_x, cfg.start_z),
    };

    status("asking gateway for shard assignment…");
    let mut gateway = match GatewayClient::connect(&cfg.client.gateway_url).await {
        Ok(c) => c,
        Err(e) => {
            status(&format!("gateway unreachable: {e}"));
            return;
        }
    };
    let shard = match gateway.player_connect(&jwt, start_x, start_z).await {
        Ok(a) => a,
        Err(e) => {
            status(&format!("gateway rejection: {e}"));
            return;
        }
    };
    status(&format!("connecting to shard {}:{}…", shard.ip, shard.port));

    let mut client = GameClient::new(&cfg.client);
    if let Err(e) = client.connect(&shard.ip, shard.port, &jwt).await {
        status(&format!("shard connect failed: {e}"));
        return;
    }
    if let Err(e) = client.wait_for_session_open().await {
        status(&format!("session auth failed: {e}"));
        return;
    }

    // Publish the opened session so the renderer can switch from "loading"
    // to "playing" and lock the camera to `player_id`. `persistent_id`
    // survives shard handoffs and is what the new shard expects in the
    // HandoffAuth packet, so we capture it here too.
    let persistent_id = client.session.persistent_id;
    let _ = net_events.send(NetEvent::SessionOpened {
        player_id: client.session.player_id,
        persistent_id,
        origin_x: client.session.origin_x,
        origin_z: client.session.origin_z,
        shard_id: client.session.shard_id,
    });

    // ── Hot loop ──────────────────────────────────────────────────────
    //
    // We tick faster than input_hz (every 5 ms) so latency between the
    // user pressing a key and the packet leaving the NIC is dominated by
    // input_hz cadence, not our polling granularity. Each tick:
    //   1. Drain all pending GuiCmds (updates `input` or pushes actions).
    //   2. Poll the GameClient; translate events into NetEvent.
    //   3. If the input-hz clock elapsed, send a PlayerMove reflecting
    //      the latest input.
    //   4. Emit a Ping every 500 ms so the HUD can show RTT.
    //   5. On `ShardHandoffReceived` (or a late one within 250 ms of a
    //      bare `Disconnected`), reconnect to the new shard and keep
    //      going. `gateway` and `jwt` ride into the loop so we can
    //      bounce through the gateway if the target shard is also
    //      draining.
    run_hot_loop(client, net_events.clone(), gui_cmds, &cfg, jwt, persistent_id, gateway).await;
    status("net loop exited");
}

async fn run_hot_loop(
    mut client: GameClient,
    net_events: crossbeam_channel::Sender<NetEvent>,
    gui_cmds: crossbeam_channel::Receiver<GuiCmd>,
    cfg: &ViewerConfig,
    jwt: String,
    persistent_id: u32,
    mut gateway: GatewayClient,
) {
    #[derive(Default, Clone)]
    struct InputState {
        move_x: f32,
        move_z: f32,
        orientation: f32,
        buttons: u32,
    }
    let mut input = InputState::default();

    let input_interval =
        Duration::from_micros(1_000_000 / cfg.client.input_hz.max(1) as u64);
    let ping_interval = Duration::from_millis(500);

    let mut last_input_tx = Instant::now();
    let mut last_ping_tx = Instant::now();
    let mut last_rtt_us = 0u64;

    // Liveness probe: if `session_open` per the FSM but the server stops
    // sending `StateAck` for this long, suspect an orphaned source
    // session (handoff cleanup ran on the server, the player record is
    // gone, but our UDP socket still appears alive) and reconnect via
    // gateway. 3 s is well past worst-case server tick stalls (split
    // coordinator can pause per-shard work briefly during region swaps,
    // ~hundreds of ms) but short enough that the user notices recovery
    // before they get bored. See entanglement-server#9 for the
    // server-side root cause.
    const LIVENESS_TIMEOUT: Duration = Duration::from_secs(3);
    // `Instant::now()` rather than the wait-for-session-open moment
    // because the server starts emitting StateAck immediately after
    // SESSION_OPEN, so there's never a legitimate cold-start gap;
    // any > 3 s gap is an anomaly we want to recover from.
    let mut last_state_ack = Instant::now();
    // Last known world-space player position. We seed it from the
    // initial origin and keep it fresh from `StateAck` so a
    // gateway-bounce after a draining target shard can re-route to the
    // correct region. f32 because that's what the gateway proto takes.
    let mut last_pos = (
        client.session.origin_x as f32,
        client.session.origin_z as f32,
    );

    loop {
        // 1. Drain GUI commands. try_recv is non-blocking; `disconnected`
        //    here means the render thread has closed, so we exit.
        loop {
            match gui_cmds.try_recv() {
                Ok(GuiCmd::Move {
                    move_x,
                    move_z,
                    orientation,
                    buttons,
                }) => {
                    input.move_x = move_x;
                    input.move_z = move_z;
                    input.orientation = orientation;
                    input.buttons = buttons;
                }
                Ok(GuiCmd::Action { action_type }) => {
                    client.send_action(action_type);
                }
                Ok(GuiCmd::Quit) => {
                    client.stop();
                    return;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    client.stop();
                    return;
                }
            }
        }

        // 2. Poll the game client and forward events.
        //    `Disconnected` and `ShardHandoffReceived` need batch-level
        //    handling (handoff vs. graceful close vs. genuine drop), so
        //    we collect first, forward the rest, and react after.
        let events = client.poll_events();
        let handoff_target = events.iter().find_map(|e| match e {
            GameEvent::ShardHandoffReceived {
                new_ip,
                new_port,
                new_shard_id,
                handoff_token,
                ..
            } => Some((new_ip.clone(), *new_port, *new_shard_id, *handoff_token)),
            _ => None,
        });
        let saw_disconnect = events
            .iter()
            .any(|e| matches!(e, GameEvent::Disconnected));
        // Post-session-open `SessionAuthFailed` is the server's explicit
        // "your session is gone, reconnect" signal — emitted by
        // entanglement-server when a handoff cleanup tears down our
        // source-shard slot. Treat it the same as `Disconnected` for
        // the recovery path: try the gateway with last_pos before
        // exiting. The reason code matters less than the fact that
        // the server told us to leave; we don't try to interpret it
        // beyond "go via gateway".
        let saw_auth_failed = events
            .iter()
            .any(|e| matches!(e, GameEvent::SessionAuthFailed { .. }));

        for ev in events {
            // Track player position so a `RetryViaGateway` after a
            // ShardDraining target can re-route correctly. World-space
            // (StateAck.x / .z is already world-space, not wire-relative).
            if let GameEvent::StateAck { x, z, .. } = &ev {
                last_pos = (*x as f32, *z as f32);
                last_state_ack = Instant::now();
            }
            // Defer Disconnected / ShardHandoffReceived to the
            // post-loop handler — emitting NetEvent::Disconnected here
            // would flicker the HUD into "not in session" before the
            // reconnect we're about to do.
            if matches!(ev, GameEvent::Disconnected | GameEvent::ShardHandoffReceived { .. }) {
                continue;
            }
            if let Some(mapped) = map_event(ev) {
                // If the session got a fresher RTT, forward that too.
                if client.session.last_rtt_us != last_rtt_us {
                    last_rtt_us = client.session.last_rtt_us;
                    let _ = net_events.send(NetEvent::RttSample(
                        (last_rtt_us / 1000) as u32,
                    ));
                }
                if net_events.send(mapped).is_err() {
                    // Renderer gone — stop cleanly.
                    client.stop();
                    return;
                }
            }
        }

        // 2b. Handle handoff or grace period before sending inputs/pings.
        if let Some((new_ip, new_port, new_shard_id, handoff_token)) = handoff_target {
            if !do_handoff(
                &mut client,
                cfg,
                &jwt,
                persistent_id,
                &new_ip,
                new_port,
                new_shard_id,
                handoff_token,
                &mut gateway,
                last_pos,
                &net_events,
            )
            .await
            {
                return;
            }
            // After a successful handoff, origin shifts; refresh
            // `last_pos` to the new origin so the next loop iteration
            // doesn't carry stale wire-space coordinates. Reset the
            // liveness clock so the LIVENESS_TIMEOUT check doesn't
            // immediately fire on the post-handoff window before the
            // new shard's first StateAck arrives.
            last_pos = (
                client.session.origin_x as f32,
                client.session.origin_z as f32,
            );
            last_state_ack = Instant::now();
        } else if saw_disconnect || saw_auth_failed {
            // Grace period (mirrors mmo-cli/src/gameplay.rs:678-735):
            // a `Disconnected` without a preceding handoff might be the
            // shard tearing down the UDP socket microseconds before the
            // SHARD_HANDOFF lands. Poll for one for 250 ms before giving
            // up.
            let mut late: Option<(String, u16, u32, u64)> = None;
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                for ev in client.poll_events() {
                    if let GameEvent::ShardHandoffReceived {
                        new_ip,
                        new_port,
                        new_shard_id,
                        handoff_token,
                        ..
                    } = ev
                    {
                        late = Some((new_ip, new_port, new_shard_id, handoff_token));
                        break;
                    }
                }
                if late.is_some() {
                    break;
                }
            }
            if let Some((new_ip, new_port, new_shard_id, handoff_token)) = late {
                let _ = net_events.send(NetEvent::Status(format!(
                    "⟳ Handoff (late) → shard 0x{:X} at {}:{}",
                    new_shard_id, new_ip, new_port
                )));
                if !do_handoff(
                    &mut client,
                    cfg,
                    &jwt,
                    persistent_id,
                    &new_ip,
                    new_port,
                    new_shard_id,
                    handoff_token,
                    &mut gateway,
                    last_pos,
                    &net_events,
                )
                .await
                {
                    return;
                }
                last_pos = (
                    client.session.origin_x as f32,
                    client.session.origin_z as f32,
                );
                last_state_ack = Instant::now();
            } else {
                // Bare `Disconnected` after the handoff grace expired. The
                // socket really is gone and no late SHARD_HANDOFF came. Try
                // the gateway with our last known position before giving
                // up — covers the orphan-source-session case where the
                // shard cleaned up our handoff slot but the client never
                // received SHARD_HANDOFF (server drops it during a split's
                // brief routing limbo, packet loss on the control
                // channel, etc.). One re-route via gateway gets us back
                // on a live shard at last_pos. If that also fails, the
                // viewer exits cleanly.
                let _ = net_events.send(NetEvent::Status(
                    "⟳ Disconnect — recovering via gateway".into(),
                ));
                if !retry_via_gateway(
                    &mut client, cfg, &jwt, &mut gateway, last_pos, &net_events,
                )
                .await
                {
                    return;
                }
                last_pos = (
                    client.session.origin_x as f32,
                    client.session.origin_z as f32,
                );
                last_state_ack = Instant::now();
            }
        } else if last_state_ack.elapsed() > LIVENESS_TIMEOUT {
            // Liveness probe: the FSM thinks we're connected but no
            // StateAck has arrived for `LIVENESS_TIMEOUT`. Almost
            // certainly an orphan source session — the server cleaned
            // up our handoff slot but the UDP transport hasn't noticed.
            // Trigger gateway recovery the same way a bare disconnect
            // would, so the viewer never gets stuck staring at a frozen
            // world.
            let _ = net_events.send(NetEvent::Status(
                "⟳ StateAck stale — recovering via gateway".into(),
            ));
            if !retry_via_gateway(
                &mut client, cfg, &jwt, &mut gateway, last_pos, &net_events,
            )
            .await
            {
                return;
            }
            last_pos = (
                client.session.origin_x as f32,
                client.session.origin_z as f32,
            );
            last_state_ack = Instant::now();
        }

        // 3. Send a PlayerMove at input_hz cadence regardless of whether
        //    the input changed. The shard expects a steady input stream;
        //    going quiet would make our player look laggy and trip the
        //    inactivity dedup on the server.
        let now = Instant::now();
        if now.duration_since(last_input_tx) >= input_interval {
            last_input_tx = now;
            client.send_move(
                input.move_x,
                input.move_z,
                input.orientation,
                input.buttons,
            );
        }

        // 4. Periodic ping.
        if now.duration_since(last_ping_tx) >= ping_interval {
            last_ping_tx = now;
            client.send_ping();
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Translate a GameClient event into a viewer-level NetEvent. Returning
/// `None` drops events the viewer doesn't visualise (e.g. handoff, which
/// is out of scope for the MVP).
fn map_event(ev: GameEvent) -> Option<NetEvent> {
    match ev {
        GameEvent::SessionOpened { .. } => {
            // Already reported once at bootstrap; duplicates (from a
            // watchdog resend, say) are harmless to drop.
            None
        }
        GameEvent::SessionAuthFailed { reason } => {
            Some(NetEvent::AuthFailed { reason })
        }
        GameEvent::StateAck {
            server_tick,
            x,
            y,
            z,
            hp,
            stamina,
        } => Some(NetEvent::StateAck {
            server_tick,
            x,
            y,
            z,
            hp,
            stamina,
        }),
        GameEvent::EntityMoved {
            entity_id,
            x,
            y,
            z,
            orientation,
            source_shard_hash,
        } => Some(NetEvent::EntityMoved {
            entity_id,
            x,
            y,
            z,
            orientation,
            source_shard_hash,
        }),
        GameEvent::HitConfirm {
            target_id,
            damage,
            target_hp,
        } => Some(NetEvent::HitConfirm {
            target_id,
            damage,
            target_hp,
        }),
        GameEvent::EntityHealth {
            entity_id,
            hp,
            max_hp,
        } => Some(NetEvent::EntityHealth {
            entity_id,
            hp,
            max_hp,
        }),
        GameEvent::EntityStateChanged {
            entity_id,
            state_id,
            param_a,
        } => Some(NetEvent::EntityStateChanged {
            entity_id,
            state_id,
            param_a,
        }),
        GameEvent::ActionRejected { reason } => {
            Some(NetEvent::ActionRejected { reason })
        }
        GameEvent::ShardHandoffReceived { .. } => {
            // Reconnect happens in `run_hot_loop`; this branch is only
            // reachable defensively if a handoff event slips past the
            // batch-level filter, in which case dropping it is correct
            // (we don't want to flash "Disconnected" mid-handoff).
            None
        }
        GameEvent::Disconnected => Some(NetEvent::Disconnected),
    }
}

// ── Handoff / gateway-retry helpers ───────────────────────────────────────

/// Spawn a tokio task that drains `old_client.poll_events()` at a small
/// interval and forwards "still useful" events (everything except the
/// local player's `StateAck` and the lifecycle events the main loop
/// already handles) to the renderer. Used during handoff so remote
/// entities (bots, ghosts) keep receiving updates from the *old* shard
/// while the new shard is still authenticating — the dual-socket path
/// the user asked for in place of the freeze-then-snap window.
///
/// Returns a `(stop_flag, JoinHandle)` pair: set the flag to `true` and
/// `await` the handle to gracefully drain remaining events from the
/// old client and shut it down.
///
/// `StateAck` is filtered out because the local player's old position
/// is the freeze point — it's already stale by the moment SHARD_HANDOFF
/// arrived; the dead-reckoning path keeps the camera moving while the
/// new shard's first StateAck takes over as the authoritative source.
fn spawn_old_client_drain(
    mut old_client: GameClient,
    net_events: crossbeam_channel::Sender<NetEvent>,
) -> (Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let handle = tokio::spawn(async move {
        // Tight enough to capture the next 4 ms tick from the old shard
        // (server is at 120 Hz = ~8 ms, we sample at ~250 Hz so we
        // never miss a broadcast); not so tight we burn the CPU
        // during the typically <30 ms handoff window.
        let mut interval = tokio::time::interval(Duration::from_millis(2));
        while !stop_flag.load(Ordering::Relaxed) {
            interval.tick().await;
            for ev in old_client.poll_events() {
                // Lifecycle events are owned by the main loop's handoff
                // logic, not by this bridge.
                if matches!(
                    ev,
                    GameEvent::Disconnected | GameEvent::ShardHandoffReceived { .. }
                ) {
                    continue;
                }
                // Skip self-state from the old shard: the player is
                // mid-handoff, frozen on this side, so the StateAck
                // values are stale. Dead-reckoning + the new shard's
                // first StateAck take over the local player.
                if matches!(ev, GameEvent::StateAck { .. }) {
                    continue;
                }
                if let Some(mapped) = map_event(ev) {
                    if net_events.send(mapped).is_err() {
                        // Renderer gone — stop draining.
                        break;
                    }
                }
            }
        }
        // One last drain on shutdown so we don't lose a final batch.
        for ev in old_client.poll_events() {
            if matches!(
                ev,
                GameEvent::Disconnected
                    | GameEvent::ShardHandoffReceived { .. }
                    | GameEvent::StateAck { .. }
            ) {
                continue;
            }
            if let Some(mapped) = map_event(ev) {
                let _ = net_events.send(mapped);
            }
        }
        old_client.stop();
    });
    (stop, handle)
}

/// Hand off the current `GameClient` to a new shard at `new_ip:new_port`
/// using `HandoffAuth`, **maintaining both UDP sessions in parallel
/// during the reconnect window** so remote entities never lose their
/// update stream. The old session keeps broadcasting bots / ghosts
/// from the source shard while the new session authenticates; once the
/// new session reports `SessionOpened` we stop the old drain and
/// publish the new origin to the renderer.
///
/// Returns `false` if the viewer should exit the hot loop (terminal
/// failure or renderer-gone). On `AuthError::ShardDraining` from the
/// target — which happens when the absorber shard re-splits before
/// our packet lands — we bounce through the gateway with `last_pos`
/// instead of giving up; the old drain stays alive across that
/// gateway round-trip too.
#[allow(clippy::too_many_arguments)]
async fn do_handoff(
    client: &mut GameClient,
    cfg: &ViewerConfig,
    jwt: &str,
    persistent_id: u32,
    new_ip: &str,
    new_port: u16,
    new_shard_id: u32,
    handoff_token: u64,
    gateway: &mut GatewayClient,
    last_pos: (f32, f32),
    net_events: &crossbeam_channel::Sender<NetEvent>,
) -> bool {
    let _ = net_events.send(NetEvent::Status(format!(
        "⟳ Handoff → shard 0x{:X} at {}:{}",
        new_shard_id, new_ip, new_port
    )));
    // Tell the renderer to enter dead-reckoning for the local
    // player. The new shard's first StateAck clears the flag.
    let _ = net_events.send(NetEvent::HandoffStarted);

    // ── Dual-socket path ───────────────────────────────────────────
    // Move ownership of the old GameClient into a background drain
    // task that keeps polling its events and forwarding remote-entity
    // updates while we authenticate the new connection. The old
    // shard's broadcasts continue to flow (source still treats our
    // session as HandoffPending, not closed), so bots near the
    // boundary keep updating without the previous ~25 ms freeze.
    //
    // We replace `*client` with a fresh GameClient here so the
    // outer hot loop's `client.poll_events()` call after we return
    // already targets the new shard.
    let old_client = std::mem::replace(client, GameClient::new(&cfg.client));
    client.set_jwt(jwt);
    let (old_drain_stop, old_drain_handle) =
        spawn_old_client_drain(old_client, net_events.clone());

    // Helper to gracefully shut down the old drain on exit.
    async fn shutdown_old_drain(
        stop: Arc<AtomicBool>,
        handle: tokio::task::JoinHandle<()>,
    ) {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.await;
    }

    let connect_res = client
        .connect_with_handoff_auth(new_ip, new_port, persistent_id, handoff_token)
        .await;
    if let Err(e) = connect_res {
        let _ = net_events.send(NetEvent::Status(format!("✗ Handoff connect failed: {e}")));
        let _ = net_events.send(NetEvent::Disconnected);
        shutdown_old_drain(old_drain_stop, old_drain_handle).await;
        return false;
    }

    match client.wait_for_session_open().await {
        Ok(()) => {
            // New shard is alive. Stop bridging from the old socket —
            // its data is now stale relative to the authoritative
            // source. Drain handle joins quickly because the loop
            // checks the stop flag every interval tick (≤ 2 ms).
            shutdown_old_drain(old_drain_stop, old_drain_handle).await;
            publish_session_opened(client, net_events);
            true
        }
        Err(AuthError::ShardDraining) => {
            // Target shard is itself draining — typically because a
            // merge absorber split again, or the orchestrator picked
            // an already-overloaded shard. Re-ask the gateway with the
            // last known world position. Old drain stays alive across
            // the gateway round-trip too — same dual-socket benefit
            // applies: bots keep updating from the source shard while
            // the gateway re-routes us.
            let result = retry_via_gateway(
                client, cfg, jwt, gateway, last_pos, net_events,
            )
            .await;
            shutdown_old_drain(old_drain_stop, old_drain_handle).await;
            result
        }
        Err(AuthError::Other(e)) => {
            let _ = net_events.send(NetEvent::Status(format!("✗ Handoff auth failed: {e}")));
            let _ = net_events.send(NetEvent::Disconnected);
            shutdown_old_drain(old_drain_stop, old_drain_handle).await;
            false
        }
    }
}

/// Re-ask the gateway for a shard assignment from `last_pos` and
/// reconnect with a regular `SessionAuth`. Used when the handoff
/// target rejects us as draining.
async fn retry_via_gateway(
    client: &mut GameClient,
    cfg: &ViewerConfig,
    jwt: &str,
    gateway: &mut GatewayClient,
    last_pos: (f32, f32),
    net_events: &crossbeam_channel::Sender<NetEvent>,
) -> bool {
    let _ = net_events.send(NetEvent::Status(
        "⟳ Target draining — re-asking gateway…".into(),
    ));
    let shard = match gateway.player_connect(jwt, last_pos.0, last_pos.1).await {
        Ok(s) => s,
        Err(e) => {
            let _ = net_events.send(NetEvent::Status(format!("✗ Gateway re-route failed: {e}")));
            let _ = net_events.send(NetEvent::Disconnected);
            return false;
        }
    };
    client.stop();
    *client = GameClient::new(&cfg.client);
    if let Err(e) = client.connect(&shard.ip, shard.port, jwt).await {
        let _ = net_events.send(NetEvent::Status(format!("✗ Reconnect failed: {e}")));
        let _ = net_events.send(NetEvent::Disconnected);
        return false;
    }
    match client.wait_for_session_open().await {
        Ok(()) => {
            publish_session_opened(client, net_events);
            true
        }
        Err(e) => {
            let _ = net_events.send(NetEvent::Status(format!(
                "✗ Session auth after gateway retry failed: {e}"
            )));
            let _ = net_events.send(NetEvent::Disconnected);
            false
        }
    }
}

/// Push a `SessionOpened` mirroring the freshly-opened `client.session`
/// so the renderer can re-anchor its world view (origin shifts on
/// every shard handoff).
fn publish_session_opened(
    client: &GameClient,
    net_events: &crossbeam_channel::Sender<NetEvent>,
) {
    let _ = net_events.send(NetEvent::SessionOpened {
        player_id: client.session.player_id,
        persistent_id: client.session.persistent_id,
        origin_x: client.session.origin_x,
        origin_z: client.session.origin_z,
        shard_id: client.session.shard_id,
    });
}
