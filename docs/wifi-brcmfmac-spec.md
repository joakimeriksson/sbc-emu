# Emulated CYW43455 (BCM4345 rev 6) over SDIO for stock `brcmfmac` — device-side spec

Target: Raspberry Pi kernel `rpi-6.18.y`, unmodified `brcmfmac` + mmc core, up to `wlan0`
registered, brought up and accepting an `escan` request.

Sources (read-only copies): `cache/linux/brcmfmac/` (downloaded from
`raspberrypi/linux@rpi-6.18.y`). File names below refer to that directory:

| local file | tree path |
|---|---|
| `sdio.c`, `sdio.h`, `bcmsdh.c`, `chip.c`, `chip.h`, `firmware.c`, `bcdc.c`, `fwil.c`, `fwil.h`, `fwil_types.h`, `core.c`, `core.h`, `common.c`, `feature.c`, `cfg80211.c`, `fweh.c`, `fweh.h`, `fwsignal.c`, `p2p.c`, `btcoex.c` | `drivers/net/wireless/broadcom/brcm80211/brcmfmac/` |
| `chipcommon.h`, `brcm_hw_ids.h`, `soc.h`, `brcmu_d11.h` | `drivers/net/wireless/broadcom/brcm80211/include/` |
| `bcma.h`, `bcma_regs.h` | `include/linux/bcma/` |
| `sdio_ids.h`, `sdio.h.mmc` (= `include/linux/mmc/sdio.h`) | `include/linux/mmc/` |
| `mmc_sdio.c`, `mmc_sdio_cis.c`, `mmc_sdio_io.c`, `mmc_sdio_ops.c`, `mmc_sdio_irq.c`, `mmc_quirks.h` | `drivers/mmc/core/` |

Conventions: all multi-byte values on the backplane and in SDPCM/BCDC are **little endian**
except the firmware-event header (big endian, §7.4). "F0/F1/F2" = SDIO function 0/1/2.
"MUST" = the driver aborts/hangs otherwise; "MAY fail" = driver tolerates an error.

The emulator does **not** run the ARM firmware. It stores the downloaded image, and when the
host releases the CR4 from reset it switches into a "firmware running" state and implements
the firmware's host-facing behaviour (clock, F2 ready, SDPCM/BCDC ioctl server, events).

---------------------------------------------------------------------------------------------

## 0. Minimum viable sequence (overview)

1. **mmc core**: CMD5 (arg 0) → R4; CMD5 (arg = OCR) until ready bit; CMD3 → RCA; CMD7;
   CMD52 reads of CCCR 0x00/0x08/0x12/0x13, common CIS @0x1000; CMD52 write CCCR 0x13 (EHS),
   0x07 (4-bit); per function FBR 0x n00 and the function CIS. (§1)
2. **brcmfmac bind** to F2 (vendor 0x02d0, device 0xa9bf): set F1 blksz 64, F2 blksz 512,
   enable F1 (IOE1→IOR1). (§2.1)
3. **probe_attach**: CHIPCLKCSR write 0x28 / read-back, ALP available; read chipid at
   0x18000000; read EROM via `eromptr`; reset-dance on CR4 and D11 wrappers; CR4 RAM-size
   probe; chipcommon caps; SLEEPCSR KSO; CCCR 0xF1 RMW; PMU control RMW. (§2, §3)
4. Disable F2, CHIPCLKCSR=0, request firmware files (async).
5. **download**: ALP request; CMD53 F1 writes of firmware to 0x198000 and NVRAM to the top
   of RAM; write reset vector to backplane address 0; release CR4 (→ emulator "boots"). (§4)
6. HT request (needs 0xC0 in CHIPCLKCSR), FORCE_HT, `tosbmailboxdata = 0x00040000`, enable F2
   (IOE2 → IOR2 within 3 s), `hostintmask = 0x200000F0`, watermarks. Claim SDIO IRQ. (§4.5)
7. **BCDC ioctl traffic over SDPCM on F2** (§6, §7): `bus:txglomalign`, `cur_etheraddr`,
   `GET_REVINFO`, `clmload`*, `ver`, `clmver`, `mpc`, `join_pref`, `event_msgs` get/set,
   scan-time ioctls, `txbf`, `cap`, feature probes, `tlv`…, `GET_VERSION`, `GET_BANDLIST`,
   `nmode`/`bw_cap`/`rxchain`, `chanspecs`, `per_chan_info`, `event_msgs` set → **wlan0
   registered** (`register_netdev` in `brcmf_net_attach`).
8. `ip link set wlan0 up` → `C_UP`, scan times, `SET_PM`, roam ioctls, `SET_INFRA`, ARP/ND
   offload, `SET_FAKEFRAG`. Scan → `mpc 0`, (`vndr_ie`), `escan` set; answer with
   `E_ESCAN_RESULT` events (§7.5).

---------------------------------------------------------------------------------------------

## 1. Generic SDIO enumeration (Linux mmc core)

### 1.1 Command responses

| Cmd | Source | What the card must return |
|---|---|---|
| CMD5 `IO_SEND_OP_COND` arg=0 | `mmc_sdio.c:mmc_attach_sdio` → `mmc_sdio_ops.c:mmc_send_io_op_cond` (single pass when ocr==0) | R4: bits 23:0 OCR window (e.g. `0x00FF8000` = 2.7–3.6 V), bits 30:28 = number of I/O functions = **2**, bit 27 memory-present = **0**, bit 24 S18A = **0** (recommended: avoids CMD11 UHS voltage switch path in `mmc_sdio_init_card`), bit 31 C (ready) may be 1. |
| CMD5 arg=selected OCR | `mmc_send_io_op_cond` loops up to 100× with 10 ms delay until `resp[0] & MMC_CARD_BUSY` (bit 31) | Same R4 with bit 31 = 1 (just answer ready immediately). |
| CMD3 `SEND_RELATIVE_ADDR` | `mmc_sdio_init_card` → `mmc_send_relative_addr` | R6: RCA in bits 31:16 (any non-zero, e.g. 0x0001), status bits 15:0 = 0. |
| CMD7 `SELECT_CARD` | `mmc_select_card` | R1b, no error. |
| CMD52 `IO_RW_DIRECT` | `mmc_sdio_ops.c:mmc_io_rw_direct_host` | Arg: bit31 R/W, bits 30:28 fn, bit27 RAW (read-after-write), bits 25:9 register address (17 bit), bits 7:0 write data. R5: bits 7:0 = data; must NOT set bit 11 (ERROR → -EIO), bit 9 (FUNCTION_NUMBER → -EINVAL), bit 8 (OUT_OF_RANGE → -ERANGE). |
| CMD53 `IO_RW_EXTENDED` | `mmc_sdio_ops.c:mmc_io_rw_extended` | Arg: bit31 R/W, bits 30:28 fn, bit27 block mode, bit26 OP code (1 = incrementing address), bits 25:9 address, bits 8:0 count (byte mode: byte count, 0 means 512; block mode: block count ≤ 511). R5 flags as for CMD52. |

Byte vs block mode (`mmc_sdio_io.c:sdio_io_rw_ext_helper`): if CCCR caps has SMB and the size >
`sdio_max_byte_size(func)`, the bulk goes as block-mode CMD53 of `cur_blksize` blocks, the
remainder as byte-mode CMD53 of ≤ `sdio_max_byte_size` bytes.
`sdio_max_byte_size = min(host->max_blk_size, func->max_blksize, 512)`
(`mmc_sdio_io.c:sdio_max_byte_size`). **`func->max_blksize` comes from the function's
CISTPL_FUNCE — if it is 0 the byte-mode loop never advances (hang). See §1.3.**

### 1.2 CCCR (function 0, addresses 0x00–0xFF)

