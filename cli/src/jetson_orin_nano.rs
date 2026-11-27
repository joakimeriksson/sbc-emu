//! Jetson Orin Nano direct Linux boot through the headless serial runner.
use crate::{
    serial_board::{self, RunConfig, SerialBoard},
    vnc::Screen,
};
use sbc_machine::Machine as HostMachine;
use std::process::ExitCode;
use tegra234::{boot, Machine};

const TAG: &str = "jetson-orin-nano-4gb";

impl SerialBoard for Machine {
    fn host(&mut self) -> HostMachine<'_> {
        HostMachine::JetsonOrinNano(self)
    }

    fn now(&self) -> u64 {
        Machine::now(self)
    }

    fn instret(&self) -> u64 {
        Machine::instret(self)
    }

    fn screen(&mut self) -> Option<&mut dyn Screen> {
        Machine::display_size(self)?;
        Some(self)
    }

    fn report(&self, tag: &str) {
        eprintln!(
            "[{tag}] pc={:#x} el={} insns={} virtual={:.3}s irqs={}",
            self.cpus[self.current_core()].pc,
            self.cpus[self.current_core()].el,
            self.instret(),
            self.now() as f64 / 1e9,
            self.interrupts,
        );
        if self.gpu_submissions() > 0 {
            eprintln!(
                "[{tag}] GPU: {} completed GPFIFO entries",
                self.gpu_submissions()
            );
        }
    }
}

impl Screen for Machine {
    fn shown_size(&self) -> Option<(u32, u32)> {
        self.display_size()
    }

    fn display_size(&self) -> (u32, u32) {
        tegra234::DISPLAY_SIZE
    }

    fn render(&self, out: &mut Vec<u8>) -> Option<(u32, u32)> {
        Machine::render(self, out)
    }

    fn name(&self) -> &'static str {
        "sbc-emu: Jetson Orin Nano"
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[{TAG}] {err}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &[String]) -> Result<(), String> {
    // (--camera takes no value)
    let valued: Vec<String> = args.iter().filter(|a| *a != "--camera").cloned().collect();
    serial_board::validate_options(
        &valued,
        &[
            "--usb-drive",
            "--camera-source",
            "--smp",
            "--rover-uart",
            "--uart-driver",
            "--bpmp",
            "--gpu",
            "--vnc",
            "--screenshot",
        ],
    )?;
    let config = RunConfig::from_args(TAG, args)?;
    let display = config.vnc.is_some()
        || config.screenshot.is_some()
        || config
            .script
            .iter()
            .any(|line| line.starts_with("screenshot "));
    let cores = crate::arg(args, "--smp")
        .map_or(Ok(tegra234::AFFINITIES.len()), str::parse::<usize>)
        .map_err(|_| "invalid --smp")?;
    let high_speed_uart = match crate::arg(args, "--uart-driver") {
        None | Some("8250") => false,
        Some("tegra") => true,
        Some(_) => return Err("--uart-driver expects 8250 or tegra".into()),
    };
    let gpu = match crate::arg(args, "--gpu") {
        None | Some("off") => false,
        Some("on") => true,
        Some(_) => return Err("--gpu expects on or off".into()),
    };
    let bpmp = match crate::arg(args, "--bpmp") {
        None => gpu,
        Some("off") if gpu => return Err("--gpu on requires BPMP".into()),
        Some("off") => false,
        Some("on") => true,
        Some(_) => return Err("--bpmp expects on or off".into()),
    };
    let ram = serial_board::ram_bytes(args, 4096)?;
    let read = |path: &str| std::fs::read(path).map_err(|err| format!("{path}: {err}"));
    let kernel = read(crate::arg(args, "--kernel").ok_or("expected --kernel IMAGE")?)?;
    let initrd = crate::arg(args, "--initrd").map(read).transpose()?;
    let cmdline = crate::arg(args, "--append").unwrap_or(boot::DEFAULT_CMDLINE);
    let no_jit = args.iter().any(|arg| arg == "--no-jit")
        || std::env::var("SBC_JIT").is_ok_and(|v| v == "0");
    let announced = std::cell::Cell::new(false);
    let mut rover = RoverLink::open(crate::arg(args, "--rover-uart"))?;
    serial_board::run_with_io(
        config,
        || {
            let mut machine = Machine::with_uart_mode(ram, cores, high_speed_uart)?;
            if display {
                machine.enable_display();
            }
            if gpu {
                machine.enable_gpu();
            } else if bpmp {
                machine.enable_bpmp();
            }
            // --usb-drive IMAGE: a USB stick (copy-on-write; each boot starts from the image), as large
            // as its GPT says when the image was cut short after its last partition
            if let Some(path) = crate::arg(args, "--usb-drive") {
                machine.attach_usb_drive(Box::new(
                    crate::disk::CowFile::open(path).map_err(|e| format!("{path}: {e}"))?.fit_gpt(),
                ));
            }
            // --camera: a USB webcam (UVC, 640x480 YUYV, a test pattern); --camera-source SPEC feeds it
            if let Some(spec) = crate::arg(args, "--camera-source") {
                machine.attach_camera(emu_core::usb::Camera::with_source(crate::camera::start(spec)?));
            } else if args.iter().any(|a| a == "--camera") {
                machine.attach_camera(emu_core::usb::Camera::new());
            }
            let jit = machine.set_jit(!no_jit);
            if !announced.replace(true) {
                eprintln!(
                "[{TAG}] experimental Tegra234: {cores} Cortex-A78AE cores ({}), GICv3, ARM timer and SBSA UART",
                crate::cpu_backend(jit),
            );
            }
            boot::boot_linux(&mut machine, &kernel, initrd.as_deref(), cmdline)?;
            Ok(machine)
        },
        |machine| rover.poll(machine),
    )
}

