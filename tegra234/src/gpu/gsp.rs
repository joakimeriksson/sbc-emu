//! Falcon memory ports and scheduler startup protocol of GA10B.

const BASE: u64 = 0x110000;
const HWCFG: u64 = BASE + 0x108;
const HWCFG2: u64 = BASE + 0x12c;
const CPU_CONTROL: u64 = BASE + 0x100;
const FALCON_IRQ_CLEAR: u64 = BASE + 0x004;
const IRQ_STATUS: u64 = BASE + 0x008;
const IRQ_SET: u64 = BASE + 0x1520;
const IRQ_CLEAR: u64 = BASE + 0x1524;
const IRQ_MASK: u64 = BASE + 0x1528;
const IRQ_DEST: u64 = BASE + 0x152c;
const MESSAGE_INTERRUPT: u32 = 1 << 6;
const BROM_STATUS: u64 = 0x11165c;
const BROM_CONFIG: u64 = 0x111668;
const ENGINE_RESET: u64 = BASE + 0x3c0;
const MAILBOX0: u64 = BASE + 0x040;
const MAILBOX1: u64 = BASE + 0x044;
/// The RISC-V core's control register (falcon2 base 0x111000): STARTCPU (bit 0), HALTED (bit 4).
/// With the GSP fused to its RISC-V core, nvgpu starts it here instead of the Falcon CPUCTL.
const RISCV_CPUCTL: u64 = 0x111388;
const RISCV_STARTCPU: u32 = 1;
const RISCV_HALTED: u32 = 1 << 4;
const IMEM_CONTROL: u64 = BASE + 0x180;
const IMEM_DATA: u64 = BASE + 0x184;
const IMEM_TAG: u64 = BASE + 0x188;
const DMEM_CONTROL: u64 = BASE + 0x1c0;
const DMEM_DATA: u64 = BASE + 0x1c4;
const EMEM_SIZE: u64 = BASE + 0xabc;
const EMEM_CONTROL: u64 = BASE + 0xac0;
const EMEM_DATA: u64 = BASE + 0xac4;
const COMMAND_HEAD: u64 = BASE + 0xc00;
const COMMAND_TAIL: u64 = BASE + 0xc04;
const MESSAGE_HEAD: u64 = BASE + 0xc80;
const MESSAGE_TAIL: u64 = BASE + 0xc84;
const EMEM_BASE: u32 = 0x8000;
const QUEUE_BYTES: u32 = 0x800;
const EMEM_BYTES: usize = (2 * QUEUE_BYTES) as usize;
const MESSAGE_BASE: u32 = EMEM_BASE + QUEUE_BYTES;
const HEADER_BYTES: u32 = 4;
const DEVICES_INFO: u8 = 3;
const DOMAIN_ADD: u8 = 5;
const DOMAIN_MESSAGE_BYTES: u32 = HEADER_BYTES + 128;
const DEVICE_WORDS: usize = 7;
const DEVICE_MESSAGE_BYTES: u32 = HEADER_BYTES + 4 * DEVICE_WORDS as u32;
const BIND_CTX_REG: u8 = 0x0e;
const BLOCK: usize = 256;
// Provisioned capacities for the modeled Falcon. HWCFG reports counts of 256-byte blocks.
const IMEM_BYTES: usize = 64 * 1024;
const DMEM_BYTES: usize = 32 * 1024;
const WRITE_INCREMENT: u32 = 1 << 24;
const READ_INCREMENT: u32 = 1 << 25;
const ADDRESS_MASK: u32 = 0x0001_fffc;
const START_CPU: u32 = 1 << 1;
const BROM_FAILED: u32 = 2;
const BROM_PASSED: u32 = 3;

struct MemoryPort {
    control: u32,
    words: Box<[u32]>,
    first_word_loaded: bool,
    last_word_loaded: bool,
}

impl MemoryPort {
    fn new(bytes: usize) -> Self {
        Self {
            control: 0,
            words: vec![0; bytes / 4].into_boxed_slice(),
            first_word_loaded: false,
            last_word_loaded: false,
        }
    }

    fn word_index(&self) -> usize {
        ((self.control & ADDRESS_MASK) / 4) as usize
    }

    fn advance(&mut self, flag: u32) {
        if self.control & flag != 0 {
            let next = (self.control + 4) & ADDRESS_MASK;
            self.control = (self.control & !ADDRESS_MASK) | next;
        }
    }

    fn read(&mut self) -> u32 {
        let value = self.words.get(self.word_index()).copied().unwrap_or(0);
        self.advance(READ_INCREMENT);
        value
    }

    fn write(&mut self, value: u32) {
        let index = self.word_index();
        self.first_word_loaded |= index == 0;
        self.last_word_loaded |= index == self.words.len() - 1;
        if let Some(word) = self.words.get_mut(index) {
            *word = value;
        }
        self.advance(WRITE_INCREMENT);
    }
}

