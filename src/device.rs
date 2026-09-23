//! cxx-qt bridge: the only place where the Rust/Qt boundary is declared.
//!
//! The Rust side owns the `Device` qobject (state + invokables), the C++ side
//! (`cpp/shell.cpp`) owns the widgets and drives this object.

use core::pin::Pin;
use std::time::Duration;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

use crate::protocol::{self, BLOCK_LEN};
use crate::settings::{self, SettingsBlock};
use crate::worker::{self, Command, Event, Handle};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        // Deliberately declared outside any namespace: `QString` keeps its
        // global C++ name, which is what cxx-qt-lib and Qt declare.
        type QString = cxx_qt_lib::QString;

        include!("cpp/bridge.h");
        fn igm5000_run() -> i32;
    }

    // `auto_cxx_name` keeps the Rust field names snake_case while the C++ side
    // sees the idiomatic `statusText` / `deviceInfo` property names.
    #[namespace = "igm5000"]
    #[auto_cxx_name = Camel]
    extern "RustQt" {
        #[qobject]
        #[qproperty(bool, connected)]
        #[qproperty(i32, battery)]
        #[qproperty(bool, charging)]
        #[qproperty(i32, mode)]
        #[qproperty(QString, status_text)]
        #[qproperty(QString, device_info)]
        type Device = super::DeviceRust;

        #[qinvokable]
        #[cxx_name = "start"]
        fn start(self: Pin<&mut Self>);
        #[qinvokable]
        #[cxx_name = "refresh"]
        fn refresh(self: Pin<&mut Self>);
        #[qinvokable]
        #[cxx_name = "reload"]
        fn reload(self: Pin<&mut Self>);
        #[qinvokable]
        #[cxx_name = "applyRate"]
        fn apply_rate(self: Pin<&mut Self>, hz: i32);
        #[qinvokable]
        #[cxx_name = "applyDpi"]
        fn apply_dpi(self: Pin<&mut Self>, level: i32, dpi: i32);
        #[qinvokable]
        #[cxx_name = "setLevelEnabled"]
        fn set_level_enabled(self: Pin<&mut Self>, level: i32, enabled: bool);
        #[qinvokable]
        #[cxx_name = "applyColor"]
        fn apply_color(self: Pin<&mut Self>, level: i32, hex: &QString);
        #[qinvokable]
        #[cxx_name = "applyLod"]
        fn apply_lod(self: Pin<&mut Self>, value: i32);
        #[qinvokable]
        #[cxx_name = "sendMode"]
        fn send_mode(self: Pin<&mut Self>, mode: i32);
        #[qinvokable]
        #[cxx_name = "sendDebounce"]
        fn send_debounce(self: Pin<&mut Self>, value: i32);
        #[qinvokable]
        #[cxx_name = "backup"]
        fn backup(self: Pin<&mut Self>, path: &QString);
        #[qinvokable]
        #[cxx_name = "restore"]
        fn restore(self: Pin<&mut Self>, path: &QString);
        #[qinvokable]
        #[cxx_name = "factoryReset"]
        fn factory_reset(self: Pin<&mut Self>, backup_path: &QString);
        /// Waits for the commands already queued to reach the device; used on
        /// the way out so a debounced edit is not lost.
        #[qinvokable]
        #[cxx_name = "shutdown"]
        fn shutdown(self: Pin<&mut Self>);
        // Getters the view pulls after blockChanged()
        #[qinvokable]
        #[cxx_name = "dpiStepCount"]
        fn dpi_step_count(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "dpiStep"]
        fn dpi_step(self: Pin<&mut Self>, index: i32) -> i32;
        #[qinvokable]
        #[cxx_name = "dpiValue"]
        fn dpi_value(self: Pin<&mut Self>, level: i32) -> i32;
        #[qinvokable]
        #[cxx_name = "dpiColor"]
        fn dpi_color(self: Pin<&mut Self>, level: i32) -> QString;
        #[qinvokable]
        #[cxx_name = "dpiEnabled"]
        fn dpi_enabled(self: Pin<&mut Self>, level: i32) -> bool;
        #[qinvokable]
        #[cxx_name = "rateHz"]
        fn rate_hz(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "currentLevel"]
        fn current_level(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "enabledLevels"]
        fn enabled_levels(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "lod"]
        fn lod(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "debounce"]
        fn debounce(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "bank"]
        fn bank(self: Pin<&mut Self>) -> i32;
        #[qinvokable]
        #[cxx_name = "blockHex"]
        fn block_hex(self: Pin<&mut Self>) -> QString;

        #[qsignal]
        #[cxx_name = "blockChanged"]
        fn block_changed(self: Pin<&mut Self>);
        #[qsignal]
        #[cxx_name = "errorOccurred"]
        fn error_occurred(self: Pin<&mut Self>, message: QString);
        #[qsignal]
        #[cxx_name = "notified"]
        fn notified(self: Pin<&mut Self>, message: QString);
    }

    impl cxx_qt::Threading for Device {}
}

