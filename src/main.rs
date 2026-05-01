//! `mmo-viewer` — a minimal 2D top-down debugger for the MMO client protocol.
//!
//! The UI is macroquad (single-crate 2D lib with its own window loop and
//! single-threaded async executor). All networking runs on a separate OS
//! thread hosting a tokio runtime; the two sides talk through
//! crossbeam channels. See `channels.rs` for the wire between them.
//!
//! One frame does:
//!   1. Drain the `NetEvent` queue into the mirrored `World`.
//!   2. Poll keyboard → `GuiCmd` list; forward to the net thread.
//!   3. Render the world + HUD.
//!
//! No game logic lives here. Everything interesting happens server-side;
//! the viewer exists to show you what the server actually sent.

mod channels;
mod config;
mod input;
mod net;
mod render;
mod world;

use std::time::Instant;

use macroquad::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::channels::GuiCmd;
use crate::config::ViewerConfig;
use crate::input::InputState;
use crate::world::World;

/// macroquad `Conf` — sets the window title and a sensible default size.
/// Real debugging sessions tend to want more real estate; resize at
/// runtime via the OS window decorations, macroquad handles it.
fn window_conf() -> Conf {
    Conf {
        window_title: "MMO Viewer".to_string(),
        window_width: 1280,
        window_height: 800,
        high_dpi: true,
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    // Tracing subscriber: viewer-level info by default, tweak via
    // RUST_LOG=mmo_viewer=debug,mmo_cli=debug. Initialised before we
    // spawn the net thread so it inherits the default subscriber.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("mmo_viewer=info".parse().unwrap())
                .add_directive("mmo_cli=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cfg = match ViewerConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return;
        }
    };

    // Spawn the tokio network thread. We don't keep the join handle:
    // the thread drops when both channels hang up, which happens when
    // we fall out of the main loop below (render thread drops
    // `gui_cmds`/`net_events`).
    let (gui, net) = channels::make_channels();
    let _net_thread = net::spawn(cfg.clone(), net);

    let mut world = World::default();
    // Seed the input layer with the configured `view_range`, which it
    // then owns and mutates in response to zoom keys / mouse wheel.
    let mut input_state = InputState::with_view_range(cfg.view_range);

    loop {
        // 1. Drain whatever the net thread produced since the last frame.
        //    Bounded drain is not needed — events arrive at a human-scale
        //    rate (tens per second) and we're happy to handle spikes.
        while let Ok(ev) = gui.net_events.try_recv() {
            world.apply(ev);
        }
        world.prune_stale(cfg.stale_entity_secs);

        // 2. Keyboard → GuiCmd list → net thread.
        //    The viewer also tracks its own yaw here (the server doesn't
        //    echo self-orientation in a way we can reliably pick up), and
        //    records action-send timestamps so the renderer can flash the
        //    triangle even for actions the server doesn't turn into a
        //    visible combat_state (jump, dodge).
        let cmds = input::poll(&mut input_state);
        world.self_orientation = input_state.orientation;
        // Mirror the latest movement intent to the world so the
        // dead-reckoning path during shard handoffs (see
        // `World::predicted_self_pos`) extrapolates with the keys
        // the user is actually holding right now, not a stale
        // pre-handoff value.
        for cmd in &cmds {
            if let GuiCmd::Move { move_x, move_z, .. } = cmd {
                world.last_input_x = *move_x;
                world.last_input_z = *move_z;
            }
        }
        for cmd in cmds {
            // Capture action code + timestamp before the Move, so an
            // Action emitted on the same frame still reaches the world
            // even if we return early on Quit.
            if let GuiCmd::Action { action_type } = &cmd {
                world.self_action_flash = Some((*action_type, Instant::now()));
            }
            let is_quit = matches!(cmd, GuiCmd::Quit);
            let _ = gui.gui_cmds.send(cmd);
            if is_quit {
                // Give the net thread a few frames to propagate the Stop
                // to the shard; macroquad's next_frame().await yields the
                // thread, which is enough for crossbeam to flush.
                next_frame().await;
                next_frame().await;
                return;
            }
        }

        // 3. Render. Any NetEvents we missed this frame will be picked
        //    up next frame — there's no visible latency at 60 Hz.
        render::draw(&world, input_state.view_range);

        // Auth failures and action rejections stay visible in the HUD
        // rather than closing the window — the user can inspect and
        // Esc out when they've read the message.

        next_frame().await;
    }
}