pub(super) struct Gsp {
    imem: MemoryPort,
    dmem: MemoryPort,
    emem: MemoryPort,
    command_head: u32,
    command_tail: u32,
    message_head: u32,
    message_tail: u32,
    context_bound: bool,
    devices: Vec<u32>,
    domains: Vec<u32>,
    irq_status: u32,
    irq_mask: u32,
    irq_dest: u32,
    imem_tags: [u32; IMEM_BYTES / BLOCK],
    brom_config: u32,
    brom_status: u32,
    mailbox: [u32; 2],
    halted: bool,
    /// The ACR descriptor's address (mailbox 0/1 at start) of an ACR run the GPU still has to do.
    acr_request: Option<u64>,
}

impl Default for Gsp {
    fn default() -> Self {
        Self {
            imem: MemoryPort::new(IMEM_BYTES),
            dmem: MemoryPort::new(DMEM_BYTES),
            emem: MemoryPort::new(EMEM_BYTES),
            command_head: 0,
            command_tail: 0,
            message_head: 0,
            message_tail: 0,
            context_bound: false,
            devices: Vec::new(),
            domains: Vec::new(),
            irq_status: 0,
            irq_mask: 0,
            irq_dest: 0,
            imem_tags: [0; IMEM_BYTES / BLOCK],
            brom_config: 0,
            brom_status: 0,
            mailbox: [0; 2],
            halted: false,
            acr_request: None,
        }
    }
}

impl Gsp {
    pub fn firmware_data_port(offset: u64) -> bool {
        matches!(offset, IMEM_DATA | DMEM_DATA)
    }

    fn publish_init_message(&mut self) {
        const INIT_UNIT: u8 = 2;
        const MESSAGE_BYTES: u8 = 24;
        let size = (QUEUE_BYTES as u16).to_le_bytes();
        let index = QUEUE_BYTES as usize / 4;
        self.emem.words[index..index + 6].copy_from_slice(&[
            u32::from_le_bytes([INIT_UNIT, MESSAGE_BYTES, 0, 0]),
            0, // Init message type.
            EMEM_BASE,
            u32::from_le_bytes([size[0], size[1], 0, 0]), // Command queue.
            MESSAGE_BASE,
            u32::from_le_bytes([size[0], size[1], 0, 1]), // Message queue.
        ]);
        self.command_head = EMEM_BASE;
        self.command_tail = EMEM_BASE;
        self.message_head = MESSAGE_BASE + u32::from(MESSAGE_BYTES);
        self.message_tail = MESSAGE_BASE;
        self.irq_mask = MESSAGE_INTERRUPT;
        self.irq_dest = MESSAGE_INTERRUPT;
        self.irq_status |= MESSAGE_INTERRUPT;
    }

    /// The boot ROM checks what the driver staged (text, data and the manifest) and starts the
    /// image. The firmware is not executed: a nonzero mailbox (the ACR descriptor's address,
    /// `nvgpu_riscv_hs_ucode_load_bootstrap`) selects the ACR, which the GPU runs at the level of
    /// its result (`take_acr_request`); otherwise it is the scheduler, which posts its init message.
    fn start(&mut self) {
        let staged = self.brom_config != 0
            && self.imem.first_word_loaded
            && self.dmem.first_word_loaded
            && self.dmem.last_word_loaded;
        self.brom_status = if staged { BROM_PASSED } else { BROM_FAILED };
        self.halted = false;
        if !staged {
            return;
        }
        let desc = u64::from(self.mailbox[0]) | u64::from(self.mailbox[1]) << 32;
        if desc != 0 {
            self.acr_request = Some(desc);
        } else {
            self.publish_init_message();
        }
    }

    pub fn take_acr_request(&mut self) -> Option<u64> {
        self.acr_request.take()
    }

    /// The ACR's exit: it halts with its status in mailbox 0 (0: success) and the failing
    /// falcon's error in mailbox 1.
    pub fn finish_acr(&mut self, status: u32, error: u32) {
        self.mailbox = [status, error];
        self.halted = true;
    }

    fn process_commands(&mut self) {
        if self.brom_status != BROM_PASSED {
            return;
        }
        // These boot commands fit in a single pass through each queue. Queue
        // rewind and runtime scheduling commands are handled by later work.
        while self.command_tail < self.command_head {
            let index = ((self.command_tail - EMEM_BASE) / 4) as usize;
            let command = self.emem.words[index];
            let [unit, size, _, sequence] = command.to_le_bytes();
            let next = self.command_tail + u32::from(size).next_multiple_of(4);
            if u32::from(size) < HEADER_BYTES || next > self.command_head {
                return;
            }
            match (unit, u32::from(size)) {
                (BIND_CTX_REG, HEADER_BYTES) if self.reply(unit, sequence) => {
                    self.context_bound = true;
                }
                (DEVICES_INFO, DEVICE_MESSAGE_BYTES) if self.context_bound => {
                    let runlist = self.emem.words[index + 2];
                    if !self.reply(unit, sequence) {
                        return;
                    }
                    if !self.devices.contains(&runlist) {
                        self.devices.push(runlist);
                    }
                }
                (DOMAIN_ADD, DOMAIN_MESSAGE_BYTES) if !self.devices.is_empty() => {
                    let domain = self.emem.words[index + 1];
                    if !self.reply(unit, sequence) {
                        return;
                    }
                    if !self.domains.contains(&domain) {
                        self.domains.push(domain);
                    }
                }
                _ => return,
            }
            self.command_tail = next;
        }
    }

