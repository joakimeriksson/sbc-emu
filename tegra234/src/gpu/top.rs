//! GA10B TOP v2 discovery table and the topology of the modeled GPU.

use super::host;

const DEVICE_INFO_CONFIG: u64 = 0x0224fc;
const DEVICE_INFO_BASE: u64 = 0x022800;
const CHAIN: u32 = 1 << 31;
const ENGINE: u32 = 1 << 30;
const GRAPHICS: u32 = 0;
const LOGICAL_COPY: u32 = 0x13;
const GPCS: u32 = 1;
pub(super) const TPCS_PER_GPC: u32 = 2;
pub(super) const LTCS: u32 = 1;

// Use separate runlists so the scheduler can dispatch the copy engine
// independently of graphics.
const fn engine(kind: u32, fault: u32, pri: u32, reset: u32, runlist: u64, slot: u32) -> [u32; 3] {
    [
        CHAIN | kind << 24 | fault,
        CHAIN | ENGINE | pri | reset,
        runlist as u32 | slot,
    ]
}

const DEVICES: [[u32; 3]; 2] = [
    engine(GRAPHICS, 0, 0x400000, 12, host::GR_RUNLIST, 0),
    engine(LOGICAL_COPY, 1, 0x104000, 6, host::RUNLIST, 0),
];

pub(super) fn read(offset: u64) -> Option<u32> {
    Some(match offset {
        0x022430 | 0x120078 => GPCS,
        0x022434 => TPCS_PER_GPC,
        0x022438 | 0x120074 => 1, // FBP count
        0x022444 => 1,            // copy engines
        0x022450 | 0x022454 | 0x12006c => LTCS,
        0x02245c => 2,            // slices per LTC
        0x022460 | 0x022480 => 1, // PES and ROPs per GPC
        DEVICE_INFO_CONFIG => ((DEVICES.len() * 3) as u32) << 20 | 2,
        DEVICE_INFO_BASE..=0x022ffc => {
            let row = ((offset - DEVICE_INFO_BASE) / 4) as usize;
            DEVICES.get(row / 3).map_or(0, |device| device[row % 3])
        }
        _ => return None,
    })
}
