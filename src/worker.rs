//! The single thread that owns all hidraw I/O.
//!
//! Blocking ioctls, retries and the battery poll live here; results are
//! published to the Qt thread through a `publish` callback (the Qt object
//! queues them onto the GUI thread).

use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::Duration;

use crate::protocol::{self, Error, Transport, BLOCK_LEN, DATA_LEN};
use crate::settings::{self, SettingsBlock, COMMIT_MARKER, COMMIT_OFFSET};

/// How often the idle poll reads the battery and checks whether the mouse
/// changed the profile itself (the DPI button moves the active level).
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub enum Command {
    /// Status, plus the settings block when none has been loaded yet.
    Refresh,
    /// Re-read psd/mode/status/block from the device.
    Reload,
    ApplyBlock { block: [u8; BLOCK_LEN], generation: u64 },
    SetMode(u8),
    SetDebounce(u8),
    Backup { path: String },
    Restore { path: String },
    /// Write the vendor defaults, saving the current state to `backup_path` first.
    FactoryReset { backup_path: String },
    /// The Qt side is done: finish the commands already queued, then stop.
    Shutdown(std::sync::mpsc::Sender<()>),
}

pub enum Event {
    Connected {
        path: String,
        pid: u16,
        psd: [u8; 4],
        mode: u8,
        bank: u8,
    },
    /// Why the device dropped out, e.g. "permission denied on hidraw node".
    Disconnected(String),
    /// `percent` is the charge to show, not necessarily the device's raw byte
    /// (see `charge_to_show`).
    Status {
        charging: bool,
        percent: Option<u8>,
    },
    Block {
        block: [u8; BLOCK_LEN],
        generation: u64,
        verified: bool,
    },
    Busy {
        busy: bool,
        label: String,
    },
    Note(String),
    Mode(u8),
    Error(String),
}

pub struct Handle {
    pub tx: Sender<Command>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        // A dead worker thread only happens while shutting down.
        let _ = self.tx.send(command);
    }

    /// Let the commands already queued finish: the invokables only enqueue an
    /// edit, so quitting would otherwise drop it. Bounded, so a stalled device
    /// cannot hold the exit.
    pub fn drain(&self, timeout: Duration) {
        let (done, finished) = std::sync::mpsc::channel();
        if self.tx.send(Command::Shutdown(done)).is_err() {
            return;
        }
        let _ = finished.recv_timeout(timeout);
    }
}

/// Spawn the device thread; `publish` is called for every event. A thread that
/// cannot be started is reported to the caller rather than leaving an app that
/// silently does nothing.
pub fn spawn(publish: impl Fn(Event) + Send + 'static) -> std::io::Result<Handle> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("igm5000-device".into())
        .spawn(move || run(rx, publish))?;
    Ok(Handle { tx })
}

struct Session {
    transport: Option<Transport>,
    path: String,
    pid: u16,
    psd: [u8; 4],
    bank: u8,
    /// Whether a successful read confirmed `bank`: a probe that failed at
    /// connect says nothing about which bank the unit has.
    bank_confirmed: bool,
    /// Whether the single polkit prompt has been used up for this session.
    access_requested: bool,
    generation: u64,
    /// The last state of charge the device reported while on battery.
    last_percent: Option<u8>,
    /// The last block the device confirmed, to spot active-level edits.
    last_block: Option<SettingsBlock>,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            transport: None,
            path: String::new(),
            pid: 0,
            psd: [0; 4],
            bank: 2,
            bank_confirmed: false,
            access_requested: false,
            generation: 0,
            last_percent: None,
            last_block: None,
        }
    }
}

impl Session {
    fn drop_transport(&mut self) {
        self.transport = None;
        self.path.clear();
        self.pid = 0;
    }

    /// Discovery + open. On failure the session is left disconnected.
    fn connect(&mut self, publish: &dyn Fn(Event)) -> bool {
        if self.transport.is_some() {
            return true;
        }
        match self.open(publish) {
            Ok(()) => true,
            Err(e) => {
                self.drop_transport();
                publish(Event::Disconnected(e.to_string()));
                false
            }
        }
    }

