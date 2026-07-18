use godot::prelude::*;

mod backends;
mod capture_thread;
mod desktop_capture;

struct DesktopCasterExtension;

#[gdextension]
unsafe impl ExtensionLibrary for DesktopCasterExtension {}