Read by `mmc_sdio.c:sdio_read_cccr`, `sdio_enable_wide`, `mmc_sdio_switch_hs`, and
`mmc_sdio_io.c:sdio_enable_func`, `mmc_sdio_irq.c`, plus Broadcom vendor regs by brcmfmac.

| Off | Name | Value / behaviour |
|---|---|---|
| 0x00 | CCCR/SDIO rev | `0x32` recommended (SDIO spec 2.00 in bits 7:4, CCCR rev 1.20 in bits 3:0). Must be CCCR rev ≤ 3 (`sdio_read_cccr` rejects >3). SDIO rev must be > 1.00 so FUNCE enable-timeout is parsed (`mmc_sdio_cis.c:cistpl_funce_func`). `0x43` (CCCR 3.00) also fine as long as CMD5 S18A=0. |
| 0x01 | SD rev | not read; `0x02`. |
| 0x02 | IOEx | R/W. bit1 = F1 enable, bit2 = F2 enable. Host does RMW (`sdio_enable_func`, `sdio_disable_func`). |
| 0x03 | IORx | RO. bit1 set as soon as IOE1 set. bit2 set only when IOE2 set **and** emulated firmware is running (§4.5). Clears when the IOE bit clears. Polled by `sdio_enable_func` (timeout = `func->enable_timeout`), and by `sdio.c:brcmf_sdio_f2_ready` before every control-frame TX. |
| 0x04 | IENx | R/W. bit0 master, bit1 F1, bit2 F2 (`mmc_sdio_irq.c:sdio_claim_irq`; OOB path in `bcmsdh.c:brcmf_sdiod_intr_register`). |
| 0x05 | INTx | RO. bit1 = F1 interrupt pending (= SDIO core `intstatus & hostintmask != 0`, §5.3). Read by `mmc_sdio_irq.c:sdio_get_pending_irqs` and `sdio.c:brcmf_sdio_bus_watchdog` (poll mode only). F2 never interrupts (dummy handler). |
| 0x06 | ABORT | W. Writing a function number (2) aborts the current CMD53 to that function (`bcmsdh.c:brcmf_sdiod_abort`); emulator: drop the current F2 RX frame / TX partial. bit3 RES = card reset. |
| 0x07 | Bus IF ctrl | R/W. Reset value bits1:0 = 00 (1-bit). Host writes width=`0x02` (4-bit) (`sdio_enable_wide`). Bits 1:0 = `01` is reserved (warn). bit7 CD_DISABLE may be written. |
| 0x08 | Card caps | `0x13` recommended: SDC (0x01) + **SMB (0x02, required for block mode)** + S4MI (0x10). Do **not** set LSC (0x40). |
| 0x09–0x0B | Common CIS ptr | `0x001000` (24-bit LE). Broadcom uses 0x1000 (`sdio.c` `SBSDIO_CIS_BASE_COMMON`). |
| 0x0C–0x0F | suspend/select/exec/ready | 0. |
| 0x10–0x11 | F0 block size | R/W, not used by Linux for F0 data. |
| 0x12 | Power ctrl | 0 (or `0x01` SMPC). Read only if CCCR rev ≥ 1.10. |
| 0x13 | Bus speed | `0x01` (SHS). Host RMW sets bit1 EHS (`mmc_sdio_switch_hs`); store it. |
| 0x14–0x16 | UHS / drive / int-ext | Read only if CCCR rev 3.00 **and** S18A was granted; otherwise unused. |
| 0xF0 | `SDIO_CCCR_BRCM_CARDCAP` | R/W, 0. Written only by `sdio.c:brcmf_sdio_sr_init` (SR, avoid). |
| 0xF1 | `SDIO_CCCR_BRCM_CARDCTRL` | R/W, 0. `sdio.c:brcmf_sdio_probe_attach` reads it and writes back with bit1 WLANRESET set. **Error here aborts probe.** |
| 0xF2 | `SDIO_CCCR_BRCM_SEPINT` | R/W. Only written in OOB-IRQ mode (Pi 4 DT has no OOB interrupt → in-band). |

Note `bcmsdh.c:brcmf_ops_sdio_probe` sets `MMC_QUIRK_LENIENT_FN0` so F0 writes outside the
standard range (0xF0–0xF2) are allowed from the host side.

### 1.3 FBR and CIS

FBR for function n at `n*0x100` (`sdio.h.mmc` `SDIO_FBR_BASE`):

| Off | Value |
|---|---|
| n00 `STD_IF` | `0x00` (class none). If low nibble == 0xF the core also reads n01 (`mmc_sdio.c:sdio_read_fbr`). |
| n09–n0B `CIS` | 24-bit pointer to function CIS, e.g. F1 `0x001100`, F2 `0x001200` (any F0 address in 0x1000–0x17FFF). |
| n10–n11 `BLKSIZE` | R/W LE16. Host writes F1 = 64 (`0x40,0x00`), F2 = 512 (`0x00,0x02`) (`bcmsdh.c:brcmf_sdiod_probe` → `mmc_sdio_io.c:sdio_set_block_size`). Use this value for CMD53 block-mode lengths per function. |

CIS parser: `mmc_sdio_cis.c:sdio_read_cis`. Tuples are read byte-by-byte with CMD52 F0:
`code, link, data[link]`; `0xFF` code (or link 0xFF) ends; `0x00` = null tuple (no link).
Known tuples (`cis_tpl_list`): 0x15 VERS_1 (min 3), 0x20 MANFID (min 4), 0x21 FUNCID
(min 2, stored unparsed), 0x22 FUNCE, 0x91 SDIO_STD. Unknown tuples (incl. Broadcom vendor
0x80–0x8F) are kept and ignored.

**Common CIS (@0x1000)** — `card->cis`:
```
20 04  d0 02  bf a9          ; CISTPL_MANFID vendor 0x02D0, device 0xA9BF
21 02  0c 00                 ; CISTPL_FUNCID (optional)
22 04  00  00 02  32         ; CISTPL_FUNCE type 0: FN0 blksize 0x0200, max_tran_speed 0x32 (25 MHz)
ff                           ; end
```
(`cistpl_manfid`: vendor = buf[0]|buf[1]<<8, device = buf[2]|buf[3]<<8; `cistpl_funce_common`:
blksize = buf[1..2], speed byte buf[3] → `speed_val[(b>>3)&15]*speed_unit[b&7]`; `0x5A` = 50 MHz.)

**Function CIS (F1 @0x1100, F2 @0x1200)** — `cistpl_funce_func` requires type byte 0x01 and
**≥ 42 bytes** when SDIO rev > 1.00:
```
20 04  d0 02  bf a9          ; MANFID (optional – if absent func inherits card IDs, sdio_read_func_cis)
22 2a  01 <41 bytes>         ; FUNCE type 1, 42 bytes total
ff
```
FUNCE type-1 body offsets (relative to buf[0] = type 0x01): buf[12..13] = `TPLFE_MAX_BLK_SIZE`
(F1: `0x0040`, F2: `0x0200`); buf[28..29] = `TPLFE_ENABLE_TIMEOUT_VAL` in 10 ms units (use
`0x0064` = 1 s; the driver overrides F2 to 3000 ms in `bcmsdh.c:brcmf_sdiod_probe`). Other
bytes 0.

**Function/device IDs that brcmfmac binds** (`bcmsdh.c` `brcmf_sdmmc_ids`, `sdio_ids.h`):
vendor `SDIO_VENDOR_ID_BROADCOM` 0x02d0 with device `0xa9bf`
(`SDIO_DEVICE_ID_BROADCOM_43455`, fw vendor WCC → module `brcmfmac-wcc`) **or** `0x4345`
(`SDIO_DEVICE_ID_BROADCOM_4345`, vendor CYW). Use **0xa9bf** (what the Pi 4 module reports,
and the only ID that gets the 43455 watermark settings in `sdio.c:brcmf_sdio_firmware_callback`).
Both F1 and F2 must carry the ID; the driver ignores F1 (`return 0`) and does all work from
the F2 probe using `card->sdio_func[0]` as F1 (`bcmsdh.c:brcmf_ops_sdio_probe`).

