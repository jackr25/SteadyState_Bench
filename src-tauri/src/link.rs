//! Owns the connection to the supply: startup autodetect, status polling, and reconnecting
//! after the link drops. A worker thread does all of it and reports through events; Tauri
//! commands borrow the open device through [`Link::with_psu`].
//!
//! Background retries are deliberately narrow. Probing writes to whatever is on the port (a
//! Modbus frame, a `*IDN?`), and merely opening some ports resets the device behind them
//! (Arduino DTR), so while disconnected we only retry the last-known port and probe ports
//! that newly appeared. A full scan happens at startup and when the user asks for one.
//!
//! A scan that turns up anything other than the supply used last time doesn't connect on
//! its own: the link goes to [`LinkState::Choosing`] and the frontend asks the user, who
//! answers with [`Link::pick`] or [`Link::dismiss`].

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
    /// A scan found a new supply, or several: waiting for the user to pick one. `suggested`
    /// indexes `found`. Background retries pause until the user answers.
    Choosing { found: Vec<Detected>, suggested: usize, last: Option<KnownDevice> },
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
    /// The user picked a supply from the [`LinkState::Choosing`] list.
    Pick { port: String, driver: String },
    /// The user declined to connect to any of the listed supplies.
    Dismiss,
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

    /// Connects to one of the supplies offered by [`LinkState::Choosing`]. The outcome
    /// arrives as a link event; `Err` only means the choice no longer applies.
    pub fn pick(&self, port: String, driver: String) -> Result<(), String> {
        match &lock(&self.shared.snapshot).link {
            LinkState::Choosing { found, .. } if offered(found, &port, &driver).is_some() => {}
            LinkState::Choosing { .. } => return Err(format!("{port} is no longer on offer")),
            _ => return Err("no supply choice is pending".into()),
        }
        let _ = self.wake.send(Wake::Pick { port, driver });
        Ok(())
    }

    /// Declines the pending choice; stays disconnected until a rescan.
    pub fn dismiss(&self) {
        let _ = self.wake.send(Wake::Dismiss);
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

/// What to do with the supplies a scan found.
#[derive(Debug)]
enum Choice {
    /// The supply used last time, back under a new port name: reconnect without asking.
    Reconnect(Detected),
    /// Something new, or several candidates: let the user pick. `suggested` indexes `found`.
    Ask { found: Vec<Detected>, suggested: usize },
    /// Nothing answered; the reason for the status bar.
    Nothing(String),
}

/// Decides what to do after a scan. `found` holds every supply that answered (ports
/// already closed again); `last` is the device from the previous connection, if any.
///
/// Called only after the fast path failed, i.e. `last` (if any) didn't answer on its own
/// port. It may still appear in `found` under a different port (USB re-enumeration), and
/// is then reconnected silently, as the fast path would have, even if other supplies were
/// found too. Anything else needs the user: connecting to a supply we haven't used before
/// could put the controls on the wrong bench. Several units of the same model can't be told
/// apart (the model string carries no serial number), so a second match also means asking.
fn choose(mut found: Vec<Detected>, last: Option<&KnownDevice>) -> Choice {
    if found.is_empty() {
        return Choice::Nothing("no supply found".into());
    }
    let is_last = |d: &Detected| {
        last.is_some_and(|l| l.driver == d.info.driver && l.model == d.info.model)
    };
    let matches: Vec<usize> = (0..found.len()).filter(|&i| is_last(&found[i])).collect();
    match matches[..] {
        [i] => Choice::Reconnect(found.swap_remove(i)),
        _ => Choice::Ask { suggested: matches.first().copied().unwrap_or(0), found },
    }
}

/// Finds the offered supply matching a pick.
fn offered<'a>(found: &'a [Detected], port: &str, driver: &str) -> Option<&'a Detected> {
    found.iter().find(|d| d.port == port && d.info.driver == driver)
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
                Ok(Wake::Pick { port, driver }) => self.pick(&port, &driver),
                Ok(Wake::Dismiss) => self.dismiss(),
                Ok(Wake::Rescan) if !self.connected() => self.search(true),
                Ok(_) | Err(RecvTimeoutError::Timeout) if self.connected() => self.poll(),
                // Waiting on the user: a background search would replace the question.
                Ok(_) | Err(RecvTimeoutError::Timeout) if self.choosing() => {}
                Ok(_) | Err(RecvTimeoutError::Timeout) => self.search(false),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn connected(&self) -> bool {
        lock(&self.shared.psu).is_some()
    }

    fn choosing(&self) -> bool {
        matches!(self.snapshot_link(), LinkState::Choosing { .. })
    }

    fn pick(&mut self, port: &str, driver: &str) {
        let LinkState::Choosing { found, .. } = self.snapshot_link() else {
            return log::info!("ignoring pick of {port}: no choice pending");
        };
        let Some(chosen) = offered(&found, port, driver).cloned() else {
            return log::info!("ignoring pick of {port}: not on offer");
        };
        // Shows "connecting" for the chosen supply while its port is reopened.
        let device = KnownDevice {
            port: chosen.port.clone(),
            driver: chosen.info.driver.into(),
            model: chosen.info.model.clone(),
        };
        self.set_link(LinkState::Searching { last: Some(device) });
        self.open_detected(chosen);
    }

    fn dismiss(&mut self) {
        if self.choosing() {
            // The offered ports stay in `seen_ports`, so background retries won't offer
            // them again; a rescan will.
            self.disconnected("not connected; rescan to choose a supply".into());
        }
    }

    fn open_detected(&mut self, d: Detected) {
        match d.open() {
            Ok(psu) => self.connect(psu, d.port),
            Err(e) => self.disconnected(format!("can't open {}: {e}", d.port)),
        }
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
        match choose(found, self.last.as_ref()) {
            Choice::Reconnect(d) => self.open_detected(d),
            Choice::Ask { found, suggested } => {
                lock(&self.shared.snapshot).status = None;
                self.set_link(LinkState::Choosing { found, suggested, last: self.last.clone() });
            }
            Choice::Nothing(reason) => self.disconnected(reason),
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

#[cfg(test)]
mod tests {
    use super::*;
    use psu_core::psu::DeviceInfo;

    const KORAD: &str = "korad-kaxxxxp";
    const HANMATEK: &str = "hanmatek-hm310t";

    fn found(port: &str, driver: &'static str, model: &str) -> Detected {
        Detected {
            port: port.into(),
            info: DeviceInfo {
                driver,
                model: model.into(),
                limits: Limits { voltage: 30.0, current: 5.0, power: 150.0 },
            },
        }
    }

    fn known(port: &str, driver: &str, model: &str) -> KnownDevice {
        KnownDevice { port: port.into(), driver: driver.into(), model: model.into() }
    }

    fn ports(found: &[Detected]) -> Vec<&str> {
        found.iter().map(|d| d.port.as_str()).collect()
    }

    #[test]
    fn nothing_found() {
        let last = known("COM3", KORAD, "KA3005P");
        assert!(matches!(choose(vec![], Some(&last)), Choice::Nothing(_)));
        assert!(matches!(choose(vec![], None), Choice::Nothing(_)));
    }

    #[test]
    fn last_device_on_new_port_reconnects_silently() {
        let last = known("COM3", KORAD, "KA3005P");
        let got = choose(vec![found("COM7", KORAD, "KA3005P")], Some(&last));
        assert!(matches!(got, Choice::Reconnect(d) if d.port == "COM7"));
    }

    #[test]
    fn last_device_wins_over_newcomers() {
        let last = known("COM3", KORAD, "KA3005P");
        let all = vec![found("COM5", HANMATEK, "HM310T"), found("COM7", KORAD, "KA3005P")];
        assert!(matches!(choose(all, Some(&last)), Choice::Reconnect(d) if d.port == "COM7"));
    }

    #[test]
    fn first_run_asks_even_for_one_supply() {
        let got = choose(vec![found("COM5", HANMATEK, "HM310T")], None);
        assert!(matches!(got, Choice::Ask { ref found, suggested: 0 } if ports(found) == ["COM5"]));
    }

    #[test]
    fn different_supply_asks() {
        let last = known("COM3", KORAD, "KA3005P");
        let got = choose(vec![found("COM5", HANMATEK, "HM310T")], Some(&last));
        assert!(matches!(got, Choice::Ask { suggested: 0, .. }));
    }

    #[test]
    fn same_model_different_driver_is_not_last() {
        let last = known("COM3", HANMATEK, "KA3005P");
        let got = choose(vec![found("COM7", KORAD, "KA3005P")], Some(&last));
        assert!(matches!(got, Choice::Ask { .. }));
    }

    #[test]
    fn several_new_supplies_ask_in_scan_order() {
        let all = vec![found("COM5", HANMATEK, "HM310T"), found("COM7", KORAD, "KA3005P")];
        let got = choose(all, None);
        assert!(matches!(
            got,
            Choice::Ask { ref found, suggested: 0 } if ports(found) == ["COM5", "COM7"]
        ));
    }

    #[test]
    fn identical_twins_of_last_ask_and_suggest_the_first() {
        let last = known("COM3", KORAD, "KA3005P");
        let all = vec![
            found("COM5", HANMATEK, "HM310T"),
            found("COM7", KORAD, "KA3005P"),
            found("COM9", KORAD, "KA3005P"),
        ];
        assert!(matches!(choose(all, Some(&last)), Choice::Ask { suggested: 1, .. }));
    }

    #[test]
    fn offered_matches_port_and_driver() {
        let all = vec![found("COM5", HANMATEK, "HM310T"), found("COM7", KORAD, "KA3005P")];
        assert_eq!(offered(&all, "COM7", KORAD).map(|d| d.port.as_str()), Some("COM7"));
        assert!(offered(&all, "COM7", HANMATEK).is_none());
        assert!(offered(&all, "COM9", KORAD).is_none());
    }

    /// The frontend (`src/lib/psu.svelte.ts`) reads this shape.
    #[test]
    fn choosing_serializes_for_the_frontend() {
        let link = LinkState::Choosing {
            found: vec![found("COM7", KORAD, "KA3005P")],
            suggested: 0,
            last: None,
        };
        let json = serde_json::to_value(&link).unwrap();
        assert_eq!(json["kind"], "choosing");
        assert_eq!(json["suggested"], 0);
        assert_eq!(json["last"], serde_json::Value::Null);
        assert_eq!(json["found"][0]["port"], "COM7");
        assert_eq!(json["found"][0]["info"]["driver"], KORAD);
        assert_eq!(json["found"][0]["info"]["limits"]["voltage"], 30.0);
    }
}
