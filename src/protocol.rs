//! hidraw transport for the ISY IGM 5000 vendor control interface.
//!
//! Port of the verified reference implementation `igmtool.py` and its
//! `PROTOCOL.md` (both ship with the vendor tool, not in this repo). All device
//! I/O is HID feature reports: report id 5 is the 8-byte command channel,
//! report id 8 the 520-byte data channel.

use std::fmt;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub const VENDOR: u16 = 0x258a;
pub const KNOWN_PIDS: [u16; 2] = [0x002e, 0x002f];
pub const RID_CMD: u8 = 5;
pub const RID_DATA: u8 = 8;
pub const CMD_LEN: usize = 8;
pub const DATA_LEN: usize = 520;
pub const BLOCK_LEN: usize = 148;
/// The settings block starts at byte 8 of the data report.
pub const BLOCK_OFFSET: usize = 8;
/// Firmware `MsFw=0x005`: data terminator byte.
pub const DATA_TERM: u8 = 0x92;

pub const CMD_GET_PSD: u8 = 0x01;
/// Read mode; written as the same opcode with `c1 = mode` (1..3).
pub const CMD_GET_MODE: u8 = 0x02;
pub const CMD_SET_DEBOUNCE: u8 = 0x1a;
/// `d0` = 0x10 charging / 0x11 on battery, `d1` = charge percent while on
/// battery (`d2 d3` = u16).
pub const CMD_GET_STATUS: u8 = 0x90;

const RETRIES: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(200);
/// The device needs settling before its reply is readable.
const REPLY_DELAY: Duration = Duration::from_millis(30);
const WRITE_SETTLE: Duration = Duration::from_millis(50);

pub const fn bank_opcode(bank: u8) -> u8 {
    (bank << 4) | 0x01
}

#[derive(Debug)]
pub enum Error {
    NoDevice,
    PermissionDenied,
    /// Access could not be obtained through `pkexec` (missing, cancelled, or
    /// refused by polkit).
    Escalation(String),
    /// `EPIPE` from the ioctl: the wireless link NAK'd the transfer.
    IoStall,
    Timeout,
    Echo { expected: u8, got: u8 },
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevice => write!(f, "no device found"),
            Error::PermissionDenied => write!(f, "permission denied on hidraw node"),
            Error::IoStall => write!(f, "device stalled"),
            Error::Timeout => write!(f, "device did not answer"),
            Error::Echo { expected, got } => {
                write!(f, "bad reply echo: expected {expected:#04x}, got {got:#04x}")
            }
            Error::Escalation(reason) => write!(f, "could not get access: {reason}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl Error {
    /// Whether the handle can no longer talk to the device and should be
    /// dropped so the next command re-discovers it (a device that re-enumerated
    /// across a suspend, or was unplugged). A NAK'd transfer or a missed reply
    /// is transient and handled by the retry policy instead.
    pub fn is_fatal(&self) -> bool {
        match self {
            Error::NoDevice | Error::PermissionDenied | Error::Escalation(_) | Error::Io(_) => true,
            Error::IoStall | Error::Timeout | Error::Echo { .. } => false,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        match e.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => Error::PermissionDenied,
            Some(libc::EPIPE) => Error::IoStall,
            Some(libc::ETIMEDOUT) => Error::Timeout,
            Some(libc::ENODEV) | Some(libc::ENOENT) => Error::NoDevice,
            _ => Error::Io(e),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub state: u8,
    pub percent: u8,
}

impl Status {
    pub fn charging(&self) -> bool {
        self.state == 0x10
    }
}

// ---------------------------------------------------------------------------
// HID report descriptor parsing - only enough to find the vendor collections
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct ReportSizes {
    input: usize,
    output: usize,
    feature: usize,
}

fn parse_report_descriptor(desc: &[u8]) -> Vec<(u8, ReportSizes)> {
    let mut reports: Vec<(u8, ReportSizes)> = Vec::new();
    let (mut rid, mut size, mut count) = (0u8, 0usize, 0usize);
    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            // long item: 0xFE, data size, tag, data
            if i + 2 >= desc.len() {
                break;
            }
            i += 3 + desc[i + 1] as usize;
            continue;
        }
        let mut bsize = (prefix & 0x03) as usize;
        let btype = (prefix >> 2) & 0x03;
        let btag = prefix >> 4;
        if bsize == 3 {
            bsize = 4;
        }
        let mut val = 0u32;
        for k in 0..bsize {
            if i + 1 + k < desc.len() {
                val |= (desc[i + 1 + k] as u32) << (8 * k);
            }
        }
        i += 1 + bsize;
        match btype {
            1 => match btag {
                7 => size = val as usize,
                8 => rid = val as u8,
                9 => count = val as usize,
                _ => {}
            },
            0 => {
                if btag == 8 || btag == 9 || btag == 11 {
                    if size != 0 && count != 0 {
                        let bytes = (size * count).div_ceil(8);
                        let idx = match reports.iter().position(|(id, _)| *id == rid) {
                            Some(i) => i,
                            None => {
                                reports.push((rid, ReportSizes::default()));
                                reports.len() - 1
                            }
                        };
                        let slot = &mut reports[idx].1;
                        match btag {
                            8 => slot.input += bytes,
                            9 => slot.output += bytes,
                            _ => slot.feature += bytes,
                        }
                    }
                    size = 0;
                    count = 0;
                } else {
                    // collection / end collection / reserved
                    size = 0;
                    count = 0;
                }
            }
            _ => {}
        }
    }
    reports
}

fn node_index(path: &str) -> u32 {
    path.chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn hidraw_nodes() -> Vec<String> {
    let mut nodes: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name
                .strip_prefix("hidraw")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            {
                nodes.push(format!("/dev/{name}"));
            }
        }
    }
    nodes.sort_by_key(|p| node_index(p));
    nodes
}

