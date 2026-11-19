//! The firmware file Linux's `powervr` driver loads (`powervr/rogue_36.50.54.182_v1.fw`).
//!
//! Imagination publishes open-driver firmware for three Rogue cores, but not for the
//! JH7110's BXE-4-32 (36.50.54.182). This module makes a stand-in in the format the driver
//! parses (`pvr_fw_info.h`): an ELF header with no segments (no firmware code: the emulator
//! plays the firmware, `crate::fw`), the device-info block that tells the driver the core's
//! errata, enhancements and features, the info header and the MIPS layout table.
//!
//! The device info follows Imagination's DDK feature table for 36.50.54.182 (identical
//! masks to 36.52.104.182, whose linux-firmware image has the same MIPS layout) and Mesa's
//! `bxe-4-32.h` for the values that depend on the configuration.

pub const BVNC: (u16, u16, u16, u16) = (36, 50, 54, 182);
/// `PVR_PACKED_BVNC`: B in bits 63:48, V 47:32, N 31:16, C 15:0.
pub const PACKED_BVNC: u64 = (BVNC.0 as u64) << 48 | (BVNC.1 as u64) << 32 | (BVNC.2 as u64) << 16 | BVNC.3 as u64;
pub const FILE_NAME: &str = "powervr/rogue_36.50.54.182_v1.fw";

/// Errata (BRN bit indices of `pvr_rogue_fwif_dev_info.h`): 63553 (9), 71242 (11).
pub const BRN_MASK: u64 = 1 << 9 | 1 << 11;
/// Enhancements: ERN 35421, 38748, 42290, 42606, 47025, 57596.
pub const ERN_MASK: u64 = 0xf5;
/// Features: the DDK's set for this core, MIPS firmware processor (bit 27).
pub const FEATURE_MASK: [u64; 2] = [0x92d9_ff57_fdbf_7cdb, 0x1f];
/// The values of the value-carrying features that are set, in increasing bit order.
pub const FEATURE_VALUES: [(u32, u64); 29] = [
    (1, 1),      // CDM_CONTROL_STREAM_FORMAT
    (3, 16384),  // COMMON_STORE_SIZE_IN_DWORDS (Mesa: 1024 * 4 * 4)
    (10, 50),    // FBCDC
    (11, 50),    // FBCDC_ALGORITHM
    (12, 7),     // FBCDC_ARCHITECTURE
    (13, 0),     // FBC_MAX_DEFAULT_DESCRIPTORS
    (14, 0),     // FBC_MAX_LARGE_DESCRIPTORS
    (20, 4),     // ISP_MAX_TILES_IN_FLIGHT
    (21, 1),     // ISP_SAMPLES_PER_PIXEL
    (23, 1),     // LAYOUT_MARS
    (24, 10),    // MAX_PARTITIONS
    (26, 0),     // META_COREMEM_SIZE
    (28, 1),     // NUM_CLUSTERS
    (29, 4),     // NUM_ISP_IPP_PIPES
    (30, 8),     // NUM_OSIDS
    (31, 1),     // NUM_RASTER_PIPES
    (36, 36),    // PHYS_BUS_WIDTH
    (42, 2),     // SIMPLE_PARAMETER_FORMAT_VERSION
    (43, 1),     // SLC_BANKS
    (44, 512),   // SLC_CACHE_LINE_SIZE_BITS
    (46, 16),    // SLC_SIZE_IN_KILOBYTES
    (51, 16),    // TILE_SIZE_X
    (52, 16),    // TILE_SIZE_Y
    (57, 2),     // USC_MIN_OUTPUT_REGISTERS_PER_PIX
    (60, 40),    // VIRTUAL_ADDRESS_SPACE_BITS
    (63, 1),     // XE_ARCHITECTURE
    (66, 19),    // XPU_MAX_REGBANKS_ADDR_WIDTH
    (67, 3),     // XPU_MAX_SLAVES
    (68, 1),     // XPU_REGISTER_BROADCAST
];

/// The MIPS firmware's address map (`struct pvr_fw_layout_entry`: id, type, base_addr,
/// max_size, alloc_size, alloc_offset), the base addresses of Imagination's real MIPS
/// images with the sizes cut to one page each (there is no code to hold).
const LAYOUT: [[u32; 6]; 6] = [
    [4, 1, 0xc000_0000, 0x2f000, 0x1000, 0x0000], // MIPS_CODE
    [5, 1, 0x9fc0_2000, 0x2000, 0x1000, 0x1000],  // MIPS_EXCEPTIONS_CODE
    [6, 1, 0xbfc0_0000, 0x1000, 0x1000, 0x2000],  // MIPS_BOOT_CODE
    [7, 2, 0xc003_2000, 0x8000, 0x1000, 0x0000],  // MIPS_PRIVATE_DATA
    [8, 2, 0xbfc0_1000, 0x1000, 0x1000, 0x1000],  // MIPS_BOOT_DATA
    [9, 2, 0xcf60_0000, 0x1000, 0x1000, 0x2000],  // MIPS_STACK
];

