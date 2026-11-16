//! Linux arm64 direct boot with a reduced RK3588S DT, not the vendor firmware chain.
use emu_core::fdt::Fdt;
use crate::Machine;

pub const DEFAULT_CMDLINE:&str="console=ttyS2,1500000n8 earlycon=uart8250,mmio32,0xfeb50000 8250.nr_uarts=3 rdinit=/init loglevel=7";
pub const DTB:u64=0x03f0_0000;
pub const INITRD:u64=0x0400_0000;

pub fn device_tree(ram:u64,cmdline:&str,initrd:Option<(u64,u64)>, sd:bool)->Vec<u8> {
    device_tree_gpu(ram,cmdline,initrd,sd,false)
}
pub fn device_tree_gpu(ram:u64,cmdline:&str,initrd:Option<(u64,u64)>,sd:bool,gpu:bool)->Vec<u8> {
    let mut f=Fdt::default();
    f.begin("");
    f.strs("compatible", &["xunlong,orangepi-5","rockchip,rk3588s"]);
    f.str("model","Orange Pi 5 (sbc-emu reduced RK3588S DT, one A55)");
    f.cells("#address-cells",&[2]); f.cells("#size-cells",&[2]); f.cells("interrupt-parent",&[1]);
    f.begin("aliases");f.str("serial2","/serial@feb50000");f.end();
    f.begin("chosen");f.str("bootargs",cmdline);f.str("stdout-path","serial2:1500000n8");
    if let Some((a,b))=initrd {f.cells("linux,initrd-start",&[(a>>32) as u32,a as u32]);f.cells("linux,initrd-end",&[(b>>32) as u32,b as u32]);}
    f.end();
    f.begin("memory@0");f.str("device_type","memory");f.cells("reg",&[0,0,(ram>>32) as u32,ram as u32]);f.end();
    f.begin("cpus");f.cells("#address-cells",&[1]);f.cells("#size-cells",&[0]);
    f.begin("cpu@0");f.str("device_type","cpu");f.str("compatible","arm,cortex-a55");f.cells("reg",&[0]);f.str("enable-method","psci");f.end();f.end();
    f.begin("psci");f.strs("compatible",&["arm,psci-1.0","arm,psci-0.2"]);f.str("method","smc");f.end();
    f.begin("timer");f.str("compatible","arm,armv8-timer");f.empty("always-on");
    f.cells("interrupts",&[1,13,4,1,14,4,1,11,4,1,10,4]);f.end();
    f.begin("interrupt-controller@fe600000");f.str("compatible","arm,gic-v3");
    f.cells("reg",&[0,0xfe600000,0,0x10000,0,0xfe680000,0,0x20000]);
    f.empty("interrupt-controller");f.cells("#interrupt-cells",&[3]);f.cells("#address-cells",&[2]);f.cells("#size-cells",&[2]);f.empty("ranges");f.cells("phandle",&[1]);f.end();
    // Firmware-initialized clocks: CRU/PMU/pinctrl are intentionally absent in this DT.
    f.begin("uart-clock");f.str("compatible","fixed-clock");f.cells("#clock-cells",&[0]);f.cells("clock-frequency",&[24_000_000]);f.cells("phandle",&[2]);f.end();
    f.begin("serial@feb50000");f.strs("compatible",&["rockchip,rk3588-uart","snps,dw-apb-uart"]);
    f.cells("reg",&[0,0xfeb50000,0,0x100]);f.cells("interrupts",&[0,333,4]);
    f.cells("reg-shift",&[2]);f.cells("reg-io-width",&[4]);f.cells("clocks",&[2,2]);f.strs("clock-names",&["baudclk","apb_pclk"]);f.end();
    if gpu {
        f.begin("gpu-supply");f.str("compatible","regulator-fixed");f.str("regulator-name","gpu");
        f.cells("regulator-min-microvolt",&[750000]);f.cells("regulator-max-microvolt",&[750000]);
        f.empty("regulator-always-on");f.cells("phandle",&[4]);f.end();
        f.begin("gpu-opp-table");f.str("compatible","operating-points-v2");f.cells("phandle",&[5]);
        f.begin("opp-24000000");f.cells("opp-hz",&[0,24000000]);f.cells("opp-microvolt",&[750000]);f.end();f.end();
        f.begin("gpu@fb000000"); f.cells("mali-supply",&[4]);f.cells("operating-points-v2",&[5]); f.strs("compatible",&["rockchip,rk3588-mali","arm,mali-valhall-csf"]);
        f.cells("reg",&[0,0xfb000000,0,0x200000]);f.cells("interrupts",&[0,92,4,0,93,4,0,94,4]);
        f.strs("interrupt-names",&["job","mmu","gpu"]);f.cells("clocks",&[2,2,2]);
        f.strs("clock-names",&["core","coregroup","stacks"]);f.end();
    }
    if sd {
        f.begin("mmc@fe2c0000");f.strs("compatible",&["rockchip,rk3588-dw-mshc","rockchip,rk3288-dw-mshc"]);
        f.cells("reg",&[0,0xfe2c0000,0,0x4000]);f.cells("interrupts",&[0,203,4]);
        f.cells("clocks",&[2,2]);f.strs("clock-names",&["biu","ciu"]);f.cells("fifo-depth",&[256]);
        f.cells("bus-width",&[4]);f.cells("max-frequency",&[24000000]);f.empty("disable-wp");f.end();
    }
    f.end();f.finish()
}