/// Read a sysfs attribute of the hidraw node (`/sys/class/hidraw/<node>/device/<name>`).
fn sysfs_read(node: &str, name: &str) -> Option<String> {
    let base = std::path::Path::new(node).file_name()?;
    std::fs::read_to_string(format!(
        "/sys/class/hidraw/{}/device/{name}",
        base.to_string_lossy()
    ))
    .ok()
}

fn hid_ids(node: &str) -> Option<(u16, u16)> {
    let uevent = sysfs_read(node, "uevent")?;
    for line in uevent.lines() {
        if let Some(rest) = line.strip_prefix("HID_ID=") {
            let mut parts = rest.trim().split(':');
            let _bus = parts.next()?;
            let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
            let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
            return Some((vid, pid));
        }
    }
    None
}

fn read_descriptor(node: &str) -> Option<Vec<u8>> {
    let base = std::path::Path::new(node).file_name()?;
    let mut buf = Vec::new();
    std::fs::File::open(format!(
        "/sys/class/hidraw/{}/device/report_descriptor",
        base.to_string_lossy()
    ))
    .ok()?
    .read_to_end(&mut buf)
    .ok()?;
    Some(buf)
}

/// Vendor collections: feature report id 5 of 7 bytes and id 8 of 519 bytes.
fn is_control_interface(reports: &[(u8, ReportSizes)]) -> bool {
    let feature = |id: u8| {
        reports
            .iter()
            .find(|(rid, _)| *rid == id)
            .map(|(_, s)| s.feature)
            .unwrap_or(0)
    };
    feature(RID_CMD) >= 7 && feature(RID_DATA) >= 519
}

/// Walk `/dev/hidraw*` and return the node carrying the vendor control reports.
///
/// Matching only VID/PID is not enough: interface 0 of the same mouse has no
/// vendor collections, and other hidraw nodes belong to unrelated devices.
pub fn find_control_node() -> Result<(String, u16)> {
    let mut fallback = None;
    for node in hidraw_nodes() {
        let Some((vid, pid)) = hid_ids(&node) else {
            continue;
        };
        if vid != VENDOR {
            continue;
        }
        let Some(desc) = read_descriptor(&node) else {
            continue;
        };
        if !is_control_interface(&parse_report_descriptor(&desc)) {
            continue;
        }
        if KNOWN_PIDS.contains(&pid) {
            return Ok((node, pid));
        }
        fallback.get_or_insert((node, pid));
    }
    fallback.ok_or(Error::NoDevice)
}

// ---------------------------------------------------------------------------
// transport
// ---------------------------------------------------------------------------

const fn ioc(direction: u64, type_char: u8, nr: u8, size: u32) -> libc::c_ulong {
    (direction << 30)
        | ((size as u64) << 16)
        | ((type_char as u64) << 8)
        | (nr as u64)
}

