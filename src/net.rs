//! Network driver for the viewer.
//!
//! Runs on a dedicated OS thread with its own `tokio::runtime`. The main
//! (macroquad) thread talks to this one via crossbeam channels:
//!
//!   GUI → NET: `GuiCmd`    (latest move input + one-shot actions + quit)
//!   NET → GUI: `NetEvent`  (status + session + per-entity updates)
//!
//! The bootstrap sequence is the same one `mmo-cli` uses (auth-service
//! login → gateway player_connect → GameClient::connect + wait_for_open)
//! but simplified: we read credentials from the viewer's own config,
//! there's no interactive lobby, and we don't bother with re-routing on
//! SHARD_DRAINING — if the shard rejects us at startup we just report
//! the error and exit. The viewer is a debugger, not a resilient
//! long-running client.

use std::time::{Duration, Instant};

use mmo_cli::auth_client::AuthServiceClient;
use mmo_cli::game_client::{GameClient, GameEvent};
use mmo_cli::gateway_client::GatewayClient;

use tokio::runtime::Builder;

use crate::channels::{GuiCmd, NetChannels, NetEvent};
use crate::config::ViewerConfig;

/// Spawn the net thread. Consumes the `NetChannels` end of the bridge;
/// the caller keeps the matching `GuiChannels` for the render loop. The
/// join handle is returned so `main` can wait for a clean shutdown after
/// macroquad's window closes, but in practice we drop it and let the
/// thread exit when its channels hang up.
pub fn spawn(cfg: ViewerConfig, ch: NetChannels) -> std::thread::JoinHandle<()> {
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
            rt.block_on(run(cfg, ch));
        })
        .expect("spawn mmo-viewer-net thread")
}

async fn run(cfg: ViewerConfig, ch: NetChannels) {
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
    // to "playing" and lock the camera to `player_id`.
    let _ = net_events.send(NetEvent::SessionOpened {
        player_id: client.session.player_id,
        persistent_id: client.session.persistent_id,
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
    run_hot_loop(client, net_events.clone(), gui_cmds, &cfg).await;
    status("net loop exited");
}

async fn run_hot_loop(
    mut client: GameClient,
    net_events: crossbeam_channel::Sender<NetEvent>,
    gui_cmds: crossbeam_channel::Receiver<GuiCmd>,
    cfg: &ViewerConfig,
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
        for ev in client.poll_events() {
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
        } => Some(NetEvent::EntityMoved {
            entity_id,
            x,
            y,
            z,
            orientation,
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
            // MVP: treat a handoff as a disconnect. Reconnecting to the
            // new shard would require plumbing the handoff token through
            // the viewer; deferred to phase 2.
            Some(NetEvent::Disconnected)
        }
        GameEvent::Disconnected => Some(NetEvent::Disconnected),
    }
}
