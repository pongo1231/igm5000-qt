//! Model of the device's 148-byte settings block (`PROTOCOL.md` §5, external).
//!
//! The block is always kept whole: the app only ever mutates the fields it
//! understands and writes the block back verbatim, so unknown fields survive
//! a read-modify-write.

/// `Cfg.ini` `[SENSOR_1] DPISET`: the sensor accepts these steps only; the
/// device stores the 1-based index (0 = level disabled).
pub const DPI_STEPS: [u16; 59] = [
    200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 1200, 1300, 1400, 1500, 1600, 1700, 1800,
    1900, 2000, 2100, 2200, 2300, 2400, 2500, 2600, 2700, 2800, 2900, 3000, 3100, 3200, 3300, 3400,
    3500, 3600, 3700, 3800, 3900, 4000, 4100, 4200, 4300, 4400, 4500, 4600, 4700, 4800, 4900, 5000,
    5500, 6000, 6400, 7000, 7500, 8000, 8500, 9000, 9500, 10000,
];

/// Polling rates in Hz; the stored code is the 1-based index into this table.
pub const RATE_CODES: [u16; 4] = [125, 250, 500, 1000];

/// DPI level slots in the settings block. The device cycles only through the
/// first [`SettingsBlock::enabled_levels`] of them.
pub const LEVELS: u8 = 8;

/// Per-level colours (`Cfg.ini` `DC`), used by the factory profile and by a
/// level that joins the profile.
pub const LEVEL_COLORS: [[u8; 3]; LEVELS as usize] = [
    [255, 0, 0],
    [0, 0, 255],
    [0, 255, 0],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 0],
    [255, 255, 255],
    [255, 70, 0],
];

/// Nearest `DPI_STEPS` entry, as a 1-based code.
pub fn dpi_to_code(dpi: u16) -> u16 {
    let mut best = 0usize;
    for (i, step) in DPI_STEPS.iter().enumerate() {
        if (i64::from(*step) - i64::from(dpi)).abs() < (i64::from(DPI_STEPS[best]) - i64::from(dpi)).abs()
        {
            best = i;
        }
    }
    best as u16 + 1
}

/// Real DPI for a stored code; 0 for "disabled" / out of range.
pub fn code_to_dpi(code: u16) -> u16 {
    if code == 0 || code as usize > DPI_STEPS.len() {
        0
    } else {
        DPI_STEPS[code as usize - 1]
    }
}

#[derive(Clone)]
pub struct SettingsBlock {
    raw: [u8; 148],
}

/// The vendor tool stamps this at `+0x90` before saving, and the device keeps
/// it: a written profile only reaches the live mouse once it is set.
pub const COMMIT_MARKER: u8 = 0xa5;
pub const COMMIT_OFFSET: usize = 0x90;

impl SettingsBlock {
    pub fn new(raw: [u8; 148]) -> Self {
        SettingsBlock { raw }
    }

    pub fn raw(&self) -> &[u8; 148] {
        &self.raw
    }

    /// Polling rate in Hz, 0 when the stored code is unknown.
    pub fn rate_hz(&self) -> u16 {
        let code = self.raw[2] & 0x0f;
        if code == 0 || code as usize > RATE_CODES.len() {
            0
        } else {
            RATE_CODES[code as usize - 1]
        }
    }

    /// Keeps the high nibble: bit 7 is the split-X/Y flag, which the app never
    /// touches (the Y DPI slots at `0x15` stay as the device reported them).
    pub fn set_rate_hz(&mut self, hz: u16) {
        if let Some(idx) = RATE_CODES.iter().position(|r| *r == hz) {
            self.raw[2] = (self.raw[2] & 0xf0) | (idx as u8 + 1);
        }
    }

    /// The active level as stored in the block: a **1-based** level number
    /// (`Cfg.ini`'s `DEFLEVEL=2` is the second level). The vendor writes it by
    /// counting the enabled levels up to the active index.
    pub fn current_level(&self) -> u8 {
        self.raw[3] >> 4
    }

    /// The active level as a 0-based index into the level arrays.
    pub fn active_index(&self) -> u8 {
        self.current_level().saturating_sub(1)
    }

    pub fn set_current_level(&mut self, level: u8) {
        self.raw[3] = ((level & 0x0f) << 4) | (self.raw[3] & 0x0f);
    }

    /// How many DPI levels the mouse cycles through (byte 3, low nibble).
    pub fn enabled_levels(&self) -> u8 {
        (self.raw[3] & 0x0f).clamp(1, LEVELS)
    }

    pub fn set_enabled_levels(&mut self, count: u8) {
        self.raw[3] = (self.raw[3] & 0xf0) | count.clamp(1, LEVELS);
    }

    /// Add `level` to the profile. Levels stay contiguous, so only the slot
    /// right after the last enabled one can be added.
    pub fn add_level(&mut self, level: u8) {
        let count = self.enabled_levels();
        if level != count || count >= LEVELS {
            return;
        }
        // A reachable slot must never hold DPI code 0: that is not "off", the
        // firmware crawls instead. The colour is left alone.
        if self.dpi_code(level) == 0 {
            self.set_dpi_code(level, 3); // 400 dpi
        }
        self.set_enabled_levels(count + 1);
    }

