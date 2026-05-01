// Compile only what the boundary-overlay needs: world_coord.proto's
// `WorldCoord/ListShards` returns shard regions, which is everything
// the viewer's debug map needs to draw the seam lines.  We don't
// pull the rest of the proto tree to keep build time and binary size
// under control — the existing networking still goes through
// `mmo-cli`'s gateway/auth clients.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["proto/proto/world_coord.proto"],
            &["proto/proto"],
        )?;
    Ok(())
}