pub fn boot_linux(m:&mut Machine,kernel:&[u8],initrd:Option<&[u8]>,cmdline:&str)->Result<(),String> {
    if kernel.len()<64 || &kernel[56..60]!=b"ARMd" {return Err("expected uncompressed arm64 Linux Image".into());}
    let offset=u64::from_le_bytes(kernel[8..16].try_into().unwrap());
    let size=u64::from_le_bytes(kernel[16..24].try_into().unwrap()).max(kernel.len() as u64);
    let entry=0x20_0000u64.checked_add(offset).ok_or("kernel offset overflow")?;
    if entry.checked_add(size).is_none_or(|end|end>DTB) {return Err("kernel exceeds direct boot layout".into());}
    m.soc.ram[entry as usize..entry as usize+kernel.len()].copy_from_slice(kernel);
    let range=if let Some(data)=initrd {
        let end=INITRD.checked_add(data.len() as u64).ok_or("initrd size overflow")?;
        if end>m.soc.ram.len() as u64 {return Err("initrd exceeds RAM".into());}
        m.soc.ram[INITRD as usize..end as usize].copy_from_slice(data);Some((INITRD,end))
    }else{None};
    let dt=device_tree_gpu(m.soc.ram.len() as u64,cmdline,range,m.soc.mmc.card.present(),m.gpu_probe);
    if DTB+dt.len() as u64>INITRD {return Err("device tree exceeds boot layout".into());}
    m.soc.ram[DTB as usize..DTB as usize+dt.len()].copy_from_slice(&dt);
    m.cpu.el=1; m.cpu.spsel=true; m.cpu.daif=0x3c0;m.cpu.pc=entry;
    m.cpu.x=[0;31];m.cpu.x[0]=DTB;m.cpu.sys.cpacr_el1=3<<20;
    Ok(())
}

/// Boot inputs read by emulated firmware from a GPT/ext4 Armbian image.
pub struct CardBoot {
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    pub notes: Vec<String>,
}

/// Implements the relevant Armbian `boot.cmd` inputs. The emulator generates its
/// reduced DT; neither U-Boot nor DT overlays execute here.
pub fn read_card(disk: &mut dyn emu_core::disk::Disk) -> Result<CardBoot, String> {
    use emu_core::ext4::Ext4;
    let parts=emu_core::gpt::partitions(disk)?;
    for part in parts {
        let Ok(mut fs)=Ext4::open(disk,part.first_lba) else {continue};
        for prefix in ["/boot/", "/"] {
            if !fs.exists(&format!("{prefix}Image")) {continue;}
            let mut notes=Vec::new();
            if fs.needs_recovery {notes.push("ext4 journal needs recovery; firmware does not replay it".to_string());}
            let env_path=format!("{prefix}armbianEnv.txt");
            let env_bytes=if fs.exists(&env_path){fs.read_file(&env_path)?}else{Vec::new()};
            let env_text=std::str::from_utf8(&env_bytes).map_err(|_|"armbianEnv.txt is not UTF-8")?;
            let mut env=std::collections::HashMap::new();
            for line in env_text.lines() {
                let line=line.trim();if line.is_empty()||line.starts_with('#'){continue;}
                if let Some((k,v))=line.split_once('='){env.insert(k.trim(),v.trim());}
            }
            let get=|key:&str,default:&str|env.get(key).copied().unwrap_or(default).to_string();
            if ["overlays","user_overlays"].iter().any(|k|env.get(k).is_some_and(|v|!v.is_empty())) {
                notes.push("device-tree overlays are not applied".into());
            }
            let kernel=fs.read_file(&format!("{prefix}Image"))?;
            let init_path=format!("{prefix}uInitrd");
            let initrd=if fs.exists(&init_path) {
                let data=fs.read_file(&init_path)?;
                if data.len()<64 || data[..4]!=[0x27,0x05,0x19,0x56] {return Err("uInitrd has no U-Boot legacy header".into());}
                let len=u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
                let end=64usize.checked_add(len).filter(|&v|v<=data.len()).ok_or("truncated uInitrd")?;
                Some(data[64..end].to_vec())
            }else{None};
            let root_default=format!("UUID={}",fs.uuid_string());
            let cmdline=format!("root={} rootwait rootfstype={} console=ttyS2,1500000 earlycon=uart8250,mmio32,0xfeb50000 8250.nr_uarts=3 loglevel={} {} {}",
                get("rootdev",&root_default),get("rootfstype","ext4"),get("verbosity","1"),get("extraargs",""),get("extraboardargs",""));
            notes.push(format!("GPT partition {} at LBA {}; {}Image",part.number,part.first_lba,prefix));
            return Ok(CardBoot{kernel,initrd,cmdline,notes});
        }
    }
    Err("no GPT ext4 partition containing Image in /boot or /".into())
}