/// The firmware file: 12 KiB.
pub fn stand_in_image() -> Vec<u8> {
    let mut f = vec![0u8; 0x3000];
    // an ELF32 MIPS executable header without program headers: nothing to load
    f[..16].copy_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    f[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    f[18..20].copy_from_slice(&8u16.to_le_bytes()); // EM_MIPS
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[28..32].copy_from_slice(&52u32.to_le_bytes()); // e_phoff
    f[40..42].copy_from_slice(&52u16.to_le_bytes()); // e_ehsize
    f[42..44].copy_from_slice(&32u16.to_le_bytes()); // e_phentsize
    // device info at size - 4096 - device_info_size
    let mut info: Vec<u64> = vec![1, 1, FEATURE_MASK.len() as u64, FEATURE_VALUES.len() as u64, BRN_MASK, ERN_MASK];
    info.extend_from_slice(&FEATURE_MASK);
    info.extend(FEATURE_VALUES.iter().map(|&(_, v)| v));
    for (i, w) in info.iter().enumerate() {
        f[0x1000 + 8 * i..0x1008 + 8 * i].copy_from_slice(&w.to_le_bytes());
    }
    // struct pvr_fw_info_header at size - 4096
    let h = 0x2000;
    let put32 = |f: &mut Vec<u8>, o: usize, v: u32| f[o..o + 4].copy_from_slice(&v.to_le_bytes());
    put32(&mut f, h, 3); // info_version
    put32(&mut f, h + 4, 48); // header_len
    put32(&mut f, h + 8, LAYOUT.len() as u32);
    put32(&mut f, h + 12, 24); // layout_entry_size
    f[h + 16..h + 24].copy_from_slice(&PACKED_BVNC.to_le_bytes());
    put32(&mut f, h + 24, 4096); // fw_page_size
    put32(&mut f, h + 28, 1); // PVR_FW_FLAGS_OPEN_SOURCE
    f[h + 32..h + 34].copy_from_slice(&1u16.to_le_bytes()); // fw_version_major
    f[h + 34..h + 36].copy_from_slice(&0u16.to_le_bytes()); // fw_version_minor
    put32(&mut f, h + 36, 0); // fw_version_build 0: the stand-in
    put32(&mut f, h + 40, 4096); // device_info_size
    for (i, e) in LAYOUT.iter().enumerate() {
        for (j, &v) in e.iter().enumerate() {
            put32(&mut f, h + 48 + 24 * i + 4 * j, v);
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feature bits whose mapping carries a value (`FEATURE_MAPPING_VALUE`).
    const VALUE_FEATURES: [u32; 31] = [1, 3, 9, 10, 11, 12, 13, 14, 20, 21, 23, 24, 25, 26, 28, 29, 30, 31, 36, 42, 43, 44, 46, 51, 52, 57, 60, 63, 66, 67, 68];

    fn has(bit: u32) -> bool {
        FEATURE_MASK[(bit / 64) as usize] >> (bit % 64) & 1 != 0
    }

    #[test]
    fn feature_values_follow_the_mask() {
        // the driver takes one value per set value-feature, in bit order
        let set: Vec<u32> = VALUE_FEATURES.iter().copied().filter(|&b| has(b)).collect();
        let given: Vec<u32> = FEATURE_VALUES.iter().map(|&(b, _)| b).collect();
        assert_eq!(set, given);
        assert!(has(27), "MIPS firmware processor");
        assert!(!has(25) && !has(37), "not META, not RISC-V");
    }

    #[test]
    fn header_and_layout() {
        let f = stand_in_image();
        assert_eq!(f.len() % 4096, 0);
        let h = f.len() - 4096;
        assert_eq!(u32::from_le_bytes(f[h..h + 4].try_into().unwrap()), 3);
        assert_eq!(u64::from_le_bytes(f[h + 16..h + 24].try_into().unwrap()), 0x0024_0032_0036_00b6);
        let dev_info = h - 4096;
        assert_eq!(u64::from_le_bytes(f[dev_info + 24..dev_info + 32].try_into().unwrap()), 29);
    }
}
