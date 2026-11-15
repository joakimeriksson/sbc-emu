//! RK3588 DesignWare MMC: single SDHC card, 32-bit PIO FIFO and interrupt interface.
//! This initial model does not implement DMA, voltage switching or eMMC.
use emu_core::sdcard::{Data, Response, SdCard, OUT_OF_RANGE};

const CMD_DONE:u32=1<<2;
const DATA_OVER:u32=1<<3;
const TXDR:u32=1<<4;
const RXDR:u32=1<<5;
const RTO:u32=1<<8;
const DRTO:u32=1<<9;

#[derive(Clone,Copy,PartialEq,Eq)]
enum Transfer { None, Read, Write }

/// The card protocol lives in `emu_core::sdcard`; this is the DesignWare host interface.
pub struct Mmc {
    pub card: SdCard,
    regs:[u32; 128],
    transfer:Transfer,
    buffer:Vec<u8>,
    position:usize,
    lba:u64,
    remaining:usize,
    raw:u32,
}
impl Default for Mmc {
    fn default()->Self {
        let mut card=SdCard::new(None);
        card.trace=std::env::var_os("SBC_MMC_TRACE").is_some();
        Self{card,regs:[0;128],transfer:Transfer::None,buffer:Vec::new(),position:0,lba:0,remaining:0,raw:0}
    }
}
impl Mmc {
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
            0x50=>u32::from(!self.card.present()),
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
        let idx=cmd&63;let arg=self.regs[0x28/4];
        let reply=self.card.command(idx,arg);
        if idx==0 {self.transfer=Transfer::None;self.buffer.clear();self.position=0;}
        let Some(response)=reply.response else {
            if idx!=0 {self.raw|=RTO;}
            self.raw|=CMD_DONE;
            return;
        };
        match response {
            Response::Short(word)=>{
                self.regs[0x30/4]=word;
                // A data command that is out of range: no data phase, a data error at once.
                // dw_mmc completes a failed data phase on DATA_OVER, as the hardware sets DTO
                // with the error.
                if word & OUT_OF_RANGE!=0 && cmd & (1<<9)!=0 {self.transfer=Transfer::None;self.raw|=DRTO|DATA_OVER;}
            }
            Response::Long(value)=>for i in 0..4 {self.regs[0x30/4+i]=(value>>(i*32)) as u32;},
        }
        let bytes=self.regs[0x20/4] as usize;
        match reply.data {
            Data::None=>{if idx==12 {self.transfer=Transfer::None;}}
            Data::Bytes(v)=>{self.remaining=v.len();self.buffer=v;self.position=0;self.transfer=Transfer::Read;}
            Data::Read{lba}|Data::Write{lba}=>{
                if bytes==0 || bytes%512!=0 || !self.card.in_range(lba,(bytes/512) as u64) {
                    self.transfer=Transfer::None;
                    self.regs[0x30/4]|=OUT_OF_RANGE;
                    self.raw|=DRTO|DATA_OVER;
                } else {
                    self.lba=lba;self.remaining=bytes;self.position=0;
                    if matches!(reply.data,Data::Read{..}) {self.transfer=Transfer::Read;self.load_block();}
                    else {self.transfer=Transfer::Write;self.buffer=vec![0;512];}
                }
            }
        }
        self.raw|=CMD_DONE;
    }
    fn load_block(&mut self){self.buffer=vec![0;512];self.position=0;self.card.read_blocks(self.lba,&mut self.buffer);}
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
        if self.position==self.buffer.len(){self.card.write_blocks(self.lba,&self.buffer);self.lba+=1;self.position=0;}
        if self.remaining==0 {self.transfer=Transfer::None;self.raw|=DATA_OVER;}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(m:&mut Mmc,cmd:u32,arg:u32) {m.write(0x28,arg);m.write(0x2c,(1<<31)|cmd);}
    #[test]
    fn sd_read_write_and_irq() {
        let mut m=Mmc::default();m.card.disk=Some(Box::new(emu_core::disk::MemDisk::new(1024)));
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
        let mut m=Mmc::default();m.card.disk=Some(Box::new(emu_core::disk::MemDisk::new(1024)));
        m.write(0x20,1024);command(&mut m,18|(1<<9),1023);
        assert_ne!(m.read(0x30)&(1<<31),0);
        assert_eq!(m.read(0x44)&RXDR,0);
        assert_ne!(m.read(0x44)&DRTO,0, "data error, not a stalled data phase");
        assert_ne!(m.read(0x44)&DATA_OVER,0, "dw_mmc completes the failed request on DATA_OVER");
    }
    #[test]
    fn erase_commands_are_answered() {
        let mut m=Mmc::default();m.card.disk=Some(Box::new(emu_core::disk::MemDisk::new(1024)));
        for (cmd,arg) in [(32,8),(33,15),(38,0)] {
            command(&mut m,cmd,arg);
            assert_eq!(m.read(0x44)&RTO,0,"CMD{cmd} answered");
            assert_eq!(m.read(0x30)&(1<<31),0);
            m.write(0x44,u32::MAX);
        }
    }
    #[test]
    fn absent_card_times_out() {let mut m=Mmc::default();command(&mut m,8,0x1aa);assert_ne!(m.read(0x44)&RTO,0);assert_eq!(m.read(0x50),1);}
}