    /// Remove `level` from the profile by shifting the levels after it down one
    /// slot and shrinking the count - the device has no "disabled" slot state.
    pub fn remove_level(&mut self, level: u8) {
        let count = self.enabled_levels();
        if level >= count || count < 2 {
            return;
        }
        let active = self.current_level();
        for slot in level..count - 1 {
            let code = self.dpi_code(slot + 1);
            self.set_dpi_code(slot, code);
            let rgb = self.color(slot + 1);
            self.set_color(slot, rgb);
        }
        self.set_dpi_code(count - 1, 0);
        // The slot left the profile, so its colour goes back to the default:
        // otherwise the next level to join would inherit whatever was there.
        self.set_color(count - 1, LEVEL_COLORS[count as usize - 1]);
        self.set_enabled_levels(count - 1);
        // A removal below the active level shifts its settings down, so follow
        // them; the number has to stay inside the profile either way.
        let active = if level + 1 < active { active - 1 } else { active };
        self.set_current_level(active.min(self.enabled_levels()));
    }

    /// The IGM 5000 vendor defaults (`Cfg.ini`: `DR=0x500`, `MDNUM=6`,
    /// `DEFLEVEL=2`, the `DPISET` indexes and per-level `DC` colours, LOD 9,
    /// LED effect 1 / parameter 2 / red). Fields the app does not map - and the
    /// split-X/Y flag - are kept from `current`.
    pub fn factory(current: &SettingsBlock) -> SettingsBlock {
        const DPI_CODES: [u16; 8] = [3, 7, 15, 31, 52, 59, 0, 0];
        let mut block = current.clone();
        block.set_rate_hz(500);
        // `DEFLEVEL=2` in Cfg.ini: the second level (1-based).
        block.set_current_level(2);
        block.set_enabled_levels(6);
        for (level, code) in DPI_CODES.iter().enumerate() {
            block.set_dpi_code(level as u8, *code);
        }
        for (level, rgb) in LEVEL_COLORS.iter().enumerate() {
            block.set_color(level as u8, *rgb);
        }
        block.set_lod(9);
        block.set_led_effect(1);
        block.set_led_arg(2);
        block.set_led_color([255, 0, 0]);
        block
    }

    pub fn dpi_code(&self, level: u8) -> u16 {
        let off = 5 + 2 * level as usize;
        u16::from_le_bytes([self.raw[off], self.raw[off + 1]])
    }

    pub fn set_dpi_code(&mut self, level: u8, code: u16) {
        let off = 5 + 2 * level as usize;
        self.raw[off..off + 2].copy_from_slice(&code.to_le_bytes());
    }

    pub fn dpi_value(&self, level: u8) -> u16 {
        code_to_dpi(self.dpi_code(level))
    }

    pub fn color(&self, level: u8) -> [u8; 3] {
        let off = 0x25 + 3 * level as usize;
        [self.raw[off], self.raw[off + 1], self.raw[off + 2]]
    }

    pub fn set_color(&mut self, level: u8, rgb: [u8; 3]) {
        let off = 0x25 + 3 * level as usize;
        self.raw[off..off + 3].copy_from_slice(&rgb);
    }

    pub fn lod(&self) -> u8 {
        self.raw[0x3d]
    }

    pub fn set_lod(&mut self, v: u8) {
        self.raw[0x3d] = v;
    }

    // The LED effect index/parameter/colour at 0x8b-0x8f have no visible effect
    // on this unit, so the app never edits them - only the factory profile sets
    // the vendor's default values.

    pub fn set_led_effect(&mut self, v: u8) {
        self.raw[0x8b] = v;
    }

    pub fn set_led_arg(&mut self, v: u8) {
        self.raw[0x8c] = v;
    }

    pub fn set_led_color(&mut self, rgb: [u8; 3]) {
        self.raw[0x8d..0x90].copy_from_slice(&rgb);
    }

    /// Annotated hex view of the whole block, for the Advanced tab.
    pub fn hex_dump(&self) -> String {
        let mut out = String::with_capacity(10 * 60);
        for (row, chunk) in self.raw.chunks(16).enumerate() {
            let hex = chunk
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("{:04x}  {hex}\n", row * 16));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory_block() -> SettingsBlock {
        SettingsBlock::factory(&SettingsBlock::new([0; 148]))
    }

    #[test]
    fn removing_a_level_below_the_active_one_keeps_the_active_settings() {
        let mut block = factory_block();
        block.set_current_level(3);
        let active_before = block.dpi_value(block.active_index());
        assert_eq!(active_before, 1600);

        block.remove_level(1);

        assert_eq!(block.enabled_levels(), 5);
        assert_eq!(block.active_index(), 1);
        assert_eq!(block.dpi_value(block.active_index()), active_before);
    }

    #[test]
    fn removing_the_active_level_keeps_the_number_inside_the_profile() {
        let mut block = factory_block();
        block.set_current_level(6);

        block.remove_level(5);

        assert_eq!(block.enabled_levels(), 5);
        assert_eq!(block.current_level(), 5);
        assert_eq!(block.active_index(), 4);
    }

    #[test]
    fn a_level_that_left_the_profile_comes_back_at_its_defaults() {
        let mut block = factory_block();
        block.set_color(5, [1, 2, 3]);

        block.remove_level(5);

        // The slot is outside the profile now: it holds no DPI and is back to
        // its default colour, so the next level to join starts clean.
        assert_eq!(block.dpi_code(5), 0);
        assert_eq!(block.color(5), LEVEL_COLORS[5]);

        block.add_level(5);

        assert_eq!(block.enabled_levels(), 6);
        assert_eq!(block.dpi_value(5), 400);
        assert_eq!(block.color(5), LEVEL_COLORS[5]);
    }

    #[test]
    fn a_level_joining_the_profile_keeps_the_colour_picked_for_it() {
        let mut block = factory_block();
        block.remove_level(5);
        block.set_color(5, [1, 2, 3]);

        block.add_level(5);

        assert_eq!(block.enabled_levels(), 6);
        assert_eq!(block.dpi_value(5), 400);
        assert_eq!(block.color(5), [1, 2, 3]);
    }
}