// Bound queued bytes separately from the guest-visible UART FIFO.
const UART_QUEUE_LIMIT: usize = 64 * 1024;

enum RoverUart {
    Detached,
    Loopback,
    Tcp(std::net::TcpStream),
}

struct RoverLink {
    mode: RoverUart,
    transmit: std::collections::VecDeque<u8>,
    receive: std::collections::VecDeque<u8>,
}

impl RoverLink {
    fn open(spec: Option<&str>) -> Result<Self, String> {
        let mode = match spec {
            None => RoverUart::Detached,
            Some("loopback") => RoverUart::Loopback,
            Some(address) => {
                let address = address.parse().map_err(|_| {
                    "--rover-uart expects loopback or a numeric HOST:PORT".to_owned()
                })?;
                let stream = std::net::TcpStream::connect_timeout(
                    &address,
                    std::time::Duration::from_secs(3),
                )
                .map_err(|error| format!("rover UART {address}: {error}"))?;
                stream
                    .set_nonblocking(true)
                    .map_err(|error| format!("rover UART: {error}"))?;
                stream
                    .set_nodelay(true)
                    .map_err(|error| format!("rover UART: {error}"))?;
                RoverUart::Tcp(stream)
            }
        };
        Ok(Self {
            mode,
            transmit: Default::default(),
            receive: Default::default(),
        })
    }

    fn poll(&mut self, machine: &mut Machine) -> Result<(), String> {
        use std::io::{Read, Write};

        let output = machine.take_rover_uart_output();
        match &mut self.mode {
            RoverUart::Detached => return Ok(()),
            RoverUart::Loopback => Self::enqueue(&mut self.receive, output)?,
            RoverUart::Tcp(stream) => {
                Self::enqueue(&mut self.transmit, output)?;
                if !self.transmit.is_empty() {
                    match stream.write(self.transmit.as_slices().0) {
                        Ok(0) => return Err("rover UART peer closed".into()),
                        Ok(bytes) => {
                            self.transmit.drain(..bytes);
                        }
                        Err(error) if Self::retry(&error) => {}
                        Err(error) => return Err(format!("rover UART send: {error}")),
                    }
                }

                let mut buffer = [0; 4096];
                let limit = buffer.len().min(UART_QUEUE_LIMIT - self.receive.len());
                if limit != 0 {
                    match stream.read(&mut buffer[..limit]) {
                        Ok(0) => return Err("rover UART peer closed".into()),
                        Ok(bytes) => self.receive.extend(&buffer[..bytes]),
                        Err(error) if Self::retry(&error) => {}
                        Err(error) => return Err(format!("rover UART receive: {error}")),
                    }
                }
            }
        }
        while let Some(&byte) = self.receive.front() {
            if !machine.rover_uart_input(byte) {
                break;
            }
            self.receive.pop_front();
        }
        Ok(())
    }

    fn enqueue(queue: &mut std::collections::VecDeque<u8>, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.len() > UART_QUEUE_LIMIT - queue.len() {
            return Err("rover UART peer did not drain its byte queue".into());
        }
        queue.extend(bytes);
        Ok(())
    }

    fn retry(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        )
    }
}
