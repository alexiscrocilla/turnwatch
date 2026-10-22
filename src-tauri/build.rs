fn main() {
    // Declaring the app commands makes them subject to the capabilities
    // (`allow-<command>` permissions in capabilities/default.json).
    let manifest = tauri_build::AppManifest::new().commands(&[
        "get_snapshot",
        "get_settings",
        "set_settings",
        "open_session",
        "ack_session",
        "set_compact",
        "set_always_on_top",
        "open_url",
        "hide_window",
        "quit_app",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to run tauri-build");
}