const fn hidev_feature(nr: u8, len: usize) -> libc::c_ulong {
    // _IOC_READ|_IOC_WRITE, 'H', nr, len - matches the kernel's HIDIOC*FEATURE.
    ioc(3, b'H', nr, len as u32)
}

/// Whether a data report whose byte 1 is a *recognisably* different bank's opcode.
///
/// The reply layout is documented only by the vendor tool, so nothing stricter
/// can be checked; a byte that repeats is data rather than an opcode.
fn names_other_bank(report: &[u8], bank: u8) -> bool {
    (1..=3u8).any(|other| other != bank && report.get(1) == Some(&bank_opcode(other)))
}

/// Directories a root-executed helper may come from, in order. Never `PATH`: the
/// session `PATH` puts user-writable directories first, so a planted `chmod`
/// there would run as root. `/run/wrappers/bin` is first because the store's
/// `pkexec` is not setuid and refuses to run.
const SYSTEM_BIN_DIRS: [&str; 6] = [
    "/run/wrappers/bin",
    "/run/current-system/sw/bin", // the NixOS system profile
    "/nix/var/nix/profiles/default/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
];

fn system_program(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    SYSTEM_BIN_DIRS.iter().find_map(|dir| {
        let candidate = std::path::Path::new(dir).join(program);
        // `metadata` follows the system profile's symlinks: what it lands on is
        // a store path, which is root-owned and read-only.
        let metadata = candidate.metadata().ok()?;
        let trusted = metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0;
        trusted.then_some(candidate)
    })
}

/// The arguments handed to `chmod`. The name is repeated when the canonical path
/// is not `chmod` itself, because coreutils here is a multicall binary that then
/// reads the program name from `argv[1]`.
fn chmod_args(chmod: &std::path::Path, node: &str) -> Vec<String> {
    let canonical = std::fs::canonicalize(chmod).unwrap_or_else(|_| chmod.to_path_buf());
    let mut args = Vec::new();
    if canonical.file_name() != Some(std::ffi::OsStr::new("chmod")) {
        args.push("chmod".to_string());
    }
    args.push("a+rw".to_string());
    args.push("--".to_string());
    args.push(node.to_string());
    args
}

/// pkexec's own message: third-party text, so it is cut to one line and
/// stripped of control characters before it can reach the UI.
fn short_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let cleaned: String = text
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    if cleaned.is_empty() {
        "no reason given".to_string()
    } else {
        cleaned
    }
}

fn run_privileged(
    pkexec: &std::path::Path,
    helper: &std::path::Path,
    args: &[String],
) -> Result<(), Error> {
    let output = std::process::Command::new(pkexec)
        .arg(helper)
        .args(args)
        .output()
        .map_err(|e| Error::Escalation(format!("could not run pkexec: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Escalation(match output.status.code() {
        // pkexec's own codes: 126 the prompt was dismissed, 127 not authorised.
        Some(126) => "the polkit prompt was dismissed".to_string(),
        Some(127) => format!("polkit refused the request ({})", short_detail(&output.stderr)),
        Some(code) => format!(
            "the helper exited with {code}: {}",
            short_detail(&output.stderr)
        ),
        None => format!("the helper was killed: {}", short_detail(&output.stderr)),
    }))
}

const ESCALATION_TIMEOUT: Duration = Duration::from_secs(120);

/// Give `node` read and write access through polkit. The grant lasts until the
/// node is re-created by a replug; `99-isymouse.rules` is the permanent fix.
pub fn grant_access(node: &str) -> Result<(), Error> {
    let chmod = system_program("chmod")
        .ok_or_else(|| Error::Escalation("chmod was not found in the system paths".into()))?;
    let pkexec = system_program("pkexec")
        .ok_or_else(|| Error::Escalation("pkexec was not found (is polkit installed?)".into()))?;
    let args = chmod_args(&chmod, node);
    // The prompt is human-gated, so it runs off the device thread: a dialog
    // nobody answers must not freeze device I/O or hold up the exit.
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("igm5000-pkexec".into())
        .spawn(move || {
            let _ = done.send(run_privileged(&pkexec, &chmod, &args));
        })
        .map_err(|e| Error::Escalation(format!("could not start the helper thread: {e}")))?;
    match outcome.recv_timeout(ESCALATION_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(Error::Escalation(
            "no answer from polkit; the device stays read-only".into(),
        )),
    }
}

