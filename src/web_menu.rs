//! Rust-only bridge between native menu actions and the GUI's web controls.
//! AppKit callbacks record requests; RunkiApp owns all server operations.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

use eframe::egui;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebMenuStatus {
    pub on: bool,
    pub lan_url: Option<String>,
    pub port: u16,
}

impl Default for WebMenuStatus {
    fn default() -> Self {
        Self {
            on: false,
            lan_url: None,
            port: 8765,
        }
    }
}

#[derive(Default)]
pub struct WebMenuBridge {
    status: RwLock<WebMenuStatus>,
    toggle_requests: AtomicUsize,
    context: RwLock<Option<egui::Context>>,
}

impl WebMenuBridge {
    pub fn set_context(&self, context: egui::Context) {
        *self.context.write().unwrap_or_else(|err| err.into_inner()) = Some(context);
    }

    pub fn request_toggle(&self) {
        self.toggle_requests.fetch_add(1, Ordering::Relaxed);
        if let Some(context) = self
            .context
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
        {
            context.request_repaint();
        }
    }

    /// Drain all requests once. Two clicks remain two toggles, even when they
    /// arrive before a GUI frame; requests arriving during the drain wait for
    /// the next frame.
    pub fn take_toggle_requests(&self) -> usize {
        self.toggle_requests.swap(0, Ordering::Relaxed)
    }

    pub fn publish(&self, status: WebMenuStatus) {
        *self.status.write().unwrap_or_else(|err| err.into_inner()) = status;
    }

    pub fn status(&self) -> WebMenuStatus {
        self.status
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn requests_are_counted_and_consumed_once_with_or_without_context() {
        let bridge = WebMenuBridge::default();
        assert_eq!(bridge.take_toggle_requests(), 0);
        bridge.request_toggle();
        bridge.request_toggle();
        assert_eq!(bridge.take_toggle_requests(), 2);
        assert_eq!(bridge.take_toggle_requests(), 0);
        bridge.set_context(egui::Context::default());
        bridge.request_toggle();
        assert_eq!(bridge.take_toggle_requests(), 1);
    }

    #[test]
    fn status_is_a_snapshot_and_publishing_does_not_consume_requests() {
        let bridge = WebMenuBridge::default();
        let off = bridge.status();
        bridge.request_toggle();
        let on = WebMenuStatus {
            on: true,
            lan_url: Some("http://192.168.1.2:9000/".into()),
            port: 9000,
        };
        bridge.publish(on.clone());
        assert_eq!(bridge.status(), on);
        assert_eq!(off, WebMenuStatus::default());
        assert_eq!(bridge.take_toggle_requests(), 1);
        bridge.publish(off.clone());
        assert_eq!(bridge.status(), off);
    }

    #[test]
    fn concurrent_requesters_do_not_lose_toggles() {
        let bridge = Arc::new(WebMenuBridge::default());
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let bridge = Arc::clone(&bridge);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        bridge.request_toggle();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(bridge.take_toggle_requests(), 400);
        assert_eq!(bridge.take_toggle_requests(), 0);
    }
}