    fn open(&mut self, publish: &dyn Fn(Event)) -> Result<(), Error> {
        let (path, pid) = protocol::find_control_node()?;
        let mut transport = match Transport::open_path(&path) {
            Ok(transport) => transport,
            // The node is root-only until the udev rule is installed: ask polkit
            // for read/write once per run, then use it. A poll that keeps failing
            // does not prompt again.
            Err(Error::PermissionDenied) if !self.access_requested => {
                self.access_requested = true;
                protocol::grant_access(&path)?;
                Transport::open_path(&path)?
            }
            Err(e) => return Err(e),
        };
        let psd = transport.psd()?;
        let mode = transport.mode()?;
        // Which bank the unit has: its PID implies one, but a stalled read says
        // nothing about which exist, so try the other before believing it is
        // single-profile.
        let primary = if pid == 0x002e { 1 } else { 2 };
        let other = if primary == 1 { 2 } else { 1 };
        let (bank, probe) = match transport.read_block(primary) {
            Ok(block) => (primary, Some(block)),
            Err(_) => match transport.read_block(other) {
                Ok(block) => (other, Some(block)),
                Err(_) => (primary, None),
            },
        };
        self.path = path;
        self.pid = pid;
        self.psd = psd;
        self.bank = bank;
        self.bank_confirmed = probe.is_some();
        self.last_block = probe.map(SettingsBlock::new);
        self.transport = Some(transport);
        self.publish_connected(publish, mode);
        // Hand it over now: the consumer has no other copy, and waiting for a
        // change would leave the UI on a placeholder it could then write back.
        if let Some(block) = probe {
            publish(Event::Block {
                block,
                generation: self.generation,
                verified: true,
            });
        }
        Ok(())
    }

    fn publish_connected(&self, publish: &dyn Fn(Event), mode: u8) {
        publish(Event::Connected {
            path: self.path.clone(),
            pid: self.pid,
            psd: self.psd,
            mode,
            bank: self.bank,
        });
        publish(Event::Mode(mode));
    }

    fn transport(&mut self) -> Option<&mut Transport> {
        self.transport.as_mut()
    }

    /// Report an operation failure: a dead handle is dropped so the next
    /// command re-discovers the device; transient errors are reported instead.
    fn report(&mut self, context: &str, error: Error, publish: &dyn Fn(Event)) {
        if error.is_fatal() {
            let reason = error.to_string();
            self.drop_transport();
            publish(Event::Disconnected(reason));
        } else {
            publish(Event::Error(format!("{context}: {error}")));
        }
    }

    /// Battery, plus anything the mouse changed on its own: pressing the DPI
    /// button writes the new active level into the profile block.
    fn poll(&mut self, publish: &dyn Fn(Event)) {
        if self.transport.is_none() && !self.connect(publish) {
            return;
        }
        let status = match self.transport() {
            Some(t) => t.status(),
            None => return,
        };
        match status {
            Ok(status) => self.publish_status(status, publish),
            Err(e) => {
                self.report("battery read failed", e, publish);
                return;
            }
        }
        match self.read_block() {
            Ok(block) => {
                if self.last_block.as_ref().map(SettingsBlock::raw) != Some(&block) {
                    self.last_block = Some(SettingsBlock::new(block));
                    publish(Event::Block {
                        block,
                        generation: self.generation,
                        verified: true,
                    });
                }
            }
            Err(e) => self.report("read settings failed", e, publish),
        }
    }

    fn read_block(&mut self) -> Result<[u8; BLOCK_LEN], Error> {
        let bank = self.bank;
        match self.read_bank(bank) {
            Ok(block) => {
                self.bank_confirmed = true;
                Ok(block)
            }
            Err(e) if self.bank_confirmed => Err(e),
            Err(e) => {
                // Never confirmed by a read, so it may not be the live bank.
                let other = if bank == 1 { 2 } else { 1 };
                match self.read_bank(other) {
                    Ok(block) => {
                        self.bank = other;
                        self.bank_confirmed = true;
                        Ok(block)
                    }
                    Err(_) => Err(e),
                }
            }
        }
    }

