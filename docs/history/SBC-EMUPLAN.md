# SBC-EMU implementation plan for coding agents

> **Historical record.** This is the project's first plan, written on 2026-11-13 before any
> code. It proposed building on QEMU; sbc-emu instead has its own CPU cores and device models
> ([decisions.md](../decisions.md), D1), so its binding rules 1 and 7, its scope for v0.1 and much
> of its structure no longer apply. It is kept as a record of where the project started. For what
> sbc-emu is and does today, see the [README](../../README.md) and
> [how the boards share code](../architecture.md).

**Status:** proposed implementation specification; no SBC-EMU implementation or successful board test is implied by this document.  
**Date:** 2026-11-13. **Repository / executable:** `sbc-emu`.  
**Mission:** Real boards. Virtual hardware. A standalone, local-first, open-source SBC emulator that complements esp32sim and Cooja-NG.

## 1. Decisions and scope

The proposed design: QEMU CPU/SoC emulation, C device models, Rust orchestration, declarative boards, real Linux interfaces, independent executables connected through versioned protocols. Preserve the priority order: Raspberry Pi 4B and 3B; then Pi 5 and Zero 2 W; Compute Modules; Orange Pi and Radxa. This is an engineering priority, not a claim about measured sales rankings.

Build a useful headless tool before attempting complete silicon fidelity. The first public MVP must boot tested Linux images on Pi 4B and 3B, provide storage and W0 networking, and demonstrate GPIO, I²C and SPI through normal guest Linux interfaces. Integration contracts start early; exact cross-emulator time coordination is a separate gate.

### Binding implementation rules

1. Reuse QEMU; do not implement another ARM CPU interpreter or translate Pi drivers into host APIs.
2. Keep SBC-EMU usable without esp32sim, Cooja-NG, a cloud service or a web UI.
3. Do not label an ARM `virt` machine as an emulated Raspberry Pi.
4. Separate application compatibility, board fidelity, network behavior and timing fidelity in every report.
5. Do not claim stock-image compatibility from a custom kernel boot, onboard Ethernet compatibility from USB Ethernet, or Wi-Fi compatibility from a renamed Ethernet interface.
6. Pin dependencies and verify capabilities at the selected revisions. Moving upstream documentation is discovery evidence, not a release support guarantee.
7. Keep QEMU modifications small, testable and suitable for upstream submission. Do not maintain duplicate canonical copies of device code.
8. Research milestones cannot block W0 networking or the initial release.

### Out of scope for v0.1

Cycle-accurate CPUs, GPU/NPU acceleration, camera ISP, video codecs, complete VideoCore firmware execution, unrestricted vendor-image compatibility, full Pi 5/RK3588 support, Bluetooth RF fidelity, W2/W3 Wi-Fi, production-grade guest isolation guarantees, and browser-hosted QEMU. Pico W/Pico 2 W belong to a future MCU adapter, not the initial SBC backend.

## 2. Evidence baseline and unresolved facts

The following observations were checked against primary sources when preparing this plan. Implementation must replace moving URLs with exact revisions in `docs/evidence/upstream-baseline.md`.

