//! System tray icon for the Flux server.
//!
//! Displays a tray icon with a context menu showing server status,
//! active sessions, and controls for pairing / shutdown.

use std::sync::Arc;

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use parking_lot::RwLock;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Actions that can be triggered from the tray menu.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TrayAction {
    ShowPin,
    ShowStatus,
    OpenConfig,
    Quit,
}

/// Shared state exposed to the tray for display purposes.
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    pub active_sessions: usize,
    pub server_name: String,
    pub bind_address: String,
}

/// Manages the system tray icon and context menu.
pub struct FluxTray {
    _tray_icon: TrayIcon,
    menu_items: TrayMenuItems,
    state: Arc<RwLock<TrayState>>,
}

struct TrayMenuItems {
    status_item: MenuItem,
    sessions_item: MenuItem,
    pin_item: MenuItem,
    config_item: MenuItem,
    quit_item: MenuItem,
}

impl FluxTray {
    /// Create and display the system tray icon.
    ///
    /// Must be called from the main thread (Windows requirement for message pump).
    pub fn new(state: Arc<RwLock<TrayState>>) -> Result<Self, Box<dyn std::error::Error>> {
        // Build the context menu
        let status_item = MenuItem::new("Flux Server: Starting...", false, None);
        let sessions_item = MenuItem::new("Active Sessions: 0", false, None);
        let pin_item = MenuItem::new("Show Pairing PIN", true, None);
        let config_item = MenuItem::new("Open Configuration", true, None);
        let quit_item = MenuItem::new("Quit Flux Server", true, None);

        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&sessions_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&pin_item)?;
        menu.append(&config_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_item)?;

        // Create a simple icon (16x16 solid color as placeholder)
        let icon = create_flux_icon()?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Flux Remote Streaming Server")
            .with_icon(icon)
            .build()?;

        tracing::info!("System tray icon created");

        // Update initial state
        {
            let s = state.read();
            status_item.set_text(&format!("Flux: {}", s.server_name));
        }

        Ok(Self {
            _tray_icon: tray_icon,
            menu_items: TrayMenuItems {
                status_item,
                sessions_item,
                pin_item,
                config_item,
                quit_item,
            },
            state,
        })
    }

    /// Poll for tray menu events (non-blocking).
    ///
    /// Returns the action if the user clicked a menu item.
    pub fn poll_event(&self) -> Option<TrayAction> {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id;
            if id == self.menu_items.pin_item.id() {
                return Some(TrayAction::ShowPin);
            } else if id == self.menu_items.config_item.id() {
                return Some(TrayAction::OpenConfig);
            } else if id == self.menu_items.quit_item.id() {
                return Some(TrayAction::Quit);
            }
        }
        None
    }

    /// Update the tray menu to reflect current server state.
    pub fn update_state(&self) {
        let s = self.state.read();
        self.menu_items
            .status_item
            .set_text(&format!("Flux: {} ({})", s.server_name, s.bind_address));
        self.menu_items
            .sessions_item
            .set_text(&format!("Active Sessions: {}", s.active_sessions));
    }
}

/// Create the Flux tray icon — a 32x32 RGBA image.
///
/// Generates a programmatic icon: a blue square with a white "F" shape.
fn create_flux_icon() -> Result<Icon, Box<dyn std::error::Error>> {
    let size: u32 = 32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let idx = ((y * size + x) * 4) as usize;

            // Background: rounded blue (#2563EB)
            let in_bounds = x >= 2 && x < size - 2 && y >= 2 && y < size - 2;
            if in_bounds {
                rgba[idx] = 37;     // R
                rgba[idx + 1] = 99; // G
                rgba[idx + 2] = 235; // B
                rgba[idx + 3] = 255; // A
            } else {
                rgba[idx + 3] = 0; // Transparent
            }

            // White "F" letter shape
            let is_f = (y >= 7 && y <= 9 && x >= 9 && x <= 22)    // Top bar
                     || (y >= 14 && y <= 16 && x >= 9 && x <= 19)   // Middle bar
                     || (x >= 9 && x <= 11 && y >= 7 && y <= 25);   // Vertical bar

            if is_f && in_bounds {
                rgba[idx] = 255;     // R
                rgba[idx + 1] = 255; // G
                rgba[idx + 2] = 255; // B
                rgba[idx + 3] = 255; // A
            }
        }
    }

    let icon = Icon::from_rgba(rgba, size, size)?;
    Ok(icon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_creation() {
        let icon = create_flux_icon();
        assert!(icon.is_ok(), "Icon creation should not fail: {:?}", icon.err());
    }

    #[test]
    fn test_tray_state_default() {
        let state = TrayState::default();
        assert_eq!(state.active_sessions, 0);
        assert!(state.server_name.is_empty());
        assert!(state.bind_address.is_empty());
    }

    #[test]
    fn test_icon_rgba_buffer_size() {
        // The icon is 32x32 RGBA = 4096 bytes
        let size: u32 = 32;
        let expected_len = (size * size * 4) as usize;
        let rgba = vec![0u8; expected_len];
        assert_eq!(rgba.len(), 4096);
    }
}
