pub mod backlinks_panel;

pub use backlinks_panel::{BacklinksPanel, BacklinkEntry};

use gpui::App;

pub fn init(cx: &mut App) {
    backlinks_panel::init(cx);
} 