    fn read_bank(&mut self, bank: u8) -> Result<[u8; BLOCK_LEN], Error> {
        match self.transport() {
            Some(t) => t.read_block(bank),
            None => Err(Error::NoDevice),
        }
    }

    /// Show the device's charge, remembering the last reading that was real.
    fn publish_status(&mut self, status: protocol::Status, publish: &dyn Fn(Event)) {
        self.last_percent = charge_to_show(self.last_percent, status);
        publish(Event::Status {
            charging: status.charging(),
            percent: self.last_percent,
        });
    }

    fn reload(&mut self, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        let mode = self.transport().and_then(|t| t.mode().ok());
        let status = self.transport().and_then(|t| t.status().ok());
        let block = self.read_block();
        if let Some(mode) = mode {
            publish(Event::Mode(mode));
        }
        if let Some(status) = status {
            self.publish_status(status, publish);
        }
        match block {
            Ok(block) => {
                self.last_block = Some(SettingsBlock::new(block));
                publish(Event::Block {
                    block,
                    generation: self.generation,
                    verified: true,
                })
            }
            Err(e) => self.report("read settings failed", e, publish),
        }
    }

    fn refresh(&mut self, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        self.poll(publish);
    }

    /// Handle one command; `false` means the thread should stop.
    fn handle(&mut self, command: Command, publish: &dyn Fn(Event)) -> bool {
        match command {
            Command::Refresh => self.refresh(publish),
            Command::Reload => self.reload(publish),
            Command::ApplyBlock { block, generation } => {
                self.apply_block(block, generation, publish);
            }
            Command::SetMode(mode) => self.set_mode(mode, publish),
            Command::SetDebounce(value) => self.set_debounce(value, publish),
            Command::Backup { path } => self.backup(&path, publish),
            Command::Restore { path } => self.restore(&path, publish),
            Command::FactoryReset { backup_path } => self.factory_reset(&backup_path, publish),
            // Everything queued before this one has been applied.
            Command::Shutdown(done) => {
                let _ = done.send(());
                return false;
            }
        }
        true
    }

    /// Write a block and read it back. Returns whether the device confirmed the
    /// exact bytes: a caller that announces success (a factory reset) must not
    /// claim one the device refused.
    fn apply_block(
        &mut self,
        mut block: [u8; BLOCK_LEN],
        generation: u64,
        publish: &dyn Fn(Event),
    ) -> bool {
        // Adopt the generation before anything can fail: the Qt side already
        // advanced its own, and a stranded counter makes later blocks look stale.
        self.generation = generation;
        if !self.connect(publish) {
            return false;
        }
        // Every write here is a profile save.
        block[COMMIT_OFFSET] = COMMIT_MARKER;
        publish(Event::Busy {
            busy: true,
            label: "Writing settings".into(),
        });
        let bank = self.bank;
        let written = match self.transport().map(|t| t.write_block(bank, &block)) {
            Some(result) => result,
            None => Err(Error::NoDevice),
        };
        let outcome = written.and_then(|()| self.read_block());
        publish(Event::Busy {
            busy: false,
            label: String::new(),
        });
        let previous = self.last_block.clone();
        match outcome {
            Ok(read_back) => {
                let next = SettingsBlock::new(read_back);
                let verified = read_back == block;
                let refresh = verified
                    && previous
                        .as_ref()
                        .is_some_and(|previous| needs_refresh(previous, &next));
                self.last_block = Some(next);
                publish(Event::Block {
                    block: read_back,
                    generation,
                    verified,
                });
                if refresh {
                    // Switch away and back so the mouse re-reads the level.
                    self.refresh_active_level(&block, publish);
                }
                verified
            }
            Err(e) => {
                self.report("write settings failed", e, publish);
                false
            }
        }
    }