No mmc quirks exist for Broadcom IDs (`mmc_quirks.h`).

### 1.4 Function enable/ready

`mmc_sdio_io.c:sdio_enable_func`: CMD52 read IOEx, write IOEx | (1<<n), poll IORx bit n
until set or `enable_timeout` ms. `sdio_disable_func` clears the IOEx bit.

---------------------------------------------------------------------------------------------

## 2. brcmfmac SDIO probe

### 2.1 Order (`bcmsdh.c:brcmf_sdiod_probe`, `sdio.c:brcmf_sdio_probe`, `brcmf_sdio_probe_attach`)

1. F1 blksize 64, F2 blksize 512 (FBR writes), `func2->enable_timeout = 3000`.
2. `sdio_enable_func(F1)` — IOE1 → IOR1.
3. `brcmf_sdio_probe_attach`:
   1. F1 CMD53 read of `0x18000000` (debug print of chipid).
   2. CHIPCLKCSR ← `0x28` (`BRCMF_INIT_CLKCTL1` = FORCE_HW_CLKREQ_OFF|ALP_AVAIL_REQ); read
      back; **`(val & ~0xC0) == 0x28` required** else probe fails.
   3. `chip.c:brcmf_chip_attach` → `sdio.c:brcmf_sdio_buscoreprep` (§2.3), recognition (§3).
   4. Requires cores `BCMA_CORE_SDIO_DEV` (0x829) and `BCMA_CORE_CHIPCOMMON` (0x800).
   5. `brcmf_sdio_kso_init`: if SDIO core rev ≥ 12: read SLEEPCSR (0x1001F); if bit0 clear
      write it set. **Read error aborts.**
   6. `brcmf_sdio_drivestrengthinit`: no-op for 4345 (only 4330/4334/43143/43362 keys).
   7. CCCR 0xF1 RMW (|= 0x02). Errors abort.
   8. PMU `pmucontrol` (+0x600 of PMU core; = chipcommon unless separate PMU, §3.4) read,
      `|= 0x2 << 13` (= 0x4000, RES_RELOAD), write. Errors abort.
4. Watchdog timer/thread created; `bus->blocksize = F2 cur_blksize` (512),
   `roundup = min(512, blocksize)`.
5. `sdio_disable_func(F2)` (IOE2 cleared), CHIPCLKCSR ← `0x00`.
6. Async firmware request (§4.1) → `brcmf_sdio_firmware_callback`.

### 2.2 Function-1 address map (emulator side)

From `sdio.h`, `bcmsdh.c:brcmf_sdiod_readl/writel/ramrw/set_backplane_window`:

| F1 address | Meaning |
|---|---|
| 0x00000–0x07FFF | Backplane window, 2-byte access mode (not used by the driver). |
| 0x08000–0x0FFFF | Backplane window with `SBSDIO_SB_ACCESS_2_4B_FLAG` (0x8000) = 4-byte access. Backplane address = `window_base | (addr & 0x7FFF)`. **All** driver backplane accesses use this range (CMD53, incrementing, byte or block mode). |
| 0x10000–0x1001F | F1 misc registers (CMD52 only), table below. |

`ramrw` never crosses a 32 KB window inside one CMD53 (it splits on `SBSDIO_SBWINDOW_MASK`).
`readl/writel` = CMD53 byte mode, 4 bytes, incrementing.

F1 misc registers (`sdio.h`, `sdio.c`):

| Addr | Name | Required behaviour |
|---|---|---|
| 0x10005–7 | GPIO_SELECT/OUT/EN | 43362 only; ignore. |
| 0x10008 | `SBSDIO_WATERMARK` | R/W; host writes `0x60` (`CY_43455_F2_WATERMARK`). |
| 0x10009 | `SBSDIO_DEVICE_CTL` | R/W. bit2 `CA_INT_ONLY` (host sets while waiting for HT clock, §8), bit4 `F2WM_ENAB` (host sets after F2 enable). |
| 0x1000A | `SBADDRLOW` | backplane window bits 15:8 (only bit 15 significant) |
| 0x1000B | `SBADDRMID` | window bits 23:16 |
| 0x1000C | `SBADDRHIGH` | window bits 31:24. Window = `(HIGH<<24 | MID<<16 | LOW<<8) & 0xFFFF8000`. Host writes LOW, MID, HIGH in that order (`brcmf_sdiod_set_backplane_window`); apply each byte immediately. **Reset value must be 0**: the driver caches `sbwad = 0` and skips the write when the target window is 0. |
| 0x1000D | `FRAMECTRL` | W. bit0 `SFC_RF_TERM` = discard current F2 RX frame; bit1 `SFC_WF_TERM` = discard partial TX. |
| 0x1000E | `CHIPCLKCSR` | See §2.3. |
| 0x1000F | `SDIOPULLUP` | R/W; host writes 0. |
| 0x10019/1A | `WFRAMEBCLO/HI` | read 0 (polled ≤3× in `brcmf_sdio_txfail`). |
| 0x1001B/1C | `RFRAMEBCLO/HI` | read 0 (polled up to 0xFFFF× in `brcmf_sdio_rxfail` until both 0). |
| 0x1001D | `MESBUSYCTRL` | R/W; host writes `0xD0` (`CY_43455_MESBUSYCTRL` = 0x50 \| 0x80). |
| 0x1001E | `WAKEUPCTRL` | R/W; SR only. |
| 0x1001F | `SLEEPCSR` | bit0 KSO (R/W), bit1 DEVON (RO). Return `0x03` once KSO written 1 (and from reset — then the init write is skipped). §8. |

### 2.3 CHIPCLKCSR (F1 0x1000E) handshake

Bits (`sdio.c`): 0x01 FORCE_ALP, 0x02 FORCE_HT, 0x04 FORCE_ILP, 0x08 ALP_AVAIL_REQ,
0x10 HT_AVAIL_REQ, 0x20 FORCE_HW_CLKREQ_OFF, **0x40 ALP_AVAIL (RO), 0x80 HT_AVAIL (RO)**.
`SBSDIO_ALPAV(v) = v & 0xC0`, `SBSDIO_HTAV(v) = (v & 0xC0) == 0xC0` — **HT available means
both bits set.**

Emulator rule: store bits 5:0 verbatim; on read return
`stored | (any of 0x01,0x02,0x08,0x10 set ? 0xC0 : 0x00)` — i.e. clocks become available
instantly. (A pure `0x40` "ALP only" state is never needed.)

Driver usage:
- `brcmf_sdio_buscoreprep`: write `0x28`, read back must equal `0x28` modulo 0xC0, then spin
  (≤1 s, `PMU_MAX_TRANSITION_DLY`) for ALPAV; write `0x21` (HW_CLKREQ_OFF|FORCE_ALP); write
  SDIOPULLUP=0.
- `brcmf_sdio_htclk(on)`: write `0x08` (alp_only, during download) or `0x10`; read; if not
  available and `pendok` → set DEVICE_CTL.CA_INT_ONLY and wait for an interrupt (avoid!);
  else poll every 5–10 ms up to 1 s. `htclk(off)`: write `0x00`.
- firmware_callback: read, write `|0x02` (FORCE_HT), later restore saved value.

---------------------------------------------------------------------------------------------

## 3. Chip identification and core discovery (`chip.c`)

### 3.1 chipcommon chipid (`0x18000000`, `chip.c:brcmf_chip_recognition`, `chipcommon.h`)

