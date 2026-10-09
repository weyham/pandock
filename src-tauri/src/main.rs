#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    // Velopack startup hook (Windows only): handles --veloapp-* lifecycle
    // args (install/update/uninstall handshakes) and may exit the process.
    #[cfg(target_os = "windows")]
    velopack::VelopackApp::build().run();
    pandock_app_lib::run()
}