/// The state the Qt object wraps. The field names are part of the property
/// contract: cxx-qt reads and writes them directly.
pub struct DeviceRust {
    connected: bool,
    battery: i32,
    charging: bool,
    mode: i32,
    status_text: QString,
    device_info: QString,

    block: SettingsBlock,
    block_loaded: bool,
    generation: u64,
    debounce_value: i32,
    bank_value: i32,
    worker: Option<Handle>,
}

impl Default for DeviceRust {
    fn default() -> Self {
        DeviceRust {
            connected: false,
            battery: -1,
            charging: false,
            mode: 0,
            status_text: QString::from("No device"),
            device_info: QString::default(),
            block: SettingsBlock::new([0; BLOCK_LEN]),
            block_loaded: false,
            generation: 0,
            debounce_value: 0,
            bank_value: 0,
            worker: None,
        }
    }
}

/// `#rrggbb` for a colour triple.
fn hex_of(rgb: [u8; 3]) -> QString {
    QString::from(&format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]))
}

/// Parse `#rrggbb` (and `rrggbb`); `None` for anything else.
fn parse_hex(hex: &QString) -> Option<[u8; 3]> {
    let text = hex.to_string();
    let text = text.trim().trim_start_matches('#');
    if text.len() != 6 {
        return None;
    }
    let mut rgb = [0u8; 3];
    for (i, byte) in rgb.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(rgb)
}

/// Quit path: how long to wait for the device thread to drain.
const WORKER_DRAIN: Duration = Duration::from_secs(3);

fn level_ok(level: i32) -> bool {
    (0..i32::from(settings::LEVELS)).contains(&level)
}

impl qobject::Device {
    fn inner(&self) -> &DeviceRust {
        <Self as CxxQtType>::rust(self)
    }

    fn inner_mut(self: Pin<&mut Self>) -> &mut DeviceRust {
        <Self as CxxQtType>::rust_mut(self).get_mut()
    }

    fn send(&self, command: Command) {
        if let Some(worker) = self.inner().worker.as_ref() {
            worker.send(command);
        }
    }

    /// Apply `edit` to the in-memory block and schedule one debounced write.
    fn edit_block(mut self: Pin<&mut Self>, edit: impl FnOnce(&mut SettingsBlock)) {
        let (block, generation) = {
            let rust = self.as_mut().inner_mut();
            edit(&mut rust.block);
            rust.generation += 1;
            (*rust.block.raw(), rust.generation)
        };
        self.send(Command::ApplyBlock { block, generation });
    }

    // -- invokables the C++ shell drives ------------------------------------

    pub fn start(mut self: Pin<&mut Self>) {
        if self.inner().worker.is_some() {
            return;
        }
        let qt_thread = self.qt_thread();
        let spawned = worker::spawn(move |event| {
            // A destroyed object is normal at shutdown.
            let _ = qt_thread.queue(move |device: Pin<&mut qobject::Device>| {
                device.handle_event(event);
            });
        });
        match spawned {
            Ok(worker) => {
                self.as_mut().inner_mut().worker = Some(worker);
                self.send(Command::Reload);
            }
            Err(e) => {
                // Without the thread every invokable is a silent no-op, which
                // looks exactly like a mouse that is not plugged in.
                let message = format!("could not start the device thread: {e}");
                self.as_mut().set_status_text(QString::from(&message));
                self.as_mut().error_occurred(QString::from(&message));
            }
        }
    }

    /// Wait for queued commands before exiting, so a debounced edit is not lost.
    pub fn shutdown(self: Pin<&mut Self>) {
        if let Some(worker) = self.inner().worker.as_ref() {
            worker.drain(WORKER_DRAIN);
        }
    }