`SI_ENUM_BASE_DEFAULT = 0x18000000` (`soc.h`, `chip.c:brcmf_chip_enum_base`).

| bits | field | value |
|---|---|---|
| 15:0 | chip id | `0x4345` (`BRCM_CC_4345_CHIP_ID`) |
| 19:16 | chip rev | `6` |
| 23:20 | package | any (0) |
| 27:24 | core count | informational (e.g. number of EROM cores) |
| 31:28 | type | **1 = AXI** (`SOCI_AI`). 0 (SB) is rejected for non-4329. |

Example: `0x15064345`. `0xFFFFFFFF` = read failure → -ENODEV.
Chip rev 6 matters for firmware naming: `sdio.c` `brcmf_sdio_fwnames`
`BRCMF_FW_ENTRY(0x4345, 0xFFFFFDC0, 43455)` → rev 6 → `brcmfmac43455-sdio.*`
(rev 9 → 43456).

Other chipcommon registers used (offsets from `chipcommon.h struct chipcregs`):

| Off | Reg | Value / use |
|---|---|---|
| 0x004 | capabilities | `brcmf_chip_setup`. bit28 `CC_CAP_PMU` (0x10000000): set it (realistic) or clear it (then pmurev=0 → SR impossible). |
| 0x0AC | capabilities_ext | bit6 `BCMA_CC_CAP_EXT_AOB_PRESENT` (0x40): **keep clear** → PMU regs live in chipcommon (`brcmf_chip_get_pmu` uses a separate PMU core only if cc rev ≥ 35 and this bit set). |
| 0x0FC | eromptr | Backplane address of the EROM table (e.g. `0x18120000`). |
| 0x600 | pmucontrol | R/W (RMW in probe_attach). |
| 0x604 | pmucapabilities | bits 7:0 = pmurev (e.g. 28). |
| 0x650/0x654 | chipcontrol_addr/data | `brcmf_chip_sr_capable` for 4345 (pmurev ≥ 17): writes 3 to addr, reads data; **return bit2 = 0** → SR disabled (recommended). |

### 3.2 EROM (DMP) format (`chip.c:brcmf_chip_dmp_erom_scan`, `brcmf_chip_dmp_get_desc`, `brcmf_chip_dmp_get_regaddr`)

32-bit LE words read sequentially from `eromptr`. Low nibble = descriptor type:

| type (bits 3:0) | meaning |
|---|---|
| `0x0` | empty (skipped) |
| `0x1` | component (CIA / CIB) |
| `0x3` | master port descriptor |
| `0x5` | address descriptor (`0xD` = same with bit3 "AG32": a 64-bit upper word follows) |
| `0xF` | end of table |

Component A (CIA): bits 31:20 designer (0x4BF Broadcom; unchecked), **19:8 part number = core
id**, 7:4 class, 3:0 = 1.
Component B (CIB, must immediately follow CIA else `WARN_ON` + -EFAULT): **31:24 revision**,
23:19 #slave wrappers (nsw), 18:14 #master wrappers (nmw), 13:9 #slave ports, 8:4 #master
ports, 3:0 = 1. Cores with `nmw + nsw == 0` are skipped (except PMU 0x827 / GCI 0x840).
Address descriptor: **31:12 base address**, 11:8 port number, **7:6 slave type** (0 slave,
1 bridge, 2 slave-wrapper, 3 master-wrapper), **5:4 size type** (0 = 4 KB, 1 = 8 KB, 2 = 16 KB,
3 = size descriptor follows), 3:0 = 0x5.

`get_regaddr` per core: if the first descriptor after CIB is a master port (type 3) the
wrapper kind sought is MWRAP (3), if it is an address descriptor the wrapper kind is SWRAP (2).
It then scans address descriptors; only 4 KB/8 KB ones count; `regbase` = first type-0 slave,
`wrapbase` = first wrapper of the sought kind. Hitting the next CIA stops (core still added);
hitting EOT returns -EFAULT (core dropped). **The first core in the table must be chipcommon**
(`brcmf_chip_get_chipcommon` / `brcmf_chip_setup` use list head).

Minimal 43455 EROM (no master ports, slave wrappers; rev numbers are plausible, only the
SDIO-core rev ≥ 12 is behaviourally checked):

| # | core | id | rev | base | wrapper | words |
|---|---|---|---|---|---|---|
| 1 | ChipCommon | 0x800 | e.g. 54 (0x36) | 0x18000000 | 0x18100000 | `4BF80001 36080201 18000005 18100085` |
| 2 | 802.11 (D11) | 0x812 | e.g. 42 (0x2A) | 0x18001000 | 0x18101000 | `4BF81201 2A080201 18001005 18101085` |
| 3 | ARM CR4 | 0x83E | e.g. 7 | 0x18002000 | 0x18102000 | `4BF83E01 07080201 18002005 18102085` |
| 4 | SDIO device | 0x829 | **≥ 12**, e.g. 21 (0x15) | 0x18004000 | 0x18104000 | `4BF82901 15080201 18004005 18104085` |
| – | end | | | | | `0000000F` |

(CIB `0xRR080201` = rev RR, nsw=1, nsp=1. Slave addr = `base|0x05`, wrapper addr =
`wrap|0x80|0x05`.) No SOCRAM core is needed: `brcmf_chip_cores_check` requires a CPU
(CR4 satisfies it) and only demands SOCRAM for CM3. A separate PMU core (0x827) is optional
and unused when AOB is clear.

### 3.3 Wrapper (AI/DMP) registers (`bcma_regs.h`, `chip.c:brcmf_chip_ai_*`)

| wrap+off | name | behaviour |
|---|---|---|
| 0x408 | `BCMA_IOCTL` | Plain R/W. bit0 CLK, bit1 FGC, core-specific bits (CR4: 0x20 CPUHALT; D11: 0x04 PHYCLOCKEN, 0x08 PHYRESET). |
| 0x800 | `BCMA_RESET_CTL` | Plain R/W, bit0 RESET; writes take effect immediately. `coredisable` writes 1 then spins ≤300 µs until it reads exactly 1; `resetcore` writes 0 until bit0 reads 0 (≤50 tries). |
| 0x804 | `BCMA_RESET_ST` | not read by brcmfmac; return 0. |

`iscoreup` = `(ioctl & 0x3) == 0x1 && !(resetctrl & 1)`.

Sequence during attach (`brcmf_chip_recognition` → `brcmf_chip_set_passive` →
`brcmf_chip_cr4_set_passive`):
- CR4: read ioctl; `resetcore(prereset = ioctl & 0x20, reset = 0x20, postreset = 0x20)`:
  [if resetctrl==0: ioctl ← pre|0x03, resetctrl ← 1, wait] ioctl ← 0x23; resetctrl ← 0;
  ioctl ← 0x21. **Final state: CPU out of reset but CPUHALT=1** ("passive").
- D11 (every 0x812 core): `coredisable(0x0C, 0x04)` → ioctl 0x0F, resetctrl 1, ioctl 0x07.

### 3.4 RAM info (`chip.c:brcmf_chip_get_raminfo`, `brcmf_chip_tcm_ramsize`, `brcmf_chip_tcm_rambase`)

With a CR4 core present: `rambase = 0x198000` (hard-coded for 0x4345), `ramsize` from CR4
core registers (core base, not wrapper):