    /// Re-select the active level so the mouse re-reads its colour and DPI.
    /// The intermediate level is only selected for the length of one write.
    fn refresh_active_level(&mut self, block: &[u8; BLOCK_LEN], publish: &dyn Fn(Event)) {
        let count = block[3] & 0x0f;
        if count < 2 {
            return;
        }
        // Level numbers in the block are 1-based.
        let active = (block[3] >> 4).max(1);
        let other = if active == 1 { 2 } else { 1 };
        let mut scratch = *block;
        scratch[3] = (other << 4) | count;
        let bank = self.bank;
        let Some(transport) = self.transport.as_mut() else {
            return;
        };
        if transport.write_block(bank, &scratch).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(60));
        if transport.write_block(bank, block).is_err() {
            // The mouse is left on the intermediate level - the one thing this
            // function exists to prevent - so say so instead of claiming the
            // block it handed back is live.
            publish(Event::Error(
                "the mouse did not take the active level back; press its DPI button to re-select it"
                    .into(),
            ));
        }
    }

    /// Restore the vendor defaults, saving the current state first: a reset has
    /// to be undoable.
    fn factory_reset(&mut self, backup_path: &str, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        publish(Event::Busy {
            busy: true,
            label: "Restoring factory defaults".into(),
        });
        let bank = self.bank;
        let raw = match self.transport() {
            Some(t) => t.read_report(bank),
            None => Err(Error::NoDevice),
        };
        match raw {
            Ok(raw) => {
                if let Err(e) = std::fs::write(backup_path, raw) {
                    publish(Event::Busy {
                        busy: false,
                        label: String::new(),
                    });
                    publish(Event::Error(format!(
                        "factory reset cancelled, could not save {backup_path}: {e}"
                    )));
                    return;
                }
            }
            Err(e) => {
                publish(Event::Busy {
                    busy: false,
                    label: String::new(),
                });
                self.report("factory reset cancelled, read failed", e, publish);
                return;
            }
        }

        let current = match self.read_block() {
            Ok(current) => SettingsBlock::new(current),
            Err(e) => {
                publish(Event::Busy {
                    busy: false,
                    label: String::new(),
                });
                self.report("factory reset cancelled, read failed", e, publish);
                return;
            }
        };
        let factory = SettingsBlock::factory(&current);
        let generation = self.generation + 1;
        if !self.apply_block(*factory.raw(), generation, publish) {
            publish(Event::Error(
                "factory defaults were not accepted by the device".into(),
            ));
            return;
        }
        self.set_mode(2, publish);
        publish(Event::Note(format!(
            "Factory defaults written (previous settings saved to {backup_path})"
        )));
    }

    fn set_mode(&mut self, mode: u8, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        let result = match self.transport().map(|t| t.set_mode(mode)) {
            Some(result) => result,
            None => Err(Error::NoDevice),
        };
        match result {
            Ok(()) => match self.transport().and_then(|t| t.mode().ok()) {
                Some(read_back) => publish(Event::Mode(read_back)),
                None => publish(Event::Note(format!("Mode {mode} sent (no read-back)"))),
            },
            Err(e) => self.report("set mode failed", e, publish),
        }
    }

    fn set_debounce(&mut self, value: u8, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        let result = match self.transport().map(|t| t.set_debounce(value)) {
            Some(result) => result,
            None => Err(Error::NoDevice),
        };
        match result {
            Ok(()) => publish(Event::Note(format!(
                "Debounce register set to {value} (write-only, not in a backup)"
            ))),
            Err(e) => self.report("set debounce failed", e, publish),
        }
    }

    fn backup(&mut self, path: &str, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        publish(Event::Busy {
            busy: true,
            label: "Reading backup".into(),
        });
        let bank = self.bank;
        let raw: Result<[u8; DATA_LEN], Error> = match self.transport() {
            Some(t) => t.read_report(bank),
            None => Err(Error::NoDevice),
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(e) => {
                publish(Event::Busy {
                    busy: false,
                    label: String::new(),
                });
                // A device error, not a file error: this may kill the handle.
                self.report("backup failed", e, publish);
                return;
            }
        };
        let outcome = std::fs::write(path, raw).map_err(|e| e.to_string());
        publish(Event::Busy {
            busy: false,
            label: String::new(),
        });
        match outcome {
            Ok(()) => publish(Event::Note(format!(
                "Backup of bank {bank} written to {path}"
            ))),
            Err(e) => publish(Event::Error(format!("backup failed: {e}"))),
        }
    }

    fn restore(&mut self, path: &str, publish: &dyn Fn(Event)) {
        if !self.connect(publish) {
            return;
        }
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                publish(Event::Error(format!("restore failed: {e}")));
                return;
            }
        };
        // The file is a raw data report whose block is written verbatim, so only
        // exactly that shape is accepted.
        if data.len() != DATA_LEN {
            publish(Event::Error(format!(
                "restore failed: {path} is {} bytes, not a {DATA_LEN}-byte data report",
                data.len()
            )));
            return;
        }
        if data[0] != protocol::RID_DATA {
            publish(Event::Error(format!(
                "restore failed: {path} does not start with data report {:#04x}",
                protocol::RID_DATA
            )));
            return;
        }
        let mut block = [0u8; BLOCK_LEN];
        block.copy_from_slice(&data[protocol::BLOCK_OFFSET..protocol::BLOCK_OFFSET + BLOCK_LEN]);
        // The level nibbles have to describe a profile this app can represent.
        let levels = block[3] & 0x0f;
        let active = block[3] >> 4;
        if levels == 0 || levels > settings::LEVELS || active == 0 || active > levels {
            publish(Event::Error(format!(
                "restore failed: {path} describes {levels} levels with level {active} active"
            )));
            return;
        }
        // Everything else is restored verbatim; the DPI codes are clamped into the
        // sensor's table so the device cannot hold a state the UI cannot show.
        let mut restored = SettingsBlock::new(block);
        let last_code = settings::DPI_STEPS.len() as u16;
        for level in 0..settings::LEVELS {
            let code = restored.dpi_code(level).min(last_code);
            restored.set_dpi_code(level, code);
        }
        if self.apply_block(*restored.raw(), self.generation, publish) {
            publish(Event::Note(format!("Settings restored from {path}")));
        } else {
            publish(Event::Error(format!("restore of {path} failed")));
        }
    }
}