    /// Refuse edits until a block has been read: before that the in-memory block
    /// is a placeholder, and writing it back would replace the profile with zeros.
    fn ensure_block_loaded(mut self: Pin<&mut Self>) -> bool {
        if self.inner().block_loaded {
            return true;
        }
        // The widget shell keeps the editors disabled until then; this covers
        // every other invoker of the qobject.
        self.as_mut()
            .notified(QString::from("Waiting for the device settings to load"));
        false
    }

    pub fn refresh(self: Pin<&mut Self>) {
        self.send(Command::Refresh);
    }

    pub fn reload(self: Pin<&mut Self>) {
        self.send(Command::Reload);
    }

    pub fn apply_rate(mut self: Pin<&mut Self>, hz: i32) {
        if !self.as_mut().ensure_block_loaded() {
            return;
        }
        let hz = hz as u16;
        self.as_mut().edit_block(move |block| block.set_rate_hz(hz));
    }

    pub fn apply_dpi(mut self: Pin<&mut Self>, level: i32, dpi: i32) {
        if !level_ok(level) || !self.as_mut().ensure_block_loaded() {
            return;
        }
        // 0 means "no value" only for a slot outside the profile.
        let code = if dpi > 0 {
            settings::dpi_to_code(dpi.clamp(0, i32::from(u16::MAX)) as u16)
        } else if level as u8 >= self.inner().block.enabled_levels() {
            0
        } else {
            settings::dpi_to_code(settings::DPI_STEPS[0])
        };
        self.as_mut()
            .edit_block(move |block| block.set_dpi_code(level as u8, code));
    }

    /// Removal shifts the later levels down: the device has no per-slot "disabled".
    pub fn set_level_enabled(mut self: Pin<&mut Self>, level: i32, enabled: bool) {
        if !level_ok(level) || !self.as_mut().ensure_block_loaded() {
            return;
        }
        let level = level as u8;
        self.as_mut().edit_block(move |block| {
            if enabled {
                block.add_level(level);
            } else {
                block.remove_level(level);
            }
        });
    }

    pub fn factory_reset(self: Pin<&mut Self>, backup_path: &QString) {
        self.send(Command::FactoryReset {
            backup_path: backup_path.to_string(),
        });
    }

    pub fn apply_color(mut self: Pin<&mut Self>, level: i32, hex: &QString) {
        if !level_ok(level) || !self.as_mut().ensure_block_loaded() {
            return;
        }
        let Some(rgb) = parse_hex(hex) else {
            return;
        };
        self.as_mut()
            .edit_block(move |block| block.set_color(level as u8, rgb));
    }

    pub fn apply_lod(mut self: Pin<&mut Self>, value: i32) {
        if !self.as_mut().ensure_block_loaded() {
            return;
        }
        let value = value.clamp(0, 255) as u8;
        self.as_mut().edit_block(move |block| block.set_lod(value));
    }

    pub fn send_mode(self: Pin<&mut Self>, mode: i32) {
        self.send(Command::SetMode(mode.clamp(1, 3) as u8));
    }

    pub fn send_debounce(mut self: Pin<&mut Self>, value: i32) {
        let value = value.clamp(0, 255) as u8;
        self.as_mut().inner_mut().debounce_value = i32::from(value);
        self.send(Command::SetDebounce(value));
    }

    pub fn backup(self: Pin<&mut Self>, path: &QString) {
        self.send(Command::Backup {
            path: path.to_string(),
        });
    }

    pub fn restore(self: Pin<&mut Self>, path: &QString) {
        self.send(Command::Restore {
            path: path.to_string(),
        });
    }

    pub fn dpi_step_count(self: Pin<&mut Self>) -> i32 {
        settings::DPI_STEPS.len() as i32
    }

    pub fn dpi_step(self: Pin<&mut Self>, index: i32) -> i32 {
        settings::DPI_STEPS
            .get(index.max(0) as usize)
            .copied()
            .map(i32::from)
            .unwrap_or(0)
    }

    pub fn dpi_value(self: Pin<&mut Self>, level: i32) -> i32 {
        if level_ok(level) {
            i32::from(self.inner().block.dpi_value(level as u8))
        } else {
            0
        }
    }