| CR4 off | reg | value |
|---|---|---|
| 0x04 | `ARMCR4_CAP` | bits 3:0 nab (#A banks), bits 7:4 nbb (#B banks). |
| 0x40 | `ARMCR4_BANKIDX` | W: bank index. |
| 0x44 | `ARMCR4_BANKINFO` | for the selected bank: bits 6:0 size-1 in units of 8 KB (1 KB if bit9 set). |

`ramsize = Σ (bsz+1) * 8 KB`. Real 43455 = `0xC8000` (800 KB). Example: CAP = `0x00000005`
(5 A-banks), each BANKINFO = `0x13` (20 × 8 KB = 160 KB) → 0xC8000. RAM therefore spans
**0x198000–0x25FFFF**. ramsize must be non-zero and ≤ 4 MB.

---------------------------------------------------------------------------------------------

## 4. Firmware download and start

### 4.1 Files (`sdio.c:brcmf_sdio_prepare_fw_request`, `firmware.c`)

`brcmfmac43455-sdio.bin` (required), `.txt` NVRAM (required), `.clm_blob` (optional); with
board-type prefix tried first (e.g. `brcmfmac43455-sdio.raspberrypi,4-model-b.txt`). These
must exist in the guest rootfs (stock Raspberry Pi OS ships them). The emulator only needs
to accept the bytes.

### 4.2 Download (`sdio.c:brcmf_sdio_download_firmware`)

1. `clkctl(CLK_AVAIL)` with `alp_only` → CHIPCLKCSR ← 0x08, needs 0x40/0xC0.
2. `rstvec = le32(fw[0..3])`.
3. Firmware: `brcmf_sdiod_ramrw(write, 0x198000, fw, size)` — window set via SBADDR*, CMD53 F1
   writes at F1 `0x8000 + (addr & 0x7FFF)`, block mode 64-byte blocks + byte-mode tail, ≤32 KB
   per window.
4. (DEBUG builds only, `brcmf_sdio_verifymemory`): reads everything back in 2 KB chunks and
   compares — RAM must read back exactly.
5. NVRAM: written at `rambase + ramsize - nvlen` = `0x260000 - nvlen`. Format
   (`firmware.c:brcmf_fw_nvram_strip`): `key=value\0…\0`, zero-padded to a multiple of 4
   (`roundup(len+1, 4)`), then a 4-byte LE **length token**
   `token = (~words << 16) | (words & 0xFFFF)` with `words = padded_len / 4`. So the last
   word of RAM (`0x25FFFC`) holds the token after download.
6. `brcmf_chip_set_active` → `brcmf_chip_cr4_set_active`:
   - `sdio.c:brcmf_sdio_buscore_activate`: write `0xFFFFFFFF` to SDIO core `intstatus`
     (W1C, §5.1); write `rstvec` (4 bytes) to **backplane address 0x00000000** (window 0) —
     the emulator must accept a 4-byte write there (CR4 ATCM/vector alias).
   - `resetcore(CR4, prereset = 0x20, reset = 0, postreset = 0)`: ioctl ← 0x23, resetctrl ← 1,
     ioctl ← 0x03, resetctrl ← 0, ioctl ← 0x01.
   - **Emulator trigger:** CR4 wrapper `ioctl & 0x20 == 0` and `resetctrl == 0` after a
     download → enter FW_RUNNING (§4.4).
7. `clkctl(CLK_SDONLY)` → CHIPCLKCSR ← 0.

### 4.3 sdpcm_shared (only needed for DEBUG builds / crash dumps)

`sdio.c:brcmf_sdio_readshared` exists only under `#ifdef DEBUG` (CONFIG_BRCMDBG); it is called
on DEVREADY/FWREADY mailbox (`brcmf_sdio_hostmail` → `brcmf_sdio_get_console_addr`) and from
`brcmf_sdio_checkdied` on failures. It reads the last RAM word (`rambase+ramsize-4`); the
pointer is valid unless 0 or it still looks like the NVRAM token
(`((~a>>16)&0xFFFF) == (a&0xFFFF)`). Structure (`struct sdpcm_shared_le`, LE u32):
`flags, trap_addr, assert_exp_addr, assert_file_addr, assert_line, console_addr,
msgtrace_addr, u8 tag[32], brpt_addr` (88 bytes). Only check: `flags & 0xFF ≤ 3` (`SDPCM_SHARED_VERSION`); bits 0x100
ASSERT_BUILT, 0x200 ASSERT, 0x400 TRAP must be 0. Recommendation: on boot, place a struct with
`flags = 0x0003`, all other fields 0, somewhere in RAM and overwrite the last RAM word with
its address. Harmless for non-DEBUG kernels.

### 4.4 What "firmware running" must do

- CHIPCLKCSR: HT available on request (rule in §2.3 already covers it).
- IORx bit2 follows IOEx bit2 (F2 ready) — `sdio_enable_func(F2)` times out after 3000 ms.
- Optionally post `tohostmailboxdata = HMB_DATA_FWREADY (0x08) | (4 << 16)` and raise
  `I_HMB_HOST_INT` once F2 is enabled — **not required**; the driver never waits for it
  (`brcmf_sdio_hostmail` only checks the version and, in DEBUG, reads sdpcm_shared).
  Never raise `I_HMB_HOST_INT` with `tohostmailboxdata == 0` (§8.4).
- Serve SDPCM/BCDC on F2 (§6, §7).

### 4.5 F2 bring-up (`sdio.c:brcmf_sdio_firmware_callback`)

1. watchdog started (10 ms); `clkctl(CLK_AVAIL)`: CHIPCLKCSR ← 0x10, must read 0xC0 bits
   within 1 s (polled; `pendok=false`).
2. read CHIPCLKCSR, write `|0x02` (FORCE_HT).
3. SDIO core `tosbmailboxdata` (+0x48) ← `0x00040000` (`SDPCM_PROT_VERSION 4 << 16`).
4. `sdio_enable_func(F2)` (IOE |= 0x04, poll IOR bit2 ≤3 s). Failure → checkdied, release
   driver.
5. `hostintmask` (+0x24) ← `0x200000F0` (`I_HMB_SW_MASK | I_CHIPACTIVE`).
6. 43455 (device 0xa9bf): WATERMARK ← 0x60; DEVICE_CTL |= 0x10; MESBUSYCTRL ← 0xD0.
7. `brcmf_chip_sr_capable` (§3.1) → false → CHIPCLKCSR ← saved value.
8. state = DATA; `brcmf_sdiod_intr_register`: in-band → `sdio_claim_irq(F1)`, `(F2)` →
   CCCR IENx |= 0x01|0x02|0x04.
9. `brcmf_alloc`, `brcmf_attach` (§7).

---------------------------------------------------------------------------------------------

## 5. SDIO device core registers and interrupts

### 5.1 Registers (core base 0x18004000, `sdio.h struct sdpcmd_regs`, `SD_REG()`)

| Off | Reg | Behaviour |
|---|---|---|
| 0x20 | `intstatus` | **W1C.** Firmware-side events set bits; host reads, masks with `hostintmask`, writes the set bits back to clear (`sdio.c:brcmf_sdio_intr_rstatus`). |
| 0x24 | `hostintmask` | R/W. Host sets `0x200000F0`. |
| 0x40 | `tosbmailbox` | Host → dongle. bit0 `SMB_NAK` (host NAKs an RX frame, §8.4), bit1 `SMB_INT_ACK` (ack of host-mailbox), bit2 `SMB_USE_OOB`, bit3 `SMB_DEV_INT`. |
| 0x44 | `tohostmailbox` | unused by host. |
| 0x48 | `tosbmailboxdata` | Host writes `0x00040000` before enabling F2. |
| 0x4C | `tohostmailboxdata` | Read by host on `I_HMB_HOST_INT`: `HMB_DATA_NAKHANDLED 0x01`, `DEVREADY 0x02`, `FC 0x04`, `FWREADY 0x08`, `FWHALT 0x10` (→ `brcmf_fw_crashed`!), bits 23:16 protocol version (must be 4), bits 31:24 FC bits. |
| other | | Return 0 / accept writes. |

intstatus bits of interest (`sdio.c`): `I_HMB_SW0` 0x10 FC_STATE (while set, host stops data
TX), `I_HMB_SW1` 0x20 FC_CHANGE, **`I_HMB_SW2` 0x40 FRAME_IND (RX frame(s) available)**,
**`I_HMB_SW3` 0x80 HOST_INT (mailbox data)**, `I_CHIPACTIVE` 0x20000000 (ignored).

### 5.2 Interrupt delivery

Pi 4 DT gives no OOB IRQ → in-band SDIO card interrupt (`bcmsdh.c:brcmf_sdiod_intr_register`
else-branch). The card asserts DAT1 interrupt (and CCCR INTx bit1 reads 1) while
`(intstatus & hostintmask) != 0` **and** IENx has master (bit0) and F1 (bit1) set. Make it
**level**: the mmc core masks the card IRQ while running handlers and re-arms after; if
the condition is still true it must re-fire. The handler `bcmsdh.c:brcmf_sdiod_ib_irqhandler`
→ `sdio.c:brcmf_sdio_isr(in_isr=false)` reads/clears intstatus and schedules the DPC.
If the host controller lacks SDIO IRQ support, the mmc core polls CCCR 0x05 every ≤10 ms
(`mmc_sdio_irq.c:sdio_irq_thread`) — same semantics.

---------------------------------------------------------------------------------------------

## 6. SDPCM framing on F2

### 6.1 Transport

- Host → dongle: `brcmf_sdiod_send_buf/send_pkt`: CMD53 **F2 write, incrementing address,
  addr 0x8000** (`(cc_core->base & 0x7FFF) | 0x8000`), size rounded up to 4; block mode
  (512-byte blocks) when > 512 bytes, byte-mode tail. Treat F2 writes as a FIFO: concatenate
  until the frame length from the HW header is reached (padding after it is discarded).
  Note: before every F2 access the driver also writes SBADDR* to select the chipcommon window
  (harmless).
- Dongle → host: `brcmf_sdiod_recv_buf/recv_pkt` → `sdio_readsb` = CMD53 **F2 read, fixed
  address 0x8000**, size rounded to 4. The driver first reads **64 bytes**
  (`BRCMF_FIRSTREAD`), parses the header, then reads `len - 64` rounded up to 4/8
  (`head_align`) or to the 512 block size (`brcmf_sdio_read_control`, `brcmf_sdio_pad`).
- Emulator RX model: queue of frames. A read that starts with no frame "in progress" pops
  the next frame; return frame bytes then zeros; once the cursor reaches the end of the
  frame (padding included in the same read) the frame is complete. If the queue is empty
  return **all zeros** (the HW header `0000 0000` = "no more frames", `brcmf_sdio_hdparse`
  → -ENODATA, ends the read loop). Frames ≤ 64 bytes are fully consumed by the first read.
  `FRAMECTRL.RF_TERM` or CCCR ABORT(2) drops the in-progress frame.

### 6.2 Header layout (`sdio.c` comments + `brcmf_sdio_hdparse`, `brcmf_sdio_hdpack`)

```
off  size  field
0    u16   frame length L (incl. all headers, excl. trailing pad)
2    u16   ~L  (checksum: (L ^ chk) must be 0xFFFF; L=0,chk=0 → no frame)
--- (host TX only, if txglom enabled: 8-byte HW ext header here – avoid, §7.2) ---
4    u8    sequence number
5    u8    bits 3:0 channel (0 control, 1 event, 2 data, 3 glom); bit7 = glom descriptor
6    u8    next-frame length in 16-byte units (RX: 0 recommended)
7    u8    data offset (from frame start; ≥ 12, ≤ L)
8    u8    flow-control bitmap (RX; 0 = all priorities open)
9    u8    max sequence (TX credit window, RX only)
10   u16   reserved (0)
```
Header size 12 (`SDPCM_HDRLEN`).

Driver checks on RX (`brcmf_sdio_hdparse`): checksum; `L ≥ 12`; for non-control channels
`L ≤ 2048` (`MAX_RX_DATASZ`); data offset in [12, L]; `nextlen*16 ≤ 2048`; sequence
mismatch only logs. Window: `tx_max = byte9`; if `(u8)(tx_max - host_tx_seq) > 0x40` it is
replaced by `tx_seq + 2`.

Credit (`sdio.c:txctl_ok`, `data_ok`): host may send while `(u8)(tx_max - tx_seq) != 0` and
`< 0x80`. Host starts with `tx_seq = 255`, `tx_max = 0` (`brcmf_sdio_probe`), so exactly
**one** frame can be sent before the first RX header arrives. Every RX frame must carry
`byte9 = (seq_of_last_host_frame_received + 1 + window) & 0xFF` with window e.g. 8 (and
≤ 0x40). Host TX sequence numbers increment per frame (`brcmf_sdio_tx_ctrlframe`).

Host TX control frame (`brcmf_sdio_tx_ctrlframe`): channel 0, `doff = 12 + align pad`,
seq = tx_seq, `L` = unpadded length, frame then padded to 4/8 or to a 512 multiple when
`L > 512`. Firmware must use `doff` to find the BCDC payload.

RX frame emitted by the emulator for each response/event: `L = 12 + payload`, `chk = ~L`,
seq = rx counter (start 0), channel 0 (ioctl response) or 1 (event), nextlen 0, doff 12,
fc 0, window as above. Then set intstatus `0x40` (FRAME_IND).

---------------------------------------------------------------------------------------------

## 7. BCDC protocol and the ioctl/iovar dialogue

### 7.1 Control channel (`bcdc.c`)

`struct brcmf_proto_bcdc_dcmd` (16 bytes, LE):

| off | field | host request | dongle response |
|---|---|---|---|
| 0 | `cmd` | ioctl number | echo |
| 4 | `len` | length of the ioctl buffer (for iovars: `strlen(name)+1+datalen`) | echo (driver ignores) |
| 8 | `flags` | bits 31:16 request id (pre-incremented u16 `reqid`, first = 1), bits 15:12 interface index, bit1 `SET`(0x02) for set, bit0 ERROR = 0 | **same id** in 31:16 (driver loops/drops responses with other ids, `brcmf_proto_bcdc_cmplt`); bit0 `BCDC_DCMD_ERROR` (0x01) on failure |
| 12 | `status` | 0 | 0, or negative BCME code when ERROR set (e.g. **-23 `BCME_UNSUPPORTED`**) |
| 16 | buffer | request data | GET: the answer, starting at offset 16 (overwrites the iovar name) |

Important details:
- Host TX is truncated to `BRCMF_TX_IOCTL_MAX_MSG_SIZE` = 1518 bytes
  (`brcmf_proto_bcdc_msg`) even when `len` is larger (e.g. `chanspecs` GET with 1536-byte
  buffer). **Use the `len` field, not the frame size, as the output buffer size.**
- GET: driver copies `min(rx_len_from_dcmd_start, len)` bytes from offset 16
  (`brcmf_proto_bcdc_query_dcmd`). Reply with ≥ `len` bytes of payload (zero-filled).
- SET: response payload ignored; send just the 16-byte header (or echo).
- Error mapping (`fwil.c:brcmf_fil_cmd_data`): ERROR flag with `status < 0` → -EBADE (or the
  raw fw error when `ifp->fwil_fwerr`, used by feature probes, §7.3).
- Response must arrive within 2.5 s (`DCMD_RESP_TIMEOUT`); otherwise -ETIMEDOUT.
- Iovar GET/SET both use ioctl 262 `GET_VAR` / 263 `SET_VAR`; buffer = `"name\0" + data`
  (`fwil.c:brcmf_create_iovar`). bsscfg-index 0 variants use the plain name.

### 7.2 Data/event channel BDC header (`bcdc.c:brcmf_proto_bcdc_hdrpull/hdrpush`)

4 bytes after the SDPCM header: `flags` (bits 7:4 = protocol version **2** → `0x20`; 0x04
SUM_GOOD; 0x08 SUM_NEEDED on TX), `priority` (bits 2:0), `flags2` (bits 3:0 = ifidx),
`data_offset` (in 4-byte words of extra TLV data before the payload; use 0). Host TX data
frames carry the same header. Packets with the wrong version are dropped.

### 7.3 Ioctls/iovars issued until `wlan0` exists — in order

Legend: **MUST** = must return success; *opt* = failure tolerated; *UNSUP* = should return
ERROR/-23 (recommended, avoids enabling optional features).

**Bus preinit** (`sdio.c:brcmf_sdio_bus_preinit`):

| # | request | rule / suggested answer |
|---|---|---|
| 1 | SET `bus:txglomalign` = u32 4 (SDIO core rev ≥ 12; rev < 12 sends `bus:txglom`=0) | **MUST** |
| 2 | GET `ulp_sdioctrl` (struct, 43012 ULP) | *opt*/UNSUP |
| 3 | SET `bus:rxglom` = 1 (only if host has sg support) | *opt* — **return UNSUP**: success enables `txglom`, adding an 8-byte HW-ext header to every host TX frame |

**Common preinit** (`common.c:brcmf_c_preinit_dcmds`):

| # | request | rule / answer |
|---|---|---|
| 4 | GET `cur_etheraddr` (6 B) — or SET if DT gave a MAC | **MUST**. Return a real MAC (not `00:90:4c:c5:12:38`, which triggers a random MAC + SET). |
| 5 | ioctl 98 `GET_REVINFO` (`struct brcmf_rev_info_le`, 17×u32) | *opt*. e.g. vendorid 0x14e4, deviceid 0x43ab, chipnum 0x4345, chiprev 6, corerev 42, bus 0 (SDIO). |
| 6 | SET `clmload` ×N (`brcmf_dload_data_le`: u16 flag, u16 type=2, u32 len, u32 crc, data ≤1400 B; flag DL_BEGIN 0x2 / DL_END 0x4, version 1<<12) — only if `.clm_blob` exists | **MUST** if issued (failure → GET `clmload_status`, then probe fails) |
| 7 | `txcapload` / cal blob | only if such files/settings exist; normally not issued |
| 8 | GET `ver` (256 B) | **MUST**. ASCII string **containing a space** (driver takes text after the last space as fw version; no space → silently skips steps 9–14!). e.g. `"wl0: Mar 18 2024 00:00:00 version 7.45.265 (28bca26 CY) FWID 01-b677b91b\n"` |
| 9 | GET `clmver` | *opt*; e.g. `"API: 12.2 Data: ... "` |
| 10 | SET `mpc` = 1 | **MUST** |
| 11 | SET `join_pref` (2×4 bytes) | *opt* |
| 12 | GET `event_msgs` (mask, `event_mask_len` bytes) | **MUST** (zeros fine) |
| 13 | SET `event_msgs` (mask, with E_IF=54 set) | **MUST** — store the mask; only send events whose bit is set |
| 14 | ioctl 185 `SET_SCAN_CHANNEL_TIME` (u32 40) | **MUST** |
| 15 | ioctl 187 `SET_SCAN_UNASSOC_TIME` (u32 40) | **MUST** |
| 16 | SET `txbf` = 1 | *opt* |

**Feature detection** (`feature.c:brcmf_feat_attach`):

| # | request | rule / answer |
|---|---|---|
| 17 | GET `cap` (768 B) | *opt*. Space-separated tokens; each matching token enables a feature. Minimal: `"sta wme 802.11d 802.11h dualband ampdu ampdu_tx ampdu_rx"` (avoid `p2p`, `mchan`, `mbss`, `mfp`, `sae`…, which pull in extra code paths). |
| 18 | `pfn_gscan_cfg` | skipped for chip 0x4345 |
| 19 | GET `pfn` | UNSUP |
| 20 | GET `wowl` (only if host PM caps allow wowl) | UNSUP |
| 21–24 | GET `rsdb_mode`, `tdls_enable`, `mfp`, `dump_obss` | UNSUP. **Any error other than -23 counts as "supported"** (`brcmf_feat_iovar_int_get` compares with `-BRCMF_FW_UNSUPPORTED`). |
| 25 | GET `pfn_macaddr` | UNSUP (success enables random-MAC scan) |
| 26–27 | GET `sup_wpa`, `scan_ver` | UNSUP (`scan_ver` success → v2 escan params layout) |
| 28 | GET `wlc_ver` | *opt*/UNSUP |
| 29 | vendor (`brcmfmac-wcc`) feat attach | needs module `brcmfmac-wcc` present in rootfs (`fwvid`) |

**Proto init done** (`fwsignal.c:brcmf_fws_attach`): SET `tlv` (*opt*, UNSUP recommended →
no TLV signalling), SET `ampdu_hostreorder` = 1 (*opt*), GET `wlfc_mode` (*opt*).

**cfg80211 attach** (`cfg80211.c:brcmf_cfg80211_attach` and callees):

| # | request | rule / answer |
|---|---|---|
| 30 | ioctl 1 `GET_VERSION` (u32) | **MUST**: `2` (`BRCMU_D11AC_IOTYPE`, chanspec encoding below) |
| 31 | ioctl 140 `GET_BANDLIST` (u32[]) | **MUST**: `[2, 1, 2]` (count, WLC_BAND_5G=1, WLC_BAND_2G=2) or `[1, 2]` for 2.4 GHz only |
| 32 | GET `vhtmode` | *opt* (0 keeps it simple; 1 → also `txstreams`, `txbf_bfe_cap`, `txbf_bfr_cap`, all *opt*) |
| 33 | GET `nmode` | *opt* (1 → HT caps, then 34) |
| 34 | GET `bw_cap` with u32 band argument (2 then 1); fallback GET `mimo_bw_cap` | *opt*; answer `1` (20 MHz only) → skips 40 MHz setup |
| 35 | GET `rxchain` | *opt* (1) |
| 36 | GET `chanspecs` (1536 B) | **MUST**: `u32 count` + `u32 chanspec[count]`. D11AC encoding: `0x1000 | ch` for 2.4 GHz 20 MHz (ch 1–13 → 0x1001…0x100D), `0xD000 | ch` for 5 GHz 20 MHz (`brcmu_d11.h`: BW_20 0x1000, BND_2G 0x0000, BND_5G 0xC000). |
| 37 | GET `per_chan_info` (u32 in = chanspec) per 20 MHz chanspec | **MUST** (at least for the last one — `brcmf_construct_chaninfo` returns the last `err`!). Answer 0 (no radar/passive). |
| 38 | `bw_cap` SET / `obss_coex` | only if HT40 2.4 GHz cap advertised |
| 39 | SET `event_msgs` (`fweh.c:brcmf_fweh_activate_events`) | **MUST** (twice: before p2p/btcoex/pno attach and again at the end) |
| 40 | `tdls_enable` SET | only if TDLS feature enabled |
| – | `p2p` attach | no I/O unless `p2pdev_forced` (module param `p2pon`) |

Then `core.c:brcmf_net_attach` → `register_netdev` → **wlan0 exists.**

### 7.4 Bring-up (`ip link set wlan0 up`, `core.c:brcmf_netdev_open`, `cfg80211.c:brcmf_config_dongle`)

| request | rule |
|---|---|
| GET `toe_ol` | *opt* |
| ioctl 2 `C_UP` | result ignored (answer success) |
| ioctl 185, 187, 258 `SET_SCAN_*_TIME` | **MUST** (`brcmf_dongle_scantime` returns the error) |
| ioctl 86 `SET_PM` | **MUST** |
| SET `bcn_timeout`, SET `roam_off`, ioctl 55 `SET_ROAM_TRIGGER`, ioctl 57 `SET_ROAM_DELTA` | **MUST** (`brcmf_dongle_roam`) |
| ioctl 20 `SET_INFRA` = 1 (`brcmf_cfg80211_change_iface`) | **MUST** |
| SET `arp_ol` / `arpoe` / `ndoe` (`core.c:brcmf_configure_arp_nd_offload`) | *opt* |
| ioctl 219 `SET_FAKEFRAG` = 1 | **MUST** |

### 7.5 Scan (`cfg80211.c:brcmf_cfg80211_scan`, `brcmf_do_escan`, `brcmf_run_escan`)

- SET `mpc` = 0 (*opt*), SET `vndr_ie` (bsscfg, when the request carries IEs — **MUST**),
  SET `escan` (`brcmf_escan_params_le`: u32 version=1, u16 action=1 START, u16 sync_id,
  `brcmf_scan_params_le`) — **MUST**.
- Then deliver events `BRCMF_E_ESCAN_RESULT` (69): zero or more with status 8 (PARTIAL)
  carrying `brcmf_escan_result_le` {u32 buflen, u32 version, u16 sync_id, u16 bss_count,
  `brcmf_bss_info_le`…}, and one final with status 0 (SUCCESS). Without the final event the
  driver times out after `BRCMF_ESCAN_TIMER_INTERVAL_MS` (10 s, `cfg80211.h`) and aborts the scan.

Event frame on SDPCM channel 1 (`fweh.h struct brcmf_event`, `brcmf_fweh_process_skb`):
```
SDPCM hdr (12) | BDC hdr 20 00 00 00 |
ethhdr: dst=MAC, src=MAC, type 0x886C (ETH_P_LINK_CTL)             (14)
brcm_ethhdr (BE): subtype 0x8001, length, version 0, oui 00 10 18, usr_subtype 0x0001 (10)
brcmf_event_msg_be (BE): u16 version 2, u16 flags, u32 event_type, u32 status, u32 reason,
   u32 auth_type, u32 datalen, u8 addr[6], char ifname[16] "wlan0", u8 ifidx 0, u8 bsscfgidx 0 (48)
event data (datalen bytes, LE structures)
```
Checks: OUI `00 10 18`, usr_subtype 1, `datalen ≤ 8192` and fits the packet; events without a
registered handler (other than E_IF) are dropped (`fweh.c:brcmf_fweh_process_event`).

---------------------------------------------------------------------------------------------

## 8. Hang/livelock hazards for a naive emulator

1. **F1/F2 `max_blksize` = 0** (missing/short CISTPL_FUNCE type 1, <42 bytes) →
   `sdio_max_byte_size` = 0 → infinite byte-mode loop in `sdio_io_rw_ext_helper`.
2. **CHIPCLKCSR read-back**: low 6 bits not stored → probe fails ("ChipClkCSR access").
   HT signalled with only 0x80 → `SBSDIO_HTAV` false (needs 0xC0).
3. **Delayed HT clock + idle**: the watchdog (`sdio.c:brcmf_sdio_bus_watchdog`, every
   `BRCMF_WD_POLL` = 10 ms, `idletime` = 1) drops the clock (`CHIPCLKCSR ← 0`) after ~2
   idle ticks; every DPC run re-requests it with `pendok = true`
   (`brcmf_sdio_bus_sleep(false, true)`). If the emulator does not report 0xC0 immediately,
   the driver sets `DEVICE_CTL.CA_INT_ONLY` and waits for a card interrupt that the emulator
   must then raise when the clock comes up — otherwise ioctls stall forever. Make clocks
   instant.
4. **`I_HMB_HOST_INT` with zero mailbox data**: `brcmf_sdio_ulp_pre_redownload_check` then
   reads CCCR IOEx; if it reads exactly `0x02` it assumes 43012 ULP deep-sleep and starts
   SHM polling / firmware re-download. Only raise HOST_INT with non-zero
   `tohostmailboxdata`; never set `HMB_DATA_FWHALT` (0x10).
5. **TX credit**: never answering (or answering with `max seq` not ahead of the host's
   `tx_seq`) blocks all further control frames (`txctl_ok`) → every ioctl times out after
   2.5 s, then ctrl frame timeout. Window ≤ 0x40 ahead.
6. **IORx bit2** must stay set while IOE2 is set: checked before *every* control TX
   (`brcmf_sdio_f2_ready`), not just at enable.
7. **Interrupt semantics**: intstatus must be W1C and the card IRQ level-triggered while
   `intstatus & hostintmask` is non-zero; FRAME_IND must be (re)asserted whenever a frame is
   queued. An empty RX FIFO must read as zeros, not repeat the last frame.
8. **Request-id echo**: BCDC response flags[31:16] must equal the request's id, otherwise
   `brcmf_proto_bcdc_cmplt` keeps waiting (each wait up to 2.5 s).
9. **SMB_NAK** (tosbmailbox bit0) after a failed RX: driver sets `rxskip` and ignores
   FRAME_IND until the dongle reports `HMB_DATA_NAKHANDLED` via HOST_INT — emulator must
   re-queue the frame and post mailbox `0x00040001` + HOST_INT, or RX stops permanently.
   Avoid by never producing malformed frames.
10. **`RFRAMEBC` registers** must read 0 after `RF_TERM` (polled up to 65535 times).
11. **Backplane window reset value** must be 0; window bytes take effect per write.
12. **KSO / save-restore**: only active if `brcmf_chip_sr_capable` returns true (4345:
    pmurev ≥ 17 and PMU chipcontrol[3] bit2). Then the driver switches to
    `brcmf_sdio_kso_control` (SLEEPCSR write/poll for 0x03, up to
    `MAX_KSO_ATTEMPTS` = 20000 × 50 µs ≈ 1 s per wake) and CMD14 vendor bits. Keep SR off
    (return 0 from chipcontrol data); SLEEPCSR then only sees the single init write.
13. **Polling bounds**: CMD5 ready ≤100×10 ms; ALP/HT ≤1 s; F1 enable ≤ FUNCE timeout;
    F2 enable ≤3 s; resetctrl spin 300 µs / 50 tries; ioctl reply 2.5 s.
14. **`ver` without a space** → preinit silently skips `mpc`, `event_msgs`, scan times
    (no error, but events never enabled → scans never complete).
15. **`per_chan_info` / `chanspecs` failing** → `brcmf_setup_wiphybands` error → no wlan0.

---------------------------------------------------------------------------------------------

## 9. Optional/tolerated probes (summary)

Tolerated failures: `ulp_sdioctrl`, `bus:rxglom`, `GET_REVINFO`, `clmver`, `join_pref`,
`txbf`, `cap`, all feature-probe GETs (`pfn`, `wowl`, `rsdb_mode`, `tdls_enable`, `mfp`,
`dump_obss`, `pfn_macaddr`, `sup_wpa`, `scan_ver`, `wlc_ver`), `tlv`, `ampdu_hostreorder`,
`wlfc_mode`, `vhtmode`, `nmode`, `bw_cap`, `mimo_bw_cap`, `rxchain`, `txstreams`,
`txbf_bfe_cap`, `txbf_bfr_cap`, `obss_coex`, `toe_ol`, `C_UP`, `arp_ol`, `arpoe`, `ndoe`,
first `mpc 0` in scan. Not needed at all: sdpcm_shared/console (non-DEBUG), FWREADY mailbox,
OOB IRQ, SR/KSO, glomming, flow-control TLVs.

Recommended generic policy for the ioctl server: implement the MUST list explicitly; reply to
every unknown GET/SET with ERROR + status −23 (BCME_UNSUPPORTED).
