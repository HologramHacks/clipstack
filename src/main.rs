// ClipStack entry point: one platform module per OS.
// All the Windows logic lives unchanged in win.rs. mac.rs is the experimental
// macOS port (v0 skeleton): compile-verified on CI, not yet run on hardware.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(target_os = "macos")]
mod mac;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(target_os = "macos")]
    mac::run();
}