    pub fn dpi_color(self: Pin<&mut Self>, level: i32) -> QString {
        if level_ok(level) {
            hex_of(self.inner().block.color(level as u8))
        } else {
            QString::default()
        }
    }

    pub fn dpi_enabled(self: Pin<&mut Self>, level: i32) -> bool {
        // Whether the level is part of the profile, which is the level count -
        // not whether its slot happens to hold a DPI value.
        level_ok(level) && (level as u8) < self.inner().block.enabled_levels()
    }

    pub fn rate_hz(self: Pin<&mut Self>) -> i32 {
        i32::from(self.inner().block.rate_hz())
    }

    pub fn current_level(self: Pin<&mut Self>) -> i32 {
        // The UI compares this with its row index, so expose the 0-based index.
        i32::from(self.inner().block.active_index())
    }

    pub fn enabled_levels(self: Pin<&mut Self>) -> i32 {
        i32::from(self.inner().block.enabled_levels())
    }

    pub fn lod(self: Pin<&mut Self>) -> i32 {
        i32::from(self.inner().block.lod())
    }

    pub fn debounce(self: Pin<&mut Self>) -> i32 {
        self.inner().debounce_value
    }

    pub fn bank(self: Pin<&mut Self>) -> i32 {
        self.inner().bank_value
    }

    pub fn block_hex(self: Pin<&mut Self>) -> QString {
        QString::from(&self.inner().block.hex_dump())
    }

    /// The state word shown above the battery line.
    fn state_text(&self) -> QString {
        let inner = self.inner();
        if !inner.connected {
            QString::from("No device")
        } else if inner.charging {
            QString::from("Charging")
        } else {
            QString::from("On battery")
        }
    }

    // -- worker events (always on the Qt thread) -----------------------------

    fn handle_event(mut self: Pin<&mut Self>, event: Event) {
        match event {
            Event::Connected {
                path,
                pid,
                psd,
                mode,
                bank,
            } => {
                let psd_text = String::from_utf8_lossy(&psd).to_string();
                let info = format!(
                    "{path}  {:04x}:{pid:04x}  psd {psd_text}  mode {mode}  bank {bank}",
                    protocol::VENDOR
                );
                self.as_mut().set_connected(true);
                self.as_mut().set_device_info(QString::from(&info));
                self.as_mut().set_mode(i32::from(mode));
                self.as_mut().inner_mut().bank_value = i32::from(bank);
                let state = self.state_text();
                self.as_mut().set_status_text(state);
            }
            Event::Disconnected(reason) => {
                self.as_mut().set_connected(false);
                self.as_mut().set_battery(-1);
                self.as_mut()
                    .set_status_text(QString::from(&format!("No device: {reason}")));
                self.as_mut().set_device_info(QString::default());
            }
            Event::Status { charging, percent } => {
                // `-1` is the shell's "unknown" (no charge has been seen on
                // battery yet, and the device reports none while charging).
                self.as_mut().set_battery(percent.map_or(-1, i32::from));
                self.as_mut().set_charging(charging);
                // The shell composes the battery line; this is only the state.
                let state = self.state_text();
                self.as_mut().set_status_text(state);
            }
            Event::Block {
                block,
                generation,
                verified,
            } => {
                if generation < self.inner().generation {
                    // A newer edit is already in flight; this block is stale.
                    return;
                }
                if !verified {
                    self.as_mut()
                        .notified(QString::from("Read-back mismatch - reloading"));
                    self.send(Command::Reload);
                    return;
                }
                {
                    let rust = self.as_mut().inner_mut();
                    rust.block = SettingsBlock::new(block);
                    rust.block_loaded = true;
                }
                self.as_mut().block_changed();
            }
            Event::Busy { busy, label } => {
                if busy {
                    if !label.is_empty() {
                        self.as_mut().set_status_text(QString::from(&label));
                    }
                } else {
                    let state = self.state_text();
                    self.as_mut().set_status_text(state);
                }
            }
            Event::Note(message) => {
                self.as_mut().notified(QString::from(&message));
            }
            Event::Mode(mode) => {
                self.as_mut().set_mode(i32::from(mode));
            }
            Event::Error(message) => {
                self.as_mut().error_occurred(QString::from(&message));
            }
        }
    }
}
