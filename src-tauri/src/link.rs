//! Owns the connection to the supply: startup autodetect, status polling, and reconnecting
//! after the link drops. A worker thread does all of it and reports through events; Tauri
//! commands borrow the open device through [`Link::with_psu`].
//!
//! Background retries are deliberately narrow. Probing writes a Modbus frame to whatever is
//! on the port, and merely opening some ports resets the device behind them (Arduino DTR),
//! so while disconnected we only retry the last-known port and probe ports that newly
//! appeared. A full scan happens at startup and when the user asks for one.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use psu_core::drivers::{self, Detected};
use psu_core::psu::{self, Limits, Psu, Status};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::persist;

pub const EVENT_LINK: &str = "psu://link";
pub const EVENT_STATUS: &str = "psu://status";

const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Consecutive failed polls before the link counts as lost. A timed-out read already takes
/// 250 ms, so this declares loss after about a second of silence.
const MAX_POLL_FAILURES: u32 = 3;
/// While disconnected: how often to look for new ports and retry the last-known one.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// A supply we have talked to. Persisted so the next session can try it first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownDevice {
    pub port: String,
    pub driver: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LinkState {
    /// Probing ports. `last` is the most recent known device, shown while we look for it.
    Searching { last: Option<KnownDevice> },
    Connected { device: KnownDevice, limits: Limits },
    /// Nothing connected; the worker keeps retrying in the background.
    Disconnected { last: Option<KnownDevice>, reason: String },
}

/// Everything the frontend needs to render, for when it (re)loads after events already fired.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub link: LinkState,
    pub status: Option<Status>,
}

enum Wake {
    /// A command changed the device; poll now so the UI catches up.
    Poll,
    /// The user asked for a full scan.
    Rescan,
}

struct Shared {
    psu: Mutex<Option<Box<dyn Psu>>>,
    snapshot: Mutex<Snapshot>,
}

/// Recovers from poisoning: a panicked poll must not take the device handle with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct Link {
    shared: Arc<Shared>,
    wake: Sender<Wake>,
}