pub struct Transport {
    fd: OwnedFd,
}

impl Transport {
    pub fn open_path(path: &str) -> Result<Transport> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        use std::os::unix::io::IntoRawFd;
        let fd = unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) };
        Ok(Transport { fd })
    }

    fn ioctl(&self, req: libc::c_ulong, buf: &mut [u8]) -> Result<()> {
        let ret = unsafe { libc::ioctl(self.fd.as_raw_fd(), req, buf.as_mut_ptr()) };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    fn set_feature(&self, buf: &mut [u8]) -> Result<()> {
        self.ioctl(hidev_feature(0x06, buf.len()), buf)
    }

    fn get_feature(&self, rid: u8, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        buf[0] = rid;
        self.ioctl(hidev_feature(0x07, len), &mut buf)?;
        Ok(buf)
    }

    /// Send a 5-byte command and read back its reply; returns `d0..d3`.
    pub fn command(&mut self, cmd: [u8; 5]) -> Result<[u8; 4]> {
        let raw = self.command_raw(cmd, true)?;
        Ok([raw[2], raw[3], raw[4], raw[5]])
    }

    pub fn command_no_reply(&mut self, cmd: [u8; 5]) -> Result<()> {
        self.command_raw(cmd, false).map(|_| ())
    }

    fn command_raw(&mut self, cmd: [u8; 5], expect_reply: bool) -> Result<[u8; CMD_LEN]> {
        let mut frame = [0u8; CMD_LEN];
        frame[0] = RID_CMD;
        frame[1..6].copy_from_slice(&cmd);
        let mut last: Option<Error> = None;
        for _ in 0..RETRIES {
            match self.set_feature(&mut frame) {
                Ok(()) => {}
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
            }
            if !expect_reply {
                return Ok(frame);
            }
            std::thread::sleep(REPLY_DELAY);
            for _ in 0..RETRIES {
                match self.get_feature(RID_CMD, CMD_LEN) {
                    Ok(raw) => {
                        if raw[1] == cmd[0] {
                            let mut out = [0u8; CMD_LEN];
                            out.copy_from_slice(&raw[..CMD_LEN]);
                            return Ok(out);
                        }
                        last = Some(Error::Echo {
                            expected: cmd[0],
                            got: raw[1],
                        });
                        break;
                    }
                    Err(e) => {
                        last = Some(e);
                        std::thread::sleep(RETRY_DELAY);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(last.unwrap_or(Error::Timeout))
    }

    pub fn status(&mut self) -> Result<Status> {
        let d = self.command([CMD_GET_STATUS, 0, 0, 0, 0])?;
        Ok(Status {
            state: d[0],
            percent: d[1],
        })
    }

    pub fn psd(&mut self) -> Result<[u8; 4]> {
        self.command([CMD_GET_PSD, 0, 0, 0, 0])
    }

    pub fn mode(&mut self) -> Result<u8> {
        Ok(self.command([CMD_GET_MODE, 0, 0, 0, 0])?[0])
    }

    pub fn set_mode(&mut self, mode: u8) -> Result<()> {
        self.command_no_reply([CMD_GET_MODE, mode, 0, 0, 0])
    }

    pub fn set_debounce(&mut self, value: u8) -> Result<()> {
        self.command_no_reply([CMD_SET_DEBOUNCE, value, 0, 0, 0])
    }

    fn read_data_report(&mut self) -> Result<[u8; DATA_LEN]> {
        let mut last = Error::Timeout;
        for _ in 0..RETRIES {
            match self.get_feature(RID_DATA, DATA_LEN) {
                Ok(raw) => {
                    let mut out = [0u8; DATA_LEN];
                    out.copy_from_slice(&raw[..DATA_LEN]);
                    return Ok(out);
                }
                Err(e) => {
                    last = e;
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
        Err(last)
    }

    /// Read the settings block of a bank (1..3). A bank read has no
    /// control-channel reply; the data channel answers instead.
    pub fn read_block(&mut self, bank: u8) -> Result<[u8; BLOCK_LEN]> {
        let raw = self.read_report(bank)?;
        let mut block = [0u8; BLOCK_LEN];
        block.copy_from_slice(&raw[BLOCK_OFFSET..BLOCK_OFFSET + BLOCK_LEN]);
        Ok(block)
    }

    /// Read the whole data report of a bank, re-issuing once if the reply names
    /// another bank (see `names_other_bank`).
    pub fn read_report(&mut self, bank: u8) -> Result<[u8; DATA_LEN]> {
        let mut last = Error::Timeout;
        let mut re_issued = false;
        for _ in 0..RETRIES {
            self.command_no_reply([bank_opcode(bank), 0, 0, 0, 0])?;
            std::thread::sleep(REPLY_DELAY);
            match self.read_data_report() {
                Ok(raw) if re_issued || !names_other_bank(&raw, bank) => return Ok(raw),
                Ok(raw) => {
                    last = Error::Echo {
                        expected: bank_opcode(bank),
                        got: raw[1],
                    };
                    re_issued = true;
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(e) => {
                    last = e;
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
        Err(last)
    }

    /// Write a settings block. Byte 3 (active/enabled level nibbles) is never
    /// touched here - the caller passes a block read back from the device.
    pub fn write_block(&mut self, bank: u8, block: &[u8; BLOCK_LEN]) -> Result<()> {
        let mut frame = vec![0u8; DATA_LEN];
        frame[0] = RID_DATA;
        frame[1] = bank_opcode(bank);
        frame[3] = DATA_TERM;
        frame[BLOCK_OFFSET..BLOCK_OFFSET + BLOCK_LEN].copy_from_slice(block);
        let mut last = Error::Timeout;
        for _ in 0..RETRIES {
            match self.set_feature(&mut frame) {
                Ok(()) => {
                    std::thread::sleep(WRITE_SETTLE);
                    return Ok(());
                }
                Err(e) => {
                    last = e;
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_requests_match_the_kernel_encoding() {
        assert_eq!(hidev_feature(0x06, CMD_LEN), 0xc008_4806);
        assert_eq!(hidev_feature(0x07, CMD_LEN), 0xc008_4807);
        assert_eq!(hidev_feature(0x07, DATA_LEN), 0xc208_4807);
    }

    #[test]
    fn the_control_interface_is_the_one_with_the_vendor_feature_reports() {
        let control: &[u8] = &[
            0x06, 0x00, 0xff, // Usage Page (vendor defined)
            0x09, 0x01, // Usage
            0xa1, 0x01, // Collection (Application)
            0x85, 0x05, // Report ID 5
            0x75, 0x38, // Report Size 56 bits
            0x95, 0x01, // Report Count 1
            0xb1, 0x02, // Feature
            0x85, 0x08, // Report ID 8
            0x75, 0x08, // Report Size 8 bits
            0x96, 0x07, 0x02, // Report Count 519 (two-byte value)
            0xb1, 0x02, // Feature
            0xc0, // End Collection
        ];
        assert!(is_control_interface(&parse_report_descriptor(control)));

        let plain: &[u8] = &[
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x75, 0x08, 0x95, 0x08, 0x81, 0x02, 0xc0,
        ];
        assert!(!is_control_interface(&parse_report_descriptor(plain)));
    }

    #[test]
    fn a_data_report_from_another_bank_is_not_accepted() {
        let mut report = [0u8; DATA_LEN];
        report[1] = bank_opcode(2);

        assert!(names_other_bank(&report, 1));
        assert!(!names_other_bank(&report, 2));

        // Not an opcode: a device that leaves the byte zero, or uses it for
        // data, still has to be read.
        report[1] = 0;
        assert!(!names_other_bank(&report, 1));
    }

    #[test]
    fn the_privileged_helper_never_comes_from_path() {
        assert!(SYSTEM_BIN_DIRS.iter().all(|dir| dir.starts_with('/')));
        assert!(system_program("igm5000-no-such-helper").is_none());
    }

    #[test]
    fn the_chmod_call_survives_a_multicall_helper() {
        assert_eq!(
            chmod_args(std::path::Path::new("/usr/bin/chmod"), "/dev/hidraw6"),
            ["a+rw", "--", "/dev/hidraw6"]
        );

        // A multicall fixture: `chmod` symlinked to another name.
        let dir = std::env::temp_dir().join("igm5000-multicall-test");
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("coreutils");
        std::fs::write(&binary, []).unwrap();
        let link = dir.join("chmod");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&binary, &link).unwrap();

        assert_eq!(
            chmod_args(&link, "/dev/hidraw6"),
            ["chmod", "a+rw", "--", "/dev/hidraw6"]
        );
    }
}