- QEMU lists `raspi3b` and `raspi4b`; its current Pi documentation describes a four-core, 1 GiB Pi 3B and a four-core, 2 GiB Pi 4B. GPIO, SD/MMC, UART, SPI and I²C models are listed; Pi 4B also lists GENET and PCIe. PWM and Pi 4 PCIe MSI/MSI-X delivery are documented gaps. These listings do not prove that a selected guest driver passes. [QEMU Raspberry Pi models](https://github.com/qemu/qemu/blob/master/docs/system/arm/raspi.rst)
- QEMU recommends `virt` for generic Arm Linux. Board-specific kernels/images require compatible machines; userspace compatibility is a different property. No ready Pi 5 or RK3588 board target is established by the reviewed machine list. [QEMU Arm targets](https://www.qemu.org/docs/master/system/target-arm)
- Orange Pi PC has an upstream H3 machine with serial, SD and Ethernet among its devices. This is a useful later cross-vendor baseline, not proof of H618 or RK3588 support. [QEMU Orange Pi PC](https://www.qemu.org/docs/master/system/arm/orangepi.html)
- `mac80211_hwsim` supplies software Wi-Fi radios for Linux testing; it is a candidate for W1, not an emulation of the Pi's original radio. [Linux hwsim documentation](https://wireless.docs.kernel.org/en/latest/en/users/drivers/mac80211_hwsim.html)
- `brcmfmac` is a FullMAC driver with SDIO support. Its host-facing protocol is a distinct research surface. [Linux Broadcom driver documentation](https://wireless.docs.kernel.org/en/latest/en/users/drivers/brcm80211.html)
- QEMU instruction counting is not cycle accuracy and is incompatible with multithreaded TCG. It does not supply a ready distributed simulator scheduler. [QEMU instruction counting](https://www.qemu.org/docs/master/devel/tcg-icount.html)
- esp32sim's ESP32-C6 documentation describes Cooja integration through NDJSON `hello`, `step {t,in}`, `stop` and `done {t,wake,out}`, with nanosecond time. Treat this as a compatibility lead; inspect both implementations before freezing an adapter. [esp32sim C6 integration](https://github.com/mikroverk/esp32sim/blob/main/docs/esp32c6.md), [Cooja-NG repository](https://github.com/mikroverk/cooja-ng)

No QEMU, kernel, image, Cooja-NG or esp32sim commit is invented here. Issue S00 must select and record actual commits, licenses, build hashes and protocol fixtures. The Cooja external-nodes design file was not retrieved during this review; exact handshake fields and current coordinator behavior remain to be inspected in S02.

## 3. Execution modes and truthful capabilities

Model execution as two independent axes: **machine backend** (`application` or `hardware`) and **experiment mode** (`standalone` or `network`). Expose three user-facing entry points while retaining these axes internally.

| User mode | Backend and guest | Appropriate claim | Limitations |
|---|---|---|---|
| Application | QEMU `virt`, tested ARM64 Linux, virtio devices; optional acceleration when compatible | ARM Linux services, containers, MQTT/HTTP, portable libraries | No Pi MMIO/driver fidelity; container host-kernel dependencies still apply |
| Hardware | Actual selected QEMU board plus explicit extensions, tested board DTB/kernel | Listed board drivers and peripheral behavior | Direct kernel boot does not reproduce ROM/EEPROM/firmware boot chain |
| Network simulation | Multiple application/hardware nodes with explicit link and clock policies | Tested network scenarios at declared abstraction | Real-time mode is not deterministic; simulated Ethernet is not 802.11 RF |

CLI examples below are **interfaces to implement**, not commands claimed to exist today:

```sh
sbc-emu boards list --json
sbc-emu doctor --board pi4b
sbc-emu run scenarios/pi4-basic.yaml
sbc-emu run --mode application --profile arm64-dev
sbc-emu topology run scenarios/mixed-iot.yaml
sbc-emu test --board pi4b --suite peripherals --report-dir artifacts/pi4b
sbc-emu capabilities --board pi4b --image images/pi4-minimal.lock.json
```

Use compatibility labels B0–B4 only with an accompanying per-feature manifest:

- **B0:** kernel reaches a functioning init and answers a nonce challenge.
- **B1:** B0 plus console, timers/interrupts, storage and reboot tests.
- **B2:** B1 plus a named network backend and bidirectional TCP/UDP tests.
- **B3:** B2 plus the release's declared GPIO/I²C/SPI peripheral suite.
- **B4:** B3 plus an explicitly named W1 or W2 wireless suite. W0 alone never earns B4.

Also report `boot_path`, `vendor_image_compatibility`, `original_driver_compatibility`, `wifi_tier`, `clock_mode`, `accelerator`, `tested_cpu_count`, and unsupported features. Feature states are `unverified`, `experimental`, `tested`, `unsupported`, `not_applicable`. A level cannot hide a failing feature.

## 4. Board capability and delivery matrix

This is a **planning matrix**, not a claim of implemented SBC-EMU support. Every SBC-EMU row starts unverified. Freeze exact board revisions, RAM and wireless modules in profiles; never treat a product name as a complete hardware specification.

| Priority / target | QEMU starting point | First evidence required | Network/peripheral route | Major gaps / delivery gate |
|---|---|---|---|---|
| P0 Pi 4B | `raspi4b`, BCM2711-family model | Pinned direct Linux boot, serial, SD, 2 GiB profile | GENET after driver test; BCM GPIO/I²C/SPI; W0 | Validate interrupt/DMA behavior; W2 SDIO separate; v0.1 gate |
| P0 Pi 3B | `raspi3b`, BCM2837-family model | Independent DTB/kernel and 1 GiB boot tests | Explicit USB Ethernet fallback if onboard path fails; BCM peripherals | Onboard USB Ethernet identity/fidelity needs audit; not a Pi 3B+ alias |
| P1 Pi 5 | New/extended BCM2712 + RP1 target unless upstream changes | First serial, then init with reduced DT | RP1 PCIe/peripheral work; W0 later | New SoC, interrupt, PCIe and RP1 bring-up; no deadline promise |
| P1 Zero 2 W | Related Pi 3 components, dedicated board work | Correct RAM, wiring, DT and SD boot | USB/virtual W0 explicitly declared; W2 later | Do not relabel `raspi3b`; validate board identity and 512 MiB profile |
| P2 CM4 + named carrier | Reuse validated Pi 4 SoC work | Explicit carrier, eMMC or Lite SD, pin mapping | Carrier Ethernet/peripherals; optional wireless profile | Module/carrier combinations are separate test cases |
| P2 CM5 + named carrier | Depends on Pi 5 work | Module/carrier boot and storage tests | Reuse RP1 only where the selected hardware actually has it | Audit module/carrier topology instead of cloning Pi 5 |
| P2 CM3/CM3+ (optional) | Related Pi 3 components | Own module/carrier test | Declared storage/peripheral profile | Schedule only on demand after core targets |
| P2 Orange Pi PC | `orangepi-pc`, Allwinner H3, 32-bit Arm | UART, init, SD, EMAC | Own Allwinner pin/bus mappings; W0 | First non-Broadcom release gate |
| P2 Orange Pi Zero 3 | New H618 work | Architecture/boot feasibility before model | New SoC-specific controller audit | Not an H3 configuration change |
| P2 Orange Pi 5 | New RK3588S-family work | Minimal serial/init milestone | New clocks, reset, interrupt and storage models | GPU/NPU excluded; substantial SoC work |
| P2 Radxa ROCK 5B | Shared RK3588-family components where valid | Independent board DT and boot proof | Exact optional wireless module profile | RK3588 versus RK3588S differences; no implicit onboard Wi-Fi |

Pi 5 remains the next flagship target, but a bounded Orange Pi PC portability spike may run after the Pi MVP without delaying Pi 5 feasibility. Generic ARM64 application profiles can serve users of unsupported boards immediately, with conspicuous non-hardware labeling.

For each board, `capabilities.json` must enumerate CPU/MMU, IRQ, clocks/reset, UART, SD/eMMC, Ethernet, GPIO/pinctrl, I²C, SPI, PWM, USB, PCIe, Wi-Fi, Bluetooth, display, camera and accelerators. Attach test IDs and hashes rather than a blanket “supported”.

## 5. Architecture and ownership boundaries

```text
CLI / optional local web UI / automation clients
                   |
          Rust control plane
  profiles | lifecycle | policy | artifact store
                   |
        versioned internal interfaces
       /              |                \
QEMU adapter      network service     device environment
 QMP/control      switch/AP models    sensor values/faults
 serial streams   capture/impairment  pin stimuli/observation
       |              |                |
       +------ QEMU process per node ---+
               CPU + SoC + C device models
                          |
                Linux guest and drivers
                          |
          optional restricted test guest agent

Cooja-NG / esp32sim <-> external adapters <-> network/time services
```

### Components

| Component | Responsibility | Must not own |
|---|---|---|
| `sbc-core` | Run state machine, policy, event IDs, cancellation, immutable manifests | Device register semantics |
| `sbc-cli` | Commands, diagnostics, stable machine-readable outputs | Alternate hidden execution paths |
| `sbc-boards` | Profile validation, revision/RAM constraints, boot recipes, capability selection | Claims inferred only from board names |
| `sbc-qemu` | Discovery, process launch, QMP, serial, device transport, capability probes | Shell-built command strings |
| `sbc-peripherals` | Environment values, wiring, scenario stimuli and shared schemas | A replacement for original guest controller drivers |
| `sbc-network` | Isolated links, switching/routing boundaries, queueing, seeded impairments, capture | CPU execution control |
| `sbc-radio` | W1/W2 common AP state and radio-model interfaces | Assumption that all tiers exchange identical frames |
| `sbc-clock` | Time contracts, ordering, grant protocol, replay metadata | Claiming physical CPU-cycle fidelity |
| `sbc-cooja`, `sbc-esp32` | Protocol translation, peer discovery and compatibility tests | Vendoring another simulator's core |
| `sbc-test` | Host assertions, guest test orchestration, result manifests | Success from a single boot-log substring |
| QEMU C patches | MMIO, buses, IRQ, DMA, reset, guest-facing device state | Blocking network RPC inside an MMIO callback |
| `sbc-web` (later) | Console, board view, sensor control, topology and traces | Required boot logic or new protocol semantics |

Use QMP for lifecycle and introspection. QMP `stop`/`cont` is not an exact “run to nanosecond” API. Use QEMU character backends for byte streams and explicit network backends for packets. New device-control commands must use namespaced QAPI definitions or a documented bounded device transport; record them in backend capabilities.

Register-accurate peripheral state belongs inside QEMU. Host services send timestamped environment changes; QEMU queues them on its owning event loop and applies them at virtual-time boundaries. Define queue size, overflow behavior and disconnect handling. Initially keep sensor register models in C with shared language-neutral fixtures; share schemas and tests with esp32sim before considering a common runtime library or FFI.

Lifecycle: `created -> validated -> starting -> running <-> paused -> stopping -> exited`, with `failed` reachable from every active state. Start is transactional: reserve resources, create overlay, launch, negotiate, then mark running. On failure terminate owned children, close sockets, retain diagnostics and release only owned resources. Stop is idempotent, with graceful timeout followed by termination and final forced cleanup.

## 6. Repository layout

```text
sbc-emu/
  README.md  SBC-EMUPLAN.md  AGENTS.md  CONTRIBUTING.md
  ARCHITECTURE.md  COMPATIBILITY.md  SECURITY.md  LICENSE
  Cargo.toml  Cargo.lock  rust-toolchain.toml  Makefile
  crates/
    sbc-core/ sbc-cli/ sbc-boards/ sbc-qemu/ sbc-peripherals/
    sbc-network/ sbc-radio/ sbc-clock/ sbc-test/ sbc-cooja/ sbc-esp32/
  qemu/
    upstream.lock.json       # URL, commit, source hash, build recipe
    patches/series           # ordered canonical patch series
    patches/*.patch          # device changes and their QTests
  boards/
    application/arm64-dev.toml
    raspberrypi/{pi4b,pi3b,pi5,zero2w,cm4,cm5}.toml
    orangepi/{pc,zero3,pi5}.toml
    radxa/rock5b.toml
    carriers/                # named module carriers and wiring
  schemas/                   # profiles, scenarios, APIs, events, reports
  images/
    recipes/                 # pinned Linux + Buildroot recipes/configs
    locks/                   # hashes, provenance, licenses
    overlays/                # init, device tests, restricted guest agent
  peripherals/
    bme280/ mcp3008/ ssd1306/ # specs, fixtures and reference traces
  scenarios/
    pi4-basic.yaml pi3-basic.yaml pi4-sensors.yaml
    two-pi-loss.yaml mixed-iot.yaml
  adapters/fixtures/{cooja,esp32}/
  tests/{unit,contract,boot,storage,network,peripherals,wifi,integration}/
  scripts/                   # hermetic builds, image verification, CI entry points
  tools/                     # developer diagnostics and evidence generation
  web/                       # optional later TypeScript frontend
  docs/{adr,evidence,protocols,boards,devices,development}/
  .github/workflows/
  build/                     # ignored generated trees and binaries
  cache/                     # ignored verified downloads
  runs/                      # ignored per-run overlays and sockets
  artifacts/                 # ignored reports, logs and captures
```

Create only components needed by the current milestone; empty crate proliferation is not progress. Profile files for future boards must be marked unsupported and must fail clearly if selected. Generated QEMU checkout lives in `build/qemu-src`; patches are the source of truth until accepted upstream.

## 7. Profiles, images and reproducible boot

Board TOML is static hardware; scenario YAML is topology and stimuli; image lock JSON is software provenance. Schemas reject unknown required fields, duplicate node IDs, ambiguous pin names and incompatible combinations before launching a process.

Illustrative scenario to implement:

```yaml
schema_version: 1
id: pi4-basic
seed: 42
clock: { mode: realtime }
nodes:
  - id: pi1
    mode: hardware
    board: pi4b
    image_lock: images/locks/pi4-minimal.json
    storage: { writable: overlay, retain: on_failure }
    network: { backend: isolated, link: lan0, wifi_tier: W0 }
links:
  - id: lan0
    kind: ethernet
    egress: deny
    services: [dhcp, http-fixture]
assertions:
  - { type: guest_ready, node: pi1, timeout_wall_s: 180 }
  - { type: http_roundtrip, node: pi1, fixture: http-fixture }
```

Image recipes must pin Linux, Buildroot/toolchain, patches, config fragments, DTB, rootfs and package sources. Record architecture, kernel command line, boot protocol, console device, disk partition UUIDs, init path, modules and checksums. The launcher must not guess `/dev/mmcblk0p2` for arbitrary images.

Start with direct kernel + DTB + initramfs boot for rapid diagnosis; add a small ext4 SD root image for B1. Build a 64-bit Pi fixture and an independent generic `virt` fixture. Pi 3 and Pi 4 can share userspace but must have independently tested boot manifests. Disable unmodeled devices in the test DT only when documented; store the diff and never describe it as an unmodified vendor DT.

Treat Raspberry Pi OS as a later compatibility suite: exact image date/hash, original versus substituted kernel/DT/firmware, extraction recipe, firmware license and observed failures. A guest userland extracted from a vendor image running under a different kernel earns only that narrower claim.

Use immutable bases and per-run copy-on-write storage. Probe whether the selected SD backend accepts the intended image format; test qcow2 explicitly. If required, use a private reflink/copy of raw media and report that choice. Never modify the input image. Reject backing chains outside approved paths and prohibit raw host-block-device access by default.

## 8. Build and test command contract

These commands define deliverables for S00–S08; they are not evidence that a repository or build currently exists. Implement `make help` first and keep commands consistent with CI. Use Linux x86_64 as initial reference host; add Linux arm64 and macOS smoke coverage after the first slice. Linux image builds on macOS use a pinned build container or documented Linux builder. TCG is the portability baseline; KVM/HVF acceleration is opt-in for compatible application profiles, not assumed for Pi machines.

```sh
# From the future repository root; downloads are explicit and hash-checked.
make doctor
make bootstrap                 # verify tools; no implicit system-wide installs
make fetch                     # populate cache from locked sources
make qemu                      # apply patches and build pinned QEMU
make images                    # build minimal Pi and virt fixtures
cargo build --locked --workspace

# First usable path.
cargo run --locked -p sbc-cli -- doctor --board pi4b
cargo run --locked -p sbc-cli -- run scenarios/pi4-basic.yaml
make test-slice                 # automated Pi 4 boot/network/GPIO proof

# Routine validation.
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
make test-contract
make test-qemu                 # selected QTests via pinned QEMU build
make test-boot BOARDS=pi4b,pi3b
make test-peripherals BOARD=pi4b
make test-network
make test-security
make report                    # summarize evidence; never turn skips into passes
```

`make doctor` checks Rust/toolchain pins, compiler, Python, Ninja/Meson as required by the selected QEMU revision, GLib/pixman dependencies as configured, cross toolchain, disk space and available RAM. List missing dependencies without silently installing packages.

An illustrative backend build shape, finalized by S00:

```sh
mkdir -p build/qemu
cd build/qemu
../qemu-src/configure --target-list=aarch64-softmmu,arm-softmmu --disable-werror
ninja
```

The project must record actual configure options and dependency versions. Do not disable warnings in project code; `--disable-werror` above is only a possible upstream build portability choice. QEMU changes still pass upstream style checks and selected tests. Support libslirp only when built and advertised; do not assume every distribution QEMU has it.

`make fetch` is the only network-dependent preparation step where practical. Cached builds and default tests run offline. Fail on missing hashes, placeholder pins or incomplete fixtures. Never use a moving `latest` artifact in acceptance tests.

## 9. First executable vertical slice

**Deliverable:** one command boots a Pi 4B fixture, proves guest execution, performs a bidirectional network exchange, toggles a virtual GPIO through the real Linux GPIO interface, and exits with structured evidence. Target one board and one LED before a general UI or plugin framework.

### Slice A: baseline executable (S00–S05)

1. Pin and build QEMU; inventory `-machine help`, version/build options and relevant devices.
2. Build the minimal Pi 4 kernel/DT/initramfs. Boot headlessly with TCG, explicit serial and local QMP sockets, no display and no host shares.
3. Guest init sends `READY` plus boot ID; the test harness sends a random nonce over a dedicated test channel and requires a computed response. A static boot-log marker is insufficient.
4. Attach a small SD image; guest writes a nonce file, syncs, reboots, reads it back. Base-image hash must remain unchanged.
5. Validate GENET with the guest driver and a local HTTP fixture. If blocked, document the exact failure and demonstrate a named USB Ethernet or application-mode fallback separately. The Pi native-network acceptance item stays open.
6. Send a request and receive a response containing the nonce; capture counters and PCAP where available. Internet ping is not the network oracle.
7. Request shutdown, enforce a wall-time deadline, collect exit status, and prove no owned process/socket remains.

### Slice B: the first board-peripheral proof (S06)

1. Audit BCM GPIO model and Linux pinctrl/IRQ behavior; add the smallest QEMU patch needed to observe an output and inject an input.
2. Wire a named logical LED to BCM GPIO17 / header pin 11 in the Pi 4 profile. Pin numbering must be explicit.
3. Guest uses libgpiod or GPIO character-device v2 ioctls to set GPIO17 high, then low. Select the controller by verified metadata, not an assumed gpiochip index.
4. Capture host LED transitions with source pin, value, sequence and virtual timestamp. Compare against guest operations.
5. Inject a button edge on a second configured input; guest waits for the corresponding edge event. Include wrong-direction and reset tests.
6. `make test-slice` verifies all assertions and writes `result.json`, `run-manifest.json`, `serial.log`, `qmp.ndjson`, `events.ndjson`, disk hashes and packet capture.

**Slice pass criteria:** three consecutive clean runs on the reference host; each completes within a configurable 180-second boot watchdog and 300-second overall watchdog initially; correct nonce/network/storage/GPIO assertions; no base-image modification; no leaked child; explicit backend and feature declarations. Tune performance budgets only with recorded host details. Timing thresholds are host watchdogs, not simulated-hardware performance claims.

**If blocked:** after two focused working days on an upstream boot/network problem, produce a minimal reproducer and an evidence-backed issue. A Pi 3 or `virt` smoke path may keep infrastructure moving, but it does not close the Pi 4 slice. No fabricated successful output and no replacement of guest GPIO by a host-only LED animation.

## 10. Device emulation and guest/host interfaces

### Device model contract

Every model specifies: register map and widths; reserved/read-only bits; reset values; endianness; transaction ordering; IRQ assertion/deassertion; DMA bounds; clock/timer behavior; power/reset state; unsupported commands; save/load state or an explicit snapshot prohibition. Validate access widths and integer arithmetic. Implement documented behavior needed by the tested driver, then expand through test cases.

| Device | First implementation | Guest interface | Acceptance and negative cases |
|---|---|---|---|
| GPIO | BCM direction/value, external input, edge IRQ, declared pull behavior | GPIO character device/libgpiod | LED loop, button IRQ, invalid line, pin mux, reset |
| I²C BME280 | Chip ID, calibration registers, mode/control, deterministic raw sample and conversion timing | Kernel sensor driver/IIO where available; `i2c-dev` for diagnostics | Driver binds, expected compensated values within tolerance, NACK, repeated start, reset |
| SPI MCP3008 | Command decode, CS framing, deterministic 10-bit ADC sample | Kernel IIO driver; controlled diagnostic `spidev` binding | Channels 0/7, min/max values, malformed command, premature CS release |
| UART | Console plus optional linked serial device | `/dev/tty*`, termios | Bidirectional bytes, FIFO/IRQ where modeled, disconnect and backpressure |
| SD/eMMC | Existing controller plus verified writable medium | Normal block/filesystem stack | Persistence, full disk, read-only media, reset during I/O |
| Ethernet | Selected native/USB/virtio NIC with declared fidelity | Normal sockets and NIC driver | DHCP/static addressing, TCP/UDP, MTU, reset, link down |
| PWM | Separate missing-controller work; virtual waveform first | Linux PWM interface | Period/duty/enable, range rejection, reset; no analog claim |
| SSD1306 | Later I²C/SPI register/display model | Normal driver or userspace bus library | Known framebuffer pattern; headless golden image |
| USB | Existing supported QEMU device/controller combinations | Normal USB drivers | Enumerate, hotplug, bulk transfer and teardown |
| Bluetooth | Later functional HCI peer, then specific controller path | BlueZ/HCI | Explicit transport, discovery/connect tests; no RF timing claim |
| DS18B20 | Optional later 1-Wire device | Linux 1-Wire subsystem | Timing/reset/presence tests; not an I²C device |

For BME280 and MCP3008, derive fixtures from the exact published datasheets and tested Linux drivers during implementation; document supported subsets. Avoid implementing a fake register set simply matching one test application.

### Separate three interfaces

1. **Guest application interface:** standard Linux drivers, GPIO, IIO, sockets, I²C/SPI and serial. Hardware mode must not require rewriting applications to an SBC-only device SDK.
2. **Optional guest test agent:** restricted readiness, file-hash, test invocation and shutdown commands. Use virtio-serial on application machines when supported; use a verified secondary serial channel or dedicated network endpoint on board machines. Do not assume virtio exists on `raspi4b`. Keep console and protocol framing separate.
3. **Host control interface:** versioned local API for launch/status/stop, sensor values, pin stimulus, trace subscriptions and scenario control. The agent is test convenience, not a substitute for hardware emulation.

Start with a Unix socket and JSON request/response plus NDJSON event streams. Add optional HTTP `/v1/runs`, `/v1/runs/{id}`, `/v1/runs/{id}/events`, `/v1/runs/{id}/devices/{device}/stimuli` later using the same schemas. Loopback HTTP requires a per-session token and origin checks; remote listening requires explicit configuration and authenticated transport.

Requests have `api_version`, `request_id`, operation and typed arguments. Responses carry the same ID and either a result or stable error code. Events have `run_id`, `node_id`, `seq`, `time_domain`, `time_ns`, `kind` and payload. Encode nanosecond counters as decimal strings in the SBC JSON API to avoid JavaScript precision loss. Validate range and monotonicity before conversion to internal u64.

Initial limits: 1 MiB per control message; 64 KiB decoded packet payload maximum plus stricter medium MTU; bounded 4096-event queues; 10-second control timeout configurable per operation. Full queues cause an explicit error or recorded drop according to policy, never unbounded memory growth. Long-running operations return a handle. No arbitrary shell execution in default guest or host API.

## 11. Networking and Wi-Fi tiers W0–W3

### W0 — ordinary IP connectivity

Deliver first. Use native Ethernet where proven; otherwise a declared USB/virtio adapter appropriate to the machine. Support isolated local networks and local services by default. Offer user-mode NAT as explicit egress mode; host port forwards bind to `127.0.0.1` with collision checks. TAP is optional, separately provisioned, and never requires running the entire emulator as root.

A renamed interface is still W0. Test TCP and UDP request/response and packet delivery; QEMU user networking has ICMP limitations, so public ping must not be the acceptance test. [QEMU network backends](https://www.qemu.org/docs/master/system/devices/net.html)

Implement per-link delay, loss, bandwidth and bounded queues with a deterministic PRNG stream per link/direction. Record each draw or reproducible input sequence. A seed alone does not make a real-time multi-VM run deterministic.

### W1 — Linux Wi-Fi API behavior

Goal: real `nl80211` scan/association behavior using a compatible guest kernel, `mac80211_hwsim`, hostapd and wpa_supplicant. Begin with station and AP radios in one controlled Linux test environment. Tests require scan, association, DHCP, bidirectional data, disconnect and reconnect; wrong credentials must fail.

For multiple guests, build an explicit hwsim frame relay using the supported generic-netlink mechanism and a guest transport. Radios in separate VMs do not discover one another automatically. Test frame delivery, TX status, ACK handling, channel isolation and duplicate prevention before multi-node claims. Avoid a second in-kernel delivery path that duplicates externally relayed frames.

The guest kernel and relay are modified/configured for W1. This does not prove original Broadcom firmware or brcmfmac support. Record which authentication modes are actually exercised; start with one open test AP and one WPA2-Personal fixture. Do not claim WPA3, roaming, DFS, power save or RF accuracy until independently tested.

### W2 — original brcmfmac / SDIO compatibility

Goal: an unmodified, pinned Linux brcmfmac driver talks to an emulated SDIO FullMAC device. Begin with one exact Pi 4 radio/chip revision (candidate CYW43455-family profile, identity to be verified from the selected board/DT/driver). Pi 3 and other modules need separate device identities and tests.

The initial approach emulates host-visible firmware behavior, not the proprietary radio CPU. Record downloaded firmware/NVRAM requests and their permitted handling; never pretend a proprietary firmware blob is executed if it is merely accepted by the model.

Required work, in dependency order:

1. Audit the selected QEMU SD host and SD bus API for **SDIO**, not just SD storage. Prove CMD5, CMD52/CMD53, CCCR/FBR/CIS, function enable/block sizes, interrupt routing and reset behavior. Extend host and bus where necessary.
2. Implement chip identity, backplane/register-window access, RAM/download path, clock/power handshakes, firmware-ready transition and timeout paths demanded by the pinned driver.
3. Implement the subset of SDPCM framing, control/data/event channels, sequence/credit flow control, BCDC control requests/iovars and BDC data framing actually used by that driver. Confirm names and formats against pinned source, not this summary alone.
4. Support interface creation, event masks, scan result/events, join/disassociation, link-state changes and data TX/RX.
5. Connect the FullMAC model to the same AP/network service used by W1 through a tier-specific adapter. FullMAC Ethernet payloads cannot be mislabeled as complete over-the-air 802.11 frames.
6. Add one authentication/key path, multicast, packet filters, power-save and roam features only after separate positive and negative tests.

Gates: SDIO enumeration -> firmware handshake -> `wlan0` creation -> scan -> association -> DHCP -> bidirectional traffic -> reset/reconnect -> security tests. Each gate captures driver logs and protocol traces. “wlan0 exists” is not W2 success.

**Risk and stop rule:** timebox enumeration/firmware feasibility to four engineer-weeks initially. If no reproducible probe path emerges, publish the missing protocol/controller evidence and continue shipping W0/W1. Budget later work only after a reviewed feasibility report. Do not add magic guest-memory patches or suppress driver errors to force success. Proprietary blob redistribution and reverse-engineering provenance require explicit review before packaging.

### W3 — modeled wireless medium

Goal: declared 802.11 MAC/PHY abstractions, contention/backoff, retry/ACK behavior, channel occupancy, interference, propagation and packet error models. W3 is a modeling dimension, not automatically achieved by W2; W1 may feed a W3 medium earlier. Report both driver tier and medium-model fidelity.

Specify model assumptions and validate analytic cases and reference traces. Include hidden-node, interference, channel isolation and repeatability tests. A FullMAC model must synthesize and document MAC behavior hidden from the driver; packet loss on an Ethernet bridge is insufficient. Cooja's 802.15.4 capabilities do not supply an 802.11 model by implication. Keep calibrated real-world fidelity out of claims until measurement-backed validation exists.

## 12. Cooja-NG and esp32sim integration API

### Compatibility strategy

Pin peer repositories and inspect their actual external-node implementations in S02. Maintain golden wire transcripts for every supported peer revision. Prefer thin process adapters over linking simulator internals. Do not force peers to adopt SBC-specific envelopes or time-string encoding; translate at the boundary with checked numeric conversions.

Deliver in four independently testable steps:

1. **Packet interoperability:** real-time Ethernet/IP or explicitly supported Wi-Fi transport; mixed-node request/response demo.
2. **Scenario interoperability:** stable node IDs, startup/configuration, topology, seeded stimuli, stop and artifact collection.
3. **Coordinated time:** negotiated conservative grants only on proven backend configurations.
4. **Repeatable experiments:** replay of supported event sources and explicit nondeterminism reports.

A Linux SBC may act as a gateway to an 802.15.4 network through a modeled UART radio/border-router link or an IP-routing adapter. Ethernet, 802.11 and 802.15.4 frames are distinct typed media. Never connect raw 802.15.4 bytes directly to Ethernet without a defined gateway/encapsulation.

### Proposed SBC-side session contract

The following is a new internal protocol, **not a claim of the current Cooja wire format**:

```json
{"v":1,"id":"h1","op":"hello","node":"pi1","clock":"coordinated","time_unit":"ns","media":["ethernet"],"max_frame_bytes":1518}
{"v":1,"id":"s1","op":"advance","grant_until_ns":"1000000","inputs":[]}
{"v":1,"id":"s1","op":"done","reached_ns":"1000000","next_event_ns":null,"outputs":[]}
{"v":1,"id":"x1","op":"stop"}
```

Handshake returns selected protocol, peer/backend versions, clock guarantees, timestamp resolution, supported media and limits. `advance` permits one outstanding grant per node; replies match request ID. Each frame carries medium, port, source node, timestamp, sequence and base64 bytes. Define FCS included/excluded, byte order, MTU, channel/frequency and TX-status metadata separately per medium in the schema.

The Cooja adapter maps this to the pinned `hello/step/stop/done` contract only after validating exact field types, early-return rules and `wake` semantics. Send protocol only to stdout when using stdio; logs go to stderr. Unexpected EOF or malformed input fails the node and closes the session; never hang the experiment waiting for a dead child.

### Clock levels

| Level | Promise | Backend constraints |
|---|---|---|
| T0 real-time | Best-effort wall-clock runs, measured delivery lateness | Ordinary QEMU; optional compatible acceleration |
| T1 coordinated | Conservative bounded grants, explicit clock resolution/error | Patched/proven QEMU scheduling interface, TCG, no untracked external I/O |
| T2 repeatable | Same supported configuration/input log produces same normalized event trace | Pinned builds, controlled entropy/RTC/storage, deterministic scheduler and inputs |

T1/T2 must be rejected if unsupported, never silently downgraded. A paused QEMU process must not have externally visible device events advanced by wall time.

For coordinated mode, the scheduler owns global time. It grants each node a safe horizon using pending inputs and known lookahead. The node returns at the grant or an earlier externally visible event, with actual reached time and next wake hint. Never issue a packet timestamp before the node's committed time or beyond its grant. Deliver simultaneous events in stable `(time, node_id, seq)` order. Inputs cannot arrive in a committed past; fail the scenario with a diagnostic rather than silently retimestamping them.

Resolve unknown/zero lookahead with conservative barriers and a bounded zero-time-event loop. Detect deadlock and Zeno behavior. Test idle/WFI wakeup, reset epochs, asynchronous interrupts, early TX exits and multi-core ordering. Start T1 with a generic single-vCPU `virt` fixture; do not assume a Pi model accepts `-smp 1`. Pi multi-core coordinated execution is its own feasibility gate. Disable multithreaded TCG for icount experiments and validate the chosen configuration explicitly.

QMP alone cannot enforce these horizons. S34 must prototype a QEMU virtual-clock deadline/event-exit mechanism and prove it with tiny bare-metal fixtures before using Linux. If it cannot prevent causality violations, keep the adapter T0-only. Record/replay of one QEMU instance does not prove distributed replay.

## 13. Staged issue backlog

Each row is an issue template with mandatory dependency and acceptance evidence. Size **S** means roughly 1–2 focused engineer-days, **M** 3–5 days, **L** requires splitting after an initial spike, **R** is research with no delivery guarantee. Estimates exclude review and image-build latency. Never close an issue solely because a document or stub exists when its acceptance requires execution.

### M0 — foundation and evidence

| ID / size | Work and owner area | Dependencies | Acceptance criteria |
|---|---|---|---|
| S00 / M | Pin QEMU, Rust, kernel/build tools; inventory upstream and licenses | None | Exact hashes/commits committed; clean QEMU build; machine/device inventory captured; offline rebuild path described |
| S01 / S | Workspace, Makefile, CI, CLI skeleton and structured errors | S00 | `make help`, format/lint/unit checks work; invalid command exits nonzero; tool versions logged |
| S02 / M | Audit esp32sim/Cooja protocols; write compatibility ADR and fixtures | S00 | Real pinned transcript from each peer; licenses recorded; unsupported fields and media documented; no guessed compatibility |
| S03 / M | Image recipes, DT/kernel lock and immutable storage handling | S00 | Fresh fixture builds; hashes and license manifest generated; base unchanged after guest write |
| S04 / M | QEMU launch, QMP negotiation, serial and cancellation | S01 | Startup/error/stop tests; unique local sockets; killing CLI leaves no owned QEMU child |
| S05 / M | Pi 4 boot, storage and network baseline | S03,S04 | Slice A nonce, persistence and network tests pass, or separate precise blockers remain open |
| S06 / M | Minimal GPIO model bridge and LED/button slice | S05 | Guest character-device output and edge input match host trace; QTest reset/IRQ checks; `make test-slice` passes |
| S07 / S | Profiles/scenarios schema and evidence reports | S01,S05 | Unknown fields/conflicts rejected; capability report links every tested feature to evidence |

### M1 — usable two-board developer release

| ID / size | Work and owner area | Dependencies | Acceptance criteria |
|---|---|---|---|
| S08 / M | Pi 3 independent boot and networking profile | S05,S07 | Pi 3 B0/B1/B2 suite passes; NIC fidelity stated; no Pi 3B+ substitution |
| S09 / M | Complete overlay lifecycle and crash recovery | S03,S04 | Clean stop/power loss/base immutability tests; retained artifacts after failure; no deleting unrelated files |
| S10 / M | Local isolated network fixture and explicit NAT option | S05 | TCP/UDP request/response; default egress deny; host forwarding loopback-only; port collision diagnosed |
| S11 / M | Guest test agent and test runner | S04,S07 | Nonce and bounded command protocol; missing agent handled; no unrestricted command endpoint by default |
| S12 / M | BME280 model and I²C tests | S06,S11 | Real guest driver probes and reads compensated fixture; NACK, reset, timing and malformed access tests |
| S13 / M | MCP3008 SPI model and tests | S06,S11 | Normal guest path reads all channels; CS and boundary cases; QTest and guest integration evidence |
| S14 / S | CLI ergonomics, install/package and first tutorial | S08–S13 | Fresh reference host follows documented commands; diagnostics distinguish unsupported image/device from crash |
| S15 / M | v0.1 release validation | S14 | Pi 4 and Pi 3 declared B3 suites pass; release matrix, known gaps, SBOM and provenance attached |

### M2 — network experiments and W1

| ID / size | Work and owner area | Dependencies | Acceptance criteria |
|---|---|---|---|
| S20 / M | Multi-node lifecycle and isolated switch | S10,S11 | Two guests exchange traffic; one crash does not hang the other; teardown removes only owned resources |
| S21 / M | Impairments, capture and reproducible model tests | S20 | Zero/total loss edge cases; analytic delay/queue tests; seeded model output repeatable; PCAP metadata correct |
| S22 / M | Single-environment hwsim station/AP W1 | S03,S11 | Scan, association, DHCP, data, disconnect, wrong-key tests; exact guest kernel config recorded |
| S23 / L | Cross-guest hwsim relay | S20,S22 | Frames and TX status traverse relay; no duplicate local delivery; channel isolation and loss tests |
| S24 / M | T0 Cooja/esp32 mixed demo | S02,S20 | Linux service exchanges application data with a real peer emulator; explicit radio/IP gateway mapping; artifacts from all processes |
| S25 / M | API v1 and optional local web frontend | S07,S20 | Contract tests and auth/origin checks; UI controls existing API; headless workflow remains complete |
| S26 / S | v0.2 packaging and capability report | S21–S24 | Two-board network scenario plus W1 evidence; no T1/W2 claims; documented resource limits |

### M3 — board expansion, separate from Wi-Fi research

| ID / size | Work and owner area | Dependencies | Acceptance criteria |
|---|---|---|---|
| S30 / M | Zero 2 W board delta audit/implementation | S15 | Dedicated RAM/wiring/DT profile; boot and peripheral evidence; no machine-name-only alias |
| S31 / M | CM4 plus one named carrier | S15 | eMMC/Lite distinction, pin map, Ethernet/wireless options; B0–B2 for selected variant |
| S32 / M | Orange Pi PC portability validation | S15 | 32-bit image, boot, storage, EMAC tests; no Broadcom assumptions in shared control plane |
| S33 / R | Pi 5 architecture feasibility | S15 | Source-backed BCM2712/RP1 map, reuse audit, minimal boot design, bounded next issues and risks |
| S35 / L | Pi 5 CPU/memory/IRQ/timer/UART boot | S33 | Bare-metal fixtures then kernel init with reduced DT; unsupported peripherals declared |
| S36 / L | RP1 PCIe/interrupt skeleton and GPIO | S35 | Enumeration/interrupt tests then real GPIO driver probe and edge test; no IRQ polling workaround hidden |
| S37 / L | RP1 I²C/SPI/UART, storage, Ethernet and USB increments | S36 | Separate issue/test per controller; B1/B2/B3 only as each suite passes |
| S38 / M | CM5 carrier profile | S37 | Verified module/carrier topology and independent boot/peripheral evidence |
| S39 / R | H618 and RK3588/RK3588S feasibility | S32 | Separate SoC inventories and boot spikes; choose one next model based on evidence; no GPU/NPU scope creep |

### M4 — independent time and original-driver wireless research

| ID / size | Work and owner area | Dependencies | Acceptance criteria |
|---|---|---|---|
| S34 / R | QEMU conservative stepping prototype | S02,S21 | Tiny fixture proves grant deadlines/early events/WFI; unsupported configurations rejected; written go/no-go |
| S40 / R | SDIO bus/controller feasibility | S15 | CMD5/52/53, function enable, block I/O and IRQ tests; host-controller gaps enumerated |
| S41 / R | brcmfmac enumeration/download readiness | S40 | Pinned unmodified driver reaches interface creation; trace explains every implemented protocol element |
| S42 / L | W2 scan/join/data path | S41 | Scan, association, DHCP, bidirectional data and reset/reconnect; unsupported requests return defined errors |
| S43 / L | W2 authentication and negative tests | S42 | One named security mode, wrong-key and disconnect tests; no logging of credentials/keys |
| S44 / L | T1 external scheduler integration | S34,S24 | Two-node causality tests pass; grant/epoch/timeout contracts verified with actual peer version |
| S45 / R | T2 repeatability | S44 | Ten identical normalized event hashes for fixed supported fixture; entropy/RTC/I/O controlled; limitations published |
| S46 / R | W3 medium and cross-radio scenarios | S21,S23 or S42,S44 | Declared MAC/PHY model; hidden-node and interference reference tests; separate 802.11/802.15.4 domains |

### M5 — maturity

Split PWM, extra sensors, Bluetooth HCI, display visualization, additional vendor images, Windows support and further boards into independent issues. Each needs an explicit consumer, source specification, test fixture and maintenance owner. An unvalidated feature count is not a release metric.

## 14. Milestones, critical path and estimates

These are planning ranges for approximately two experienced systems developers, not commitments. Research can exceed them substantially.

| Milestone | Planning window | Exit / release rule |
|---|---|---|
| M0 executable slice | Weeks 1–3 | Pi 4 boot/network/storage/GPIO evidence; no UI dependency |
| M1 v0.1 | Roughly weeks 4–12 | Pi 4/3 developer workflow and declared B3 peripherals; W0 |
| M2 v0.2 | Roughly weeks 10–18 | Multi-node experiments, W1 and T0 mixed-emulator demo |
| M3 expansion | After v0.1, incremental | Zero 2 W/CM4/Orange Pi PC independently gated; Pi 5 feasibility first |
| M4 research releases | Several months or longer | T1/T2, Pi 5 maturation and W2/W3 have separate go/no-go decisions |
| M5 broader ecosystem | Evidence-driven | Add boards/features only with maintainable test coverage |

Critical path: S00 -> S03/S04 -> S05 -> S06 -> S08/S10/S11 -> S12/S13 -> S14 -> S15. Wi-Fi W2, Pi 5 and deterministic scheduling are not on the v0.1 critical path. Start protocol discovery in M0 so early schemas do not lock out later interoperability.

## 15. Validation strategy and release evidence

### Layers

- **Rust unit/property tests:** schema validation, lifecycle transitions, bounded queues, time arithmetic, ordering, PRNG isolation, path policy and protocol parsing.
- **QEMU QTests:** register reset/read/write, IRQ mask/assert/clear, bus behavior, DMA bounds, malformed access, migration state where supported. Run only relevant tests on local edits and the broader selected suite before merging.
- **Guest integration:** real driver bind/probe, device I/O and network exchange. A mocked host model is not sufficient for driver claims.
- **System tests:** multi-node startup/failure/cleanup, socket collisions, bad images, disk-full, cancellation, unreachable peer and malicious protocol lengths.
- **Peer contracts:** stored wire fixtures plus live pinned esp32sim/Cooja runs. Test incompatible versions and refusal paths.
- **Performance tests:** measured boot wall time, CPU/RSS, events/second, packet throughput and simulated/wall-time ratio. Record host and accelerator; establish baseline before thresholds.

PR CI runs formatting, lint, unit/contract tests, selected QTests and a cached Pi 4 smoke test. Nightly runs Pi 3/4 full guest suites, fresh image build verification, network tests and sanitizers. Release CI exercises a clean setup on Linux x86_64 and arm64; macOS is marked tested only after its own smoke run. Privileged TAP tests are segregated and do not determine ordinary rootless usability.

Each report includes commit, dirty-tree state, QEMU hash/patch-set hash, kernel/DT/rootfs hashes, scenario hash, board revision, seed, clock mode, peer versions, command argument vector, host summary, start/end time, test IDs, assertions and attachments. Redact secrets from arguments and logs. `skip`, `xfail`, `blocked` and `pass` are distinct; an expected failure cannot grant a capability.

For T2 compare normalized event traces rather than complete Linux console logs. Retain original logs, document normalization and prove it does not discard functional differences. Snapshot support requires coordinated capture of QEMU RAM/device state, disk overlay, host sensors, network queues, PRNG, clock and peers; reject snapshots until all active components can restore consistently.

## 16. Code quality, security and licensing

### Engineering quality

Use idiomatic Rust with strict formatting/clippy, typed errors and explicit cancellation. Avoid panics in host-input paths. Any `unsafe` requires a documented invariant and focused review. Follow QEMU C style and ownership conventions; use endian helpers, checked arithmetic and bounded copies. Run sanitizers on new C parsers/device models and fuzz schemas, frames, SDIO commands and custom control transports.

Version public schemas. An additive optional field is minor; changed meaning or required fields need a major version or negotiated capability. Keep architectural decisions in numbered ADRs. Require a reproducer for fixes and tests for important failure modes; avoid tests that only echo implementation constants.

### Security requirements

Treat guest images, firmware, packets, profiles and peer messages as untrusted input. Validate paths and image/backing formats before launch. Spawn processes with argument arrays, not shell interpolation. Do not interpolate profile values into command strings. Restrict image downloads by scheme, length and hash; avoid archive path traversal during extraction.

Run without root by default. Disable host filesystem sharing, clipboard, USB passthrough, arbitrary host exec and unrestricted network egress unless explicitly requested. Use private per-run directories/sockets, restrictive permissions, resource limits and host-appropriate process containment. Containment varies by host and is not advertised as a perfect security boundary.

Keep QMP and guest-control endpoints local and private. Use short-lived tokens for UI/API sessions; reject cross-origin control attempts. Never expose an unauthenticated QMP listener. API callers cannot select arbitrary host executables. Test agent capabilities must be allowlisted and disabled in distributed user images unless requested.

Apply QEMU security updates through the pin-refresh process, rerun regression suites and publish affected versions. Maintain `SECURITY.md` with private reporting instructions. Never collect credentials, firmware secrets or user disk contents in public CI artifacts.

### Licensing and provenance

Proposed licensing policy: MIT OR Apache-2.0 for independently authored Rust orchestration and reusable schemas where appropriate; preserve the applicable QEMU license for QEMU-derived C patches. Final SPDX choices must be checked against actual dependency/file licenses before the first release. Do not copy QEMU code into permissively labeled crates or assume process separation removes redistribution obligations.

Record provenance for every dependency, source snippet, register specification, firmware and guest package. Preserve notices and fulfill corresponding-source obligations for redistributed GPL components. Publish SBOMs and source/build recipes alongside binaries. Kernel, rootfs and firmware artifacts have their own licenses; the repository's license does not cover them automatically.

Prefer buildable open-source test fixtures. Fetch proprietary firmware only through permitted distribution mechanisms; exclude it from release bundles until redistribution is verified. Clean-room protocol modeling must record public sources and lawful traces. Review names/logos separately; describe compatibility without implying vendor endorsement.

## 17. Agent coordination rules

Copy the following working rules into the future repository's `AGENTS.md` and issue template. They govern implementation agents; they do not authorize external publication or unrelated changes.

1. Read this plan, `AGENTS.md`, the active milestone, relevant ADRs and board evidence before editing.
2. Claim one issue with explicit files/interfaces, dependencies and acceptance tests. A coordination lead owns schema/lockfile changes and merge ordering.
3. Use isolated branches/worktrees when multiple agents work. Do not concurrently modify the same QEMU patch, generated checkout or image build directory. Use per-agent output directories.
4. Parallelize independent work after shared contracts land: launcher, guest fixtures and model tests can proceed separately. Never invent a second incompatible API to avoid waiting for a dependency.
5. Keep patches small; distinguish upstream fixes, SBC extensions and generated artifacts. Do not reformat unrelated code.
6. Before changing an interface, update its schema/ADR and notify affected owners. Add compatibility fixtures and migration notes in the same change.
7. Every handoff includes commit, issue ID, changed files, exact test commands/results, artifacts, limitations and next action. “Works locally” is insufficient.
8. Never fabricate test success, substitute host-only behavior for guest-driver evidence, or change a test to accept the bug. Mark blocked work honestly.
9. Do not mark unsupported features as tested, silently substitute another board, or infer W2/T2 from W0/T0 results.
10. Preserve user changes. Do not discard other agents' branches, force-push shared work, or delete unrelated artifacts.
11. A reviewer checks protocol parsing, DMA/MMIO bounds, process isolation and license provenance for device/transport changes. The implementer supplies a minimal failing test where appropriate.
12. If the same blocker survives two focused attempts, record findings and a minimal reproducer; switch to an unblocked dependency or request a concrete design decision. Research timeboxes end with evidence, not indefinite speculative code.
13. Changes are done only when relevant checks pass, documentation/capabilities agree and evidence is reproducible. The integration owner runs milestone checks after merging dependent work.
14. Do not create sprawling frameworks before the first slice. Prefer one working board, one sensor and one documented transport, then extract proven commonality.

Suggested ownership lanes: orchestration/profiles; QEMU/peripherals; images/guest tests; network/peer adapters. With fewer developers, serialize lanes. Board expansion and W2 require dedicated review capacity; do not overload the core release owner with all research streams.

## 18. Risk register and decisions still required

| Risk | Early signal | Mitigation / decision |
|---|---|---|
| Upstream docs differ from pinned binary | Device present in docs but absent in inventory | S00 capability probe and exact revision evidence |
| Board image stalls before init | Missing IRQ/clock/DT dependency | Minimal initramfs and driver audit; preserve exact failing image |
| Native Ethernet incomplete | Driver probes but no traffic/IRQ | Isolate controller test; explicitly labeled fallback; keep native gate open |
| SD storage mistaken for SDIO support | CMD52/53/function IRQ cannot route | S40 before FullMAC implementation |
| brcmfmac firmware protocol complexity | Long unexplained handshake timeouts | One chip/driver pair, traced stages and four-week feasibility gate |
| Distributed time loses causality | Events arrive behind committed clock | Conservative grants; explicit T0 fallback choice; reject unsupported T1 |
| SMP/icount nondeterminism | Divergent traces on repeated runs | Single-vCPU generic prototype; separate board/SMP research gate |
| Pi 5/RK3588 scope overwhelms team | Many guessed MMIO stubs | Serial/init milestone; no multimedia; staged controller evidence |
| Image maintenance dominates CI | Large downloads or nondeterministic builds | Minimal fixtures, locked cache, periodic rebuilds and provenance |
| Peer protocol drift | New peer breaks transcript tests | Pinned supported range, adapter versions and explicit handshake refusal |
| License/firmware packaging uncertainty | Unclear redistribution terms | Recipes first; block only disputed artifacts, keep open-source work moving |

Decisions to record, not guess: selected QEMU/kernel/Buildroot commits; exact Pi board revisions and first carrier; peer protocol revision; native Pi 3 NIC route; Cooja Wi-Fi transport availability; SDIO controller extension strategy; viable virtual-time grant mechanism; final SPDX policy. These decisions belong to S00/S02/S05/S33/S34/S40 and must not delay writing the CLI skeleton and tests.

## 19. Immediate implementation handoff

Start S00, S01 and protocol discovery S02. Then build S03/S04 and run the Pi 4 Slice A. Add GPIO Slice B before broad peripheral abstractions. The first reviewable implementation should contain:

- A pinned QEMU build recipe and minimal Pi 4 Linux fixture recipe.
- A Rust CLI that validates a scenario, launches QEMU, communicates over QMP, captures serial and shuts down reliably.
- A machine-readable boot/network/storage result with immutable image hashes.
- One QEMU GPIO observation/stimulus extension with register/IRQ tests and a real guest LED/button demonstration.
- A documented `make test-slice` command and retained success/failure artifacts.
- A capability report that accurately states what is missing.

The next implementation agent should execute this handoff, not redesign the project or begin Pi 5, W2, a web UI or a new ARM CPU engine.
