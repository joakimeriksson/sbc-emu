//! Native serial-only RK3588S bring-up adapter. Unsupported Pi options fail explicitly.
use crate::arg;
use std::{collections::VecDeque, io::{Read, Write}, process::ExitCode,
    sync::mpsc, time::{Duration, Instant}};
use sbc_machine::{Machine as HostMachine, Stop};

pub(crate) fn run(args:&[String])->ExitCode {
    match execute(args) {Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("[orangepi5] {e}");ExitCode::FAILURE}}
}
fn execute(args:&[String])->Result<(),String> {
    let mut index=1;
    while index<args.len() {
        let a=args[index].as_str();
        if a.starts_with("--board=") || matches!(a,"--no-jit"|"--no-display") {index+=1;continue;}
        if !matches!(a,"--board"|"--kernel"|"--initrd"|"--append"|"--script"|"--timeout"|"--max-insns"|"--ram-mb"|"--smp"|"--threads"|"--dump-dtb") {
            return Err(format!("unsupported Orange Pi option '{a}'; this machine provides direct boot and serial only"));
        }
        if args.get(index+1).is_none_or(|v|v.starts_with("--")) {return Err(format!("{a} requires a value"));}
        index+=2;
    }
    if arg(args,"--smp").is_some_and(|s|s!="1") {return Err("only --smp 1 is implemented for Orange Pi".into());}
    if arg(args,"--threads").is_some_and(|s|s!="single") {return Err("Orange Pi currently uses deterministic single-thread execution".into());}
    let kernel=std::fs::read(arg(args,"--kernel").ok_or("--kernel Image is required")?).map_err(|e|e.to_string())?;
    let initrd=arg(args,"--initrd").map(std::fs::read).transpose().map_err(|e|e.to_string())?;
    let cmdline=arg(args,"--append").unwrap_or(rk3588::boot::DEFAULT_CMDLINE);
    let ram=arg(args,"--ram-mb").unwrap_or("256").parse::<usize>().map_err(|_|"invalid --ram-mb")?.checked_mul(1<<20).ok_or("RAM size overflow")?;
    let max=arg(args,"--max-insns").unwrap_or("18446744073709551615").parse::<u64>().map_err(|_|"invalid --max-insns")?;
    let timeout=arg(args,"--timeout").map(|v|v.parse::<f64>().map_err(|_|"invalid --timeout"))
        .transpose()?.map(Duration::try_from_secs_f64).transpose().map_err(|_|"invalid --timeout")?;
    let make_machine=||->Result<rk3588::Machine,String>{
        let mut m=rk3588::Machine::new(ram)?;
        rk3588::boot::boot_linux(&mut m,&kernel,initrd.as_deref(),cmdline)?;
        if !args.iter().any(|a|a=="--no-jit") {m.cpu.enable_jit();}
        Ok(m)
    };
    let mut m=make_machine()?;
    if let Some(path)=arg(args,"--dump-dtb") {
        std::fs::write(path,rk3588::boot::device_tree(ram as u64,cmdline,initrd.as_ref().map(|d|(rk3588::boot::INITRD,rk3588::boot::INITRD+d.len() as u64)))).map_err(|e|e.to_string())?;
    }
    let mut script:VecDeque<String>=match arg(args,"--script") {
        Some(p)=>std::fs::read_to_string(p).map_err(|e|e.to_string())?.lines().map(str::trim)
            .filter(|s|!s.is_empty()&&!s.starts_with('#')).map(str::to_owned).collect(),None=>VecDeque::new()
    };
    for line in &script {
        if !matches!(line.split_whitespace().next(),Some("wait-serial"|"serial"|"quit"|"reset")) {return Err(format!("unsupported Orange Pi script command: {line}"));}
    }
    eprintln!("[orangepi5] experimental RK3588S: one reduced Cortex-A55, GICv3, UART2; direct kernel boot, reduced DT");
    let _terminal=crate::terminal::RawTty::enable();
    let (tx,rx)=mpsc::channel();
    std::thread::spawn(move||{let mut stdin=std::io::stdin();let mut b=[0];while let Ok(1)=stdin.read(&mut b){if tx.send(b[0]).is_err(){break;}}});
    let mut idle_input=Vec::new();
    let mut pending=VecDeque::new();let mut output=std::io::stdout();let mut serial=String::new();
    let start=Instant::now();let mut anchor=Instant::now();let mut escape=false;
    let result=loop {
        let mut input=std::mem::take(&mut idle_input);
        while let Ok(b)=rx.try_recv(){input.push(b);}
        for b in input {
            if escape {escape=false;if b==b'x' {return Ok(());} pending.push_back(1);}
            else if b==1 {escape=true;continue;}
            pending.push_back(b);
        }
        if let Some(line)=script.front().cloned() {
            let (cmd,rest)=line.split_once(' ').unwrap_or((&line,""));
            let done=match cmd {
                "wait-serial"=>serial.contains(rest),
                "serial"=>{pending.extend(rest.replace("\\n","\r").replace("\\r","\r").replace("\\t","\t").bytes());true},
                "reset"=>{m=make_machine()?;anchor=Instant::now();serial.clear();pending.clear();true},
                "quit"=>break Ok(()),_=>unreachable!(),
            };
            if done {eprintln!("[orangepi5 script] {line}");script.pop_front();if cmd=="wait-serial"{serial.clear();}}
        }
        while let Some(&b)=pending.front(){if !HostMachine::OrangePi5(&mut m).serial_input(b){break;}pending.pop_front();}
        if timeout.is_some_and(|t|start.elapsed()>=t){break Err("host timeout before completion".into());}
        if m.cpu.instret>=max {break if script.is_empty(){Ok(())}else{Err("instruction limit before script completed".into())};}
        let budget=(max-m.cpu.instret).min(1_000_000);
        let stop=HostMachine::OrangePi5(&mut m).run(budget);
        let bytes=HostMachine::OrangePi5(&mut m).take_serial_output();
        if !bytes.is_empty(){output.write_all(&bytes).map_err(|e|e.to_string())?;output.flush().map_err(|e|e.to_string())?;
            serial.push_str(&String::from_utf8_lossy(&bytes));
            if serial.len()>1<<20 {let mut cut=serial.len()-(1<<19);while !serial.is_char_boundary(cut){cut+=1;}serial.drain(..cut);}
        }
        match stop {
            Stop::Budget=>{},
            Stop::Idle(deadline)=>{
                let wall=anchor.elapsed().as_nanos() as u64;
                if wall<=m.now(){
                    let wait=Duration::from_nanos(deadline.saturating_sub(wall).min(20_000_000).max(1));
                    if let Ok(b)=rx.recv_timeout(wait){idle_input.push(b);}
                }
                let target=deadline.min((anchor.elapsed().as_nanos() as u64).max(m.now()));
                HostMachine::OrangePi5(&mut m).idle_until(target);
            },
            Stop::Reset=>{m=make_machine()?;anchor=Instant::now();serial.clear();pending.clear();},
            Stop::PowerOff=>break if script.is_empty(){Ok(())}else{Err("guest powered off before script completed".into())},
            other=>break Err(format!("stopped: {other:?}")),
        }
    };
    eprintln!("[orangepi5] pc={:#x} insns={} virtual={:.3}s irqs={} acknowledgements={} exceptions={}",m.cpu.pc,m.cpu.instret,m.now() as f64/1e9,m.irqs,m.soc.gic.acknowledges,m.exceptions);
    if !m.soc.unknown.is_empty(){eprintln!("[orangepi5] unmapped MMIO: {:x?}",m.soc.unknown);}
    if !m.cpu.sys.unknown.is_empty(){eprintln!("[orangepi5] unmodelled system registers: {:x?}",m.cpu.sys.unknown);}
    result
}
