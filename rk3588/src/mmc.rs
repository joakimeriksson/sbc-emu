//! RK3588 DesignWare MMC: single SDHC card, 32-bit PIO FIFO and interrupt interface.
//! This initial model does not implement DMA, voltage switching or eMMC.
use emu_core::disk::Disk;

const CMD_DONE:u32=1<<2;
const DATA_OVER:u32=1<<3;
const TXDR:u32=1<<4;
const RXDR:u32=1<<5;
const RTO:u32=1<<8;

#[derive(Clone,Copy,PartialEq,Eq)]
enum State { Idle, Ready, Ident, Standby, Transfer }
#[derive(Clone,Copy,PartialEq,Eq)]
enum Transfer { None, Read, Write }

pub struct Mmc {
    pub disk: Option<Box<dyn Disk>>,
    regs:[u32; 128],
    state:State,
    app:bool,
    rca:u16,
    transfer:Transfer,
    buffer:Vec<u8>,
    position:usize,
    lba:u64,
    remaining:usize,
    raw:u32,
}
impl Default for Mmc {
    fn default()->Self {Self{disk:None,regs:[0;128],state:State::Idle,app:false,rca:0,
        transfer:Transfer::None,buffer:Vec::new(),position:0,lba:0,remaining:0,raw:0}}
}
impl Mmc {
    fn status(&self)->u32 { let st=match self.state {State::Idle=>0,State::Ready=>1,State::Ident=>2,State::Standby=>3,State::Transfer=>4}; (st<<9)|(1<<8) }
    fn events(&self)->u32 {
        self.raw | match self.transfer {Transfer::Read if self.position<self.buffer.len()=>RXDR,
            Transfer::Write if self.remaining>0=>TXDR,_=>0}
    }
    pub fn irq(&self)->bool { self.regs[0] & (1<<4)!=0 && self.events() & self.regs[0x24/4]!=0 }
    pub fn read(&mut self,off:u64)->u32 {
        match off {
            0x40=>self.events() & self.regs[0x24/4],
            0x44=>self.events(),
            0x48=> { let count=if self.transfer==Transfer::Read {(self.buffer.len().saturating_sub(self.position).div_ceil(4)).min(256)}else{0}; ((count as u32)<<17)|if count==0 {1<<2}else{0} },
            0x50=>u32::from(self.disk.is_none()),
            0x6c=>0x240a,
            0x70=>(3<<16)|(1<<7), // 32-bit PIO, no DMA
            0x200=>self.read_fifo(),
            _ if (off as usize / 4)<self.regs.len()=>self.regs[off as usize/4],
            _=>0,
        }
    }
    pub fn write(&mut self,off:u64,v:u32) {
        match off {
            0x00=>{self.regs[0]=v & !7;if v&2!=0 {self.transfer=Transfer::None;self.buffer.clear();self.position=0;}},
            0x2c=>{self.regs[off as usize/4]=v&!(1<<31); if v&(1<<31)!=0 && v&(1<<21)==0 {self.command(v);}},
            0x44=>self.raw &= !v,
            0x200=>self.write_fifo(v),
            _ if (off as usize/4)<self.regs.len()=>self.regs[off as usize/4]=v,
            _=>{}
        }
    }
    fn command(&mut self,cmd:u32) {
        let idx=cmd&63;let arg=self.regs[0x28/4];let app=std::mem::take(&mut self.app);
        if std::env::var_os("SBC_MMC_TRACE").is_some(){eprintln!("[rk-mmc] {}CMD{idx} arg={arg:#x} flags={cmd:#x} bytes={}",if app {"A"}else{""},self.regs[0x20/4]);}
        if self.disk.is_none() {self.raw|=RTO|CMD_DONE;return;}
        let mut long=None;
        let r=match (app,idx) {
            (_,0)=>{self.state=State::Idle;self.transfer=Transfer::None;0},
            (_,8)=>arg&0xfff,
            (_,55)=>{self.app=true;self.status()|(1<<5)},
            (true,41)=>{if arg&0xff8000!=0 {self.state=State::Ready;} 0xc0ff8000},
            (_,2)=>{self.state=State::Ident;long=Some(0x5b5345534243454d10005bc000011ab1u128);0},
            (_,3)=>{self.state=State::Standby;self.rca=0x5bc0;0x5bc00500},
            (_,9)=>{let sectors=self.disk.as_ref().unwrap().sectors();let c=(sectors/1024).saturating_sub(1) as u128 & 0x3fffff;long=Some((1u128<<126)|(0x0e<<112)|(0x32<<96)|(0x5b5<<84)|(9<<80)|(c<<48)|(0x7f<<39)|(2<<26)|(9<<22)|1);0},
            (_,7)=>{let r=self.status();self.state=if (arg>>16) as u16==self.rca {State::Transfer}else{State::Standby};r},
            (true,6|23|42)=>self.status(),
            (true,51)=>{self.start_special(vec![2,0x35,0x80,0,0,0,0,0]);self.status()},
            (true,13)=>{let mut v=vec![0;64];v[0]=0x80;self.start_special(v);self.status()},
            (false,6)=>{let mut v=vec![0;64];v[1]=100;v[12]=0x80;v[13]=3;v[16]=u8::from(arg&15==1);self.start_special(v);self.status()},
            (_,12)=>{self.transfer=Transfer::None;self.status()},
            (_,13|16|23)=>self.status(),
            (_,17|18|24|25)=>{
                let bytes=self.regs[0x20/4] as usize;
                let sectors=self.disk.as_ref().unwrap().sectors();
                if bytes==0 || bytes%512!=0 || (arg as u64).checked_add((bytes/512) as u64).is_none_or(|end|end>sectors) {
                    self.transfer=Transfer::None;
                    self.status() | (1 << 31) // R1 OUT_OF_RANGE
                } else {
                    self.lba=arg as u64;self.remaining=bytes;self.position=0;
                    if idx==17||idx==18 {self.transfer=Transfer::Read;self.load_block();}
                    else {self.transfer=Transfer::Write;self.buffer=vec![0;512];}
                    self.status()
                }
            },
            _=>{self.raw|=RTO|CMD_DONE;return;}
        };
        self.regs[0x30/4]=r;
        if let Some(v)=long {for i in 0..4 {self.regs[0x30/4+i]=(v>>(i*32)) as u32;}}
        self.raw|=CMD_DONE;
    }
    fn start_special(&mut self,v:Vec<u8>){self.remaining=v.len();self.buffer=v;self.position=0;self.transfer=Transfer::Read;}
    fn load_block(&mut self){self.buffer=vec![0;512];self.position=0;if let Some(d)=self.disk.as_mut(){if self.lba<d.sectors(){d.read(self.lba,&mut self.buffer);}}}
    fn read_fifo(&mut self)->u32 {
        if self.transfer!=Transfer::Read{return 0;}
        let n=self.remaining.min(self.buffer.len().saturating_sub(self.position)).min(4);
        let mut data=[0;4];data[..n].copy_from_slice(&self.buffer[self.position..self.position+n]);self.position+=n;self.remaining-=n;
        if self.remaining==0 {self.transfer=Transfer::None;self.raw|=DATA_OVER;}
        else if self.position==self.buffer.len(){self.lba+=1;self.load_block();}
        u32::from_le_bytes(data)
    }
    fn write_fifo(&mut self,v:u32){
        if self.transfer!=Transfer::Write{return;}
        let n=self.remaining.min(self.buffer.len().saturating_sub(self.position)).min(4);
        self.buffer[self.position..self.position+n].copy_from_slice(&v.to_le_bytes()[..n]);self.position+=n;self.remaining-=n;
        if self.position==self.buffer.len(){if let Some(d)=self.disk.as_mut(){if self.lba<d.sectors(){d.write(self.lba,&self.buffer);}}self.lba+=1;self.position=0;}
        if self.remaining==0 {self.transfer=Transfer::None;self.raw|=DATA_OVER;}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory(Vec<u8>);
    impl Disk for Memory {
        fn sectors(&self)->u64 {self.0.len() as u64/512}
        fn read(&mut self,lba:u64,out:&mut [u8]) {let i=lba as usize*512;out.copy_from_slice(&self.0[i..i+out.len()]);}
        fn write(&mut self,lba:u64,data:&[u8]) {let i=lba as usize*512;self.0[i..i+data.len()].copy_from_slice(data);}
    }
    fn command(m:&mut Mmc,cmd:u32,arg:u32) {m.write(0x28,arg);m.write(0x2c,(1<<31)|cmd);}
    #[test]
    fn sd_read_write_and_irq() {
        let mut m=Mmc::default();m.disk=Some(Box::new(Memory(vec![0;1024*512])));
        m.write(0,1<<4);m.write(0x24,u32::MAX);
        command(&mut m,55,0);command(&mut m,41,0x40ff8000);
        assert_eq!(m.read(0x30),0xc0ff8000);assert!(m.irq());m.write(0x44,u32::MAX);assert!(!m.irq());
        m.write(0x20,512);command(&mut m,24|(1<<9)|(1<<10),1);
        for n in 0..128 {m.write(0x200,n);}
        assert_ne!(m.read(0x44)&DATA_OVER,0);m.write(0x44,u32::MAX);
        command(&mut m,17|(1<<9),1);
        for n in 0..128 {assert_eq!(m.read(0x200),n);}
        assert_ne!(m.read(0x44)&DATA_OVER,0);
    }
    #[test]
    fn out_of_range_block_request_reports_error() {
        let mut m=Mmc::default();m.disk=Some(Box::new(Memory(vec![0;1024*512])));
        m.write(0x20,1024);command(&mut m,18|(1<<9),1023);
        assert_ne!(m.read(0x30)&(1<<31),0);
        assert_eq!(m.read(0x44)&RXDR,0);
    }
    #[test]
    fn absent_card_times_out() {let mut m=Mmc::default();command(&mut m,8,0x1aa);assert_ne!(m.read(0x44)&RTO,0);assert_eq!(m.read(0x50),1);}
}
