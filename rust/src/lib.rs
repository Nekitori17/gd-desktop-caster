use godot::prelude::*;

mod backends;
mod capture_thread;
mod desktop_caster;

struct DesktopCasterExtension;

#[gdextension]
unsafe impl ExtensionLibrary for DesktopCasterExtension {}