impl Link {
    /// Spawns the worker, which immediately starts looking for the supply.
    pub fn start(app: AppHandle, last_device_path: PathBuf) -> Self {
        let last = persist::load(&last_device_path);
        let shared = Arc::new(Shared {
            psu: Mutex::new(None),
            snapshot: Mutex::new(Snapshot {
                link: LinkState::Searching { last: last.clone() },
                status: None,
            }),
        });
        let (wake, wake_rx) = mpsc::channel();
        let worker = Worker {
            app,
            shared: shared.clone(),
            wake: wake_rx,
            last,
            last_device_path,
            seen_ports: BTreeSet::new(),
            failures: 0,
        };
        thread::Builder::new()
            .name("psu-link".into())
            .spawn(move || worker.run())
            .expect("spawn psu-link thread");
        Self { shared, wake }
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.snapshot).clone()
    }

    pub fn rescan(&self) {
        let _ = self.wake.send(Wake::Rescan);
    }

    /// Runs `f` on the connected device off the async runtime (serial I/O blocks), then
    /// triggers a poll so the change shows up immediately.
    pub async fn with_psu<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn Psu) -> psu::Result<T> + Send + 'static,
    {
        let this = self.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let result = match lock(&this.shared.psu).as_mut() {
                Some(psu) => f(psu.as_mut()).map_err(|e| e.to_string()),
                None => Err("no supply connected".to_string()),
            };
            let _ = this.wake.send(Wake::Poll);
            result
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Picks which detected supply to connect to after a scan. `found` holds every supply that
/// answered (ports already closed again); `last` is the device from the previous
/// connection, if any. `Err` carries the reason shown in the status bar, and the worker
/// stays disconnected.
///
/// Called only after the fast path failed, i.e. `last` (if any) didn't answer on its own
/// port. It may still appear in `found` under a different port (USB re-enumeration).
fn choose(found: Vec<Detected>, last: Option<&KnownDevice>) -> Result<Detected, String> {
    // TODO(human)
    todo!()
}

struct Worker {
    app: AppHandle,
    shared: Arc<Shared>,
    wake: Receiver<Wake>,
    last: Option<KnownDevice>,
    last_device_path: PathBuf,
    /// Ports present at the last scan, so retries only probe newcomers.
    seen_ports: BTreeSet<String>,
    failures: u32,
}

impl Worker {
    fn run(mut self) {
        self.search(true);
        loop {
            let interval = if self.connected() { POLL_INTERVAL } else { RETRY_INTERVAL };
            match self.wake.recv_timeout(interval) {
                Ok(Wake::Rescan) if !self.connected() => self.search(true),
                Ok(_) | Err(RecvTimeoutError::Timeout) if self.connected() => self.poll(),
                Ok(_) | Err(RecvTimeoutError::Timeout) => self.search(false),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn connected(&self) -> bool {
        lock(&self.shared.psu).is_some()
    }

    /// Looks for a supply. `full` probes every candidate port; otherwise only the
    /// last-known port and ports that appeared since the previous search.
    fn search(&mut self, full: bool) {
        let ports = match drivers::candidate_ports() {
            Ok(p) => p,
            Err(e) => return self.disconnected(format!("can't list serial ports: {e}")),
        };
        let fresh: Vec<String> =
            ports.iter().filter(|p| full || !self.seen_ports.contains(*p)).cloned().collect();
        self.seen_ports = ports.iter().cloned().collect();

        // Fast path: the last-known port with its own driver, touching nothing else.
        if let Some(last) = self.last.clone()
            && ports.contains(&last.port)
            && let Some(driver) = drivers::find(&last.driver)
        {
            if full {
                self.set_link(LinkState::Searching { last: Some(last.clone()) });
            }
            match drivers::probe_port(&last.port, &[driver]) {
                Ok(psu) => return self.connect(psu, last.port),
                Err(e) => log::info!("last device not on {}: {e}", last.port),
            }
        }

        let last_port = self.last.as_ref().map(|d| d.port.as_str());
        let candidates: Vec<String> =
            fresh.into_iter().filter(|p| Some(p.as_str()) != last_port).collect();
        if candidates.is_empty() {
            if full || !matches!(self.snapshot_link(), LinkState::Disconnected { .. }) {
                self.disconnected("no supply found".into());
            }
            return;
        }

        self.set_link(LinkState::Searching { last: self.last.clone() });
        let found = drivers::detect_on(&candidates, None);
        let picked = choose(found, self.last.as_ref());
        match picked.and_then(|d| d.open().map(|psu| (psu, d.port)).map_err(|e| e.to_string())) {
            Ok((psu, port)) => self.connect(psu, port),
            Err(reason) => self.disconnected(reason),
        }
    }

    fn connect(&mut self, psu: Box<dyn Psu>, port: String) {
        let info = psu.info().clone();
        let device = KnownDevice { port, driver: info.driver.into(), model: info.model };
        if self.last.as_ref() != Some(&device) {
            if let Err(e) = persist::save(&self.last_device_path, &device) {
                log::warn!("can't save {}: {e}", self.last_device_path.display());
            }
            self.last = Some(device.clone());
        }
        *lock(&self.shared.psu) = Some(psu);
        self.failures = 0;
        self.set_link(LinkState::Connected { device, limits: info.limits });
        self.poll();
    }

    fn poll(&mut self) {
        let result = match lock(&self.shared.psu).as_mut() {
            Some(psu) => psu.status(),
            None => return,
        };
        match result {
            Ok(status) => {
                self.failures = 0;
                lock(&self.shared.snapshot).status = Some(status.clone());
                let _ = self.app.emit(EVENT_STATUS, status);
            }
            Err(e) => {
                self.failures += 1;
                log::warn!("poll failed ({}/{MAX_POLL_FAILURES}): {e}", self.failures);
                if self.failures >= MAX_POLL_FAILURES {
                    *lock(&self.shared.psu) = None;
                    self.disconnected(format!("link lost: {e}"));
                }
            }
        }
    }

    fn disconnected(&mut self, reason: String) {
        lock(&self.shared.snapshot).status = None;
        self.set_link(LinkState::Disconnected { last: self.last.clone(), reason });
    }

    fn snapshot_link(&self) -> LinkState {
        lock(&self.shared.snapshot).link.clone()
    }

    fn set_link(&self, link: LinkState) {
        lock(&self.shared.snapshot).link = link.clone();
        let _ = self.app.emit(EVENT_LINK, link);
    }
}