    fn reply(&mut self, unit: u8, sequence: u8) -> bool {
        let next = self.message_head + HEADER_BYTES;
        if next >= MESSAGE_BASE + QUEUE_BYTES {
            return false;
        }
        let index = ((self.message_head - EMEM_BASE) / 4) as usize;
        self.emem.words[index] = u32::from_le_bytes([unit, HEADER_BYTES as u8, 0, sequence]);
        self.message_head = next;
        self.irq_status |= MESSAGE_INTERRUPT;
        true
    }

    pub fn irq(&self) -> bool {
        self.irq_status & self.irq_mask & self.irq_dest != 0
    }

    pub fn read(&mut self, offset: u64) -> Option<u32> {
        Some(match offset {
            HWCFG => (IMEM_BYTES / BLOCK) as u32 | ((DMEM_BYTES / BLOCK) as u32) << 9,
            // EMEM starts at 2^(8 + 7). The Falcon has one IMEM and one DMEM port.
            HWCFG2 => (7 << 21) | (1 << 12) | (1 << 8),
            IRQ_STATUS => self.irq_status,
            IRQ_MASK => self.irq_mask,
            IRQ_DEST => self.irq_dest,
            EMEM_SIZE => EMEM_BYTES as u32 / BLOCK as u32,
            EMEM_CONTROL => self.emem.control,
            EMEM_DATA => self.emem.read(),
            COMMAND_HEAD => self.command_head,
            COMMAND_TAIL => self.command_tail,
            MESSAGE_HEAD => self.message_head,
            MESSAGE_TAIL => self.message_tail,
            BROM_STATUS => self.brom_status,
            MAILBOX0 => self.mailbox[0],
            MAILBOX1 => self.mailbox[1],
            RISCV_CPUCTL => u32::from(self.halted) * RISCV_HALTED,
            BROM_CONFIG => self.brom_config,
            IMEM_CONTROL => self.imem.control,
            IMEM_DATA => self.imem.read(),
            IMEM_TAG => self
                .imem_tags
                .get(self.imem.word_index() * 4 / BLOCK)
                .copied()
                .unwrap_or(0),
            DMEM_CONTROL => self.dmem.control,
            DMEM_DATA => self.dmem.read(),
            _ => return None,
        })
    }

    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            HWCFG | HWCFG2 | BROM_STATUS | IRQ_STATUS | EMEM_SIZE => {}
            IRQ_SET => self.irq_status |= value & MESSAGE_INTERRUPT,
            FALCON_IRQ_CLEAR | IRQ_CLEAR => self.irq_status &= !value,
            IRQ_MASK => self.irq_mask = value,
            IRQ_DEST => self.irq_dest = value,
            EMEM_CONTROL => self.emem.control = value,
            EMEM_DATA => self.emem.write(value),
            COMMAND_HEAD => {
                if (EMEM_BASE..MESSAGE_BASE).contains(&value) && value.is_multiple_of(4) {
                    self.command_head = value;
                    self.process_commands();
                }
            }
            MESSAGE_TAIL => {
                if (MESSAGE_BASE..MESSAGE_BASE + QUEUE_BYTES).contains(&value)
                    && value.is_multiple_of(4)
                {
                    self.message_tail = value;
                }
            }
            COMMAND_TAIL | MESSAGE_HEAD => {}
            BROM_CONFIG => self.brom_config = value,
            CPU_CONTROL => {
                if value & START_CPU != 0 {
                    self.start();
                }
                return false; // Preserve CPU control readback in the register aperture.
            }
            RISCV_CPUCTL => {
                if value & RISCV_STARTCPU != 0 {
                    self.start();
                }
            }
            MAILBOX0 => self.mailbox[0] = value,
            MAILBOX1 => self.mailbox[1] = value,
            ENGINE_RESET => {
                if value & 1 != 0 {
                    *self = Self::default();
                }
                return false; // Keep the reset register's normal readback.
            }
            IMEM_CONTROL => self.imem.control = value,
            IMEM_DATA => self.imem.write(value),
            IMEM_TAG => {
                let block = self.imem.word_index() * 4 / BLOCK;
                if let Some(tag) = self.imem_tags.get_mut(block) {
                    *tag = value;
                }
            }
            DMEM_CONTROL => self.dmem.control = value,
            DMEM_DATA => self.dmem.write(value),
            _ => return false,
        }
        true
    }
}
