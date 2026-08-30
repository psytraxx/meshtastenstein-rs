//! Battery voltage-to-percentage conversion, shared by every board's
//! battery task. Pure maths over `constants::OCV_TABLE`, no hardware I/O —
//! each board is responsible for its own sampling and filtering before
//! calling this.

use crate::constants::OCV_TABLE;

/// Convert a battery terminal voltage (millivolts) to a percentage, by
/// piecewise-linear interpolation over the open-circuit-voltage table.
pub fn voltage_to_level(mvolts: u16) -> u8 {
    if mvolts >= OCV_TABLE[0] {
        return 100;
    }
    if mvolts <= OCV_TABLE[10] {
        return 0;
    }
    for i in 0..10 {
        if mvolts >= OCV_TABLE[i + 1] {
            let v_high = OCV_TABLE[i] as u32;
            let v_low = OCV_TABLE[i + 1] as u32;
            let v = mvolts as u32;
            let pct_high = (100 - i * 10) as u32;
            let pct_low = (100 - (i + 1) * 10) as u32;
            return (pct_low + (v - v_low) * (pct_high - pct_low) / (v_high - v_low)) as u8;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_charge_and_above_reads_100_percent() {
        assert_eq!(voltage_to_level(OCV_TABLE[0]), 100);
        assert_eq!(voltage_to_level(u16::MAX), 100);
    }

    #[test]
    fn empty_and_below_reads_0_percent() {
        assert_eq!(voltage_to_level(OCV_TABLE[10]), 0);
        assert_eq!(voltage_to_level(0), 0);
    }

    #[test]
    fn table_breakpoints_land_on_exact_10_percent_steps() {
        for (i, &mv) in OCV_TABLE.iter().enumerate() {
            let expected = 100 - i as u8 * 10;
            assert_eq!(voltage_to_level(mv), expected, "breakpoint {i} ({mv} mV)");
        }
    }

    #[test]
    fn interpolates_linearly_between_breakpoints() {
        // Halfway between OCV_TABLE[1]=4050 and OCV_TABLE[2]=3900 (150mV
        // span, 10 percentage points) should land near the midpoint.
        let mid = (OCV_TABLE[1] + OCV_TABLE[2]) / 2;
        let level = voltage_to_level(mid);
        assert!((80..=90).contains(&level), "got {level}% at {mid} mV");
    }
}