/// Whether the active level's own settings changed.
fn needs_refresh(previous: &SettingsBlock, next: &SettingsBlock) -> bool {
    let level = next.active_index();
    previous.color(level) != next.color(level) || previous.dpi_code(level) != next.dpi_code(level)
}

/// The charge to show: the device reports a real state of charge only while on
/// battery (charging it sent 2 for ten minutes, then 92 when unplugged), so the
/// last reading taken on battery is kept - `None` until there is one.
fn charge_to_show(previous: Option<u8>, status: protocol::Status) -> Option<u8> {
    if status.charging() {
        previous
    } else {
        Some(status.percent)
    }
}

fn run(rx: std::sync::mpsc::Receiver<Command>, publish: impl Fn(Event)) {
    let mut session = Session::default();
    let publish: &dyn Fn(Event) = &publish;
    loop {
        // Without this, a panic in one command would leave the app looking
        // alive while every later edit was dropped on the floor.
        let step = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(command) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                session.handle(command, publish)
            })),
            // Battery poll plus a look for device-side profile changes.
            Err(RecvTimeoutError::Timeout) => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.poll(publish);
                    true
                }))
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match step {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                publish(Event::Error(
                    "internal error in the device thread; restart the app".into(),
                ));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: u8, percent: u8) -> protocol::Status {
        protocol::Status { state, percent }
    }

    #[test]
    fn the_charge_shown_while_charging_is_the_last_battery_reading() {
        // Measured on the device: 2 for ten minutes while charging, then 92 the
        // moment it was unplugged - so the charging value is not a charge.
        assert_eq!(charge_to_show(None, status(0x10, 2)), None);
        assert_eq!(charge_to_show(Some(92), status(0x10, 2)), Some(92));

        // On battery the device's own reading is the charge.
        assert_eq!(charge_to_show(None, status(0x11, 92)), Some(92));
        assert_eq!(charge_to_show(Some(92), status(0x11, 46)), Some(46));
    }
}
