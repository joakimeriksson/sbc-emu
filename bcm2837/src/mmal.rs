//! The VideoCore's MMAL service (`mmal` over VCHIQ) and the shared-memory service (`SMEM`)
//! that the Raspberry Pi kernel's media drivers use: `bcm2835-mmal-vchiq` serialises MMAL
//! calls (create a component, read and set its ports' formats, parameters, enable, buffers),
//! `bcm2835-codec` builds V4L2 memory-to-memory devices on the components (`/dev/video10`
//! decode, 11 encode, 12 ISP, 18 image effects, 31 JPEG encode), `bcm2835-isp` another set on
//! the ISP, and `vc-sm-cma` imports the vb2 buffers' memory into the VideoCore so buffers pass
//! by handle ("zero copy"): the model then reads and writes guest RAM directly.
//!
//! Components (`COMPONENT_CREATE` by name):
//! - `ril.video_decode`: H.264 in (the formats `video::decoder_for` knows), 4:2:0 or RGB565
//!   out; pictures come from a `video::VideoDecoder`, with the `EFCH` format-changed event on
//!   the output port when the stream's size is known or changes, as the firmware sends it.
//! - `ril.isp`: one input, two image outputs and a statistics port; converts between the YUV
//!   and RGB formats and scales (bilinear). `ril.image_fx` the same with one output (no
//!   deinterlacing: pictures pass through).
//! - `ril.video_encode`, `ril.image_encode`: the ports and formats, but no encoder: every
//!   input picture gives an empty output buffer.
//! - `camera_info`: no camera attached.
//!
//! Messages are the 24-byte `mmal_msg_header` (magic, type, control service, context,
//! status) and a type-specific body; layouts as `mmal-msg.h` in the kernel defines them.
use crate::vchiq::{phys, read_bytes, write_bytes, Outbox};
use crate::video::{self, mmal_fourcc, Image, Picture, VideoDecoder};
use std::collections::VecDeque;

pub const MAGIC: u32 = mmal_fourcc(b"mmal");
const HDR: usize = 24;

pub const GET_VERSION: u32 = 3;
pub const COMPONENT_CREATE: u32 = 4;
pub const COMPONENT_DESTROY: u32 = 5;
pub const COMPONENT_ENABLE: u32 = 6;
pub const COMPONENT_DISABLE: u32 = 7;
pub const PORT_INFO_GET: u32 = 8;
pub const PORT_INFO_SET: u32 = 9;
pub const PORT_ACTION: u32 = 10;
pub const BUFFER_FROM_HOST: u32 = 11;
pub const BUFFER_TO_HOST: u32 = 12;
pub const PORT_PARAMETER_SET: u32 = 14;
pub const PORT_PARAMETER_GET: u32 = 15;
pub const EVENT_TO_HOST: u32 = 16;

pub const ST_SUCCESS: u32 = 0;
pub const ST_ENOSPC: u32 = 2;
pub const ST_EINVAL: u32 = 3;
pub const ST_ENOSYS: u32 = 4;
pub const ST_ENOENT: u32 = 5;

pub const PORT_CONTROL: u32 = 1;
pub const PORT_INPUT: u32 = 2;
pub const PORT_OUTPUT: u32 = 3;
const ES_TYPE_CONTROL: u32 = 1;
const ES_TYPE_VIDEO: u32 = 3;

const ACTION_ENABLE: u32 = 1;
const ACTION_DISABLE: u32 = 2;
const ACTION_FLUSH: u32 = 3;
const ACTION_CONNECT: u32 = 4;
const ACTION_DISCONNECT: u32 = 5;

pub const PARAM_SUPPORTED_ENCODINGS: u32 = 1;
pub const PARAM_ZERO_COPY: u32 = 4;
/// `MMAL_PARAMETER_CAMERA_INFO`
pub const PARAM_CAMERA_INFO: u32 = 0x1_001c;

pub const FLAG_EOS: u32 = 1 << 0;
pub const FLAG_FRAME_END: u32 = 1 << 2;
pub const FLAG_KEYFRAME: u32 = 1 << 3;
pub const TIME_UNKNOWN: i64 = i64::MIN;
/// the ISP's statistics (`MMAL_ENCODING_BRCM_STATS`)
pub const STATS: u32 = mmal_fourcc(b"STAT");
/// the format-changed event's command (`MMAL_EVENT_FORMAT_CHANGED`)
pub const EVENT_FORMAT_CHANGED: u32 = mmal_fourcc(b"EFCH");

/// Offsets in a `BUFFER_FROM_HOST` / `BUFFER_TO_HOST` message (header included).
pub mod buf {
    pub const DRV_COMPONENT: usize = 0x1c;
    pub const DRV_PORT: usize = 0x20;
    pub const CMD: usize = 0x40;
    pub const DATA: usize = 0x44;
    pub const ALLOC_SIZE: usize = 0x48;
    pub const LENGTH: usize = 0x4c;
    pub const OFFSET: usize = 0x50;
    pub const FLAGS: usize = 0x54;
    pub const PTS: usize = 0x58;
    pub const DTS: usize = 0x60;
    pub const IS_ZERO_COPY: usize = 0x98;
    pub const PAYLOAD_IN_MESSAGE: usize = 0xa0;
    pub const SIZE: usize = 0x128;
}

fn get32(d: &[u8], o: usize) -> u32 { d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap())) }
fn put32(d: &mut [u8], o: usize, v: u32) { if let Some(b) = d.get_mut(o..o + 4) { b.copy_from_slice(&v.to_le_bytes()); } }
fn get64(d: &[u8], o: usize) -> i64 { d.get(o..o + 8).map_or(0, |b| i64::from_le_bytes(b.try_into().unwrap())) }
fn put64(d: &mut [u8], o: usize, v: i64) { if let Some(b) = d.get_mut(o..o + 8) { b.copy_from_slice(&v.to_le_bytes()); } }
fn words(w: &[u32]) -> Vec<u8> { w.iter().flat_map(|x| x.to_le_bytes()).collect() }

/// The memory vc-sm-cma imported (`SMEM`): VideoCore handles for runs of guest RAM.
#[derive(Default)]
pub struct Smem {
    handles: Vec<(u32, u64, u32)>,
    next: u32,
}

const SM_FREE: u32 = 4;
const SM_IMPORT: u32 = 8;
const SM_CLIENT_VERSION: u32 = 9;

impl Smem {
    /// A message from vc-sm-cma: `type`, transaction id, body. Imports are answered with
    /// (transaction id, handle); frees and the client's version need no answer. The firmware's
    /// own version message is not sent, so the kernel releases buffers without waiting for
    /// the VideoCore.
    pub fn message(&mut self, port: u32, p: &[u8], out: &mut Outbox) {
        let (kind, trans) = (get32(p, 0), get32(p, 4));
        match kind {
            SM_IMPORT => {
                let (addr, size) = (get32(p, 12), get32(p, 16));
                self.next = self.next.wrapping_add(1).max(1);
                let h = self.next;
                self.handles.push((h, phys(addr), size));
                out.data(port, words(&[trans, h]));
            }
            SM_FREE => { let h = get32(p, 8); self.handles.retain(|e| e.0 != h); }
            SM_CLIENT_VERSION => {}
            _ => {}
        }
    }
    /// The RAM behind a handle: (physical address, size).
    pub fn lookup(&self, h: u32) -> Option<(u64, u32)> { self.handles.iter().find(|e| e.0 == h).map(|e| (e.1, e.2)) }
    pub fn len(&self) -> usize { self.handles.len() }
    pub fn is_empty(&self) -> bool { self.handles.is_empty() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind { Decode, Encode, Isp, ImageFx, ImageEncode, CameraInfo }

impl Kind {
    fn from_name(name: &str) -> Option<Kind> {
        Some(match name {
            "ril.video_decode" => Kind::Decode,
            "ril.video_encode" => Kind::Encode,
            "ril.isp" => Kind::Isp,
            "ril.image_fx" => Kind::ImageFx,
            "ril.image_encode" => Kind::ImageEncode,
            "camera_info" | "ril.camera_info" => Kind::CameraInfo,
            _ => return None,
        })
    }
    fn code(self) -> u8 { self as u8 }
    fn from_code(c: u8) -> Kind { [Kind::Decode, Kind::Encode, Kind::Isp, Kind::ImageFx, Kind::ImageEncode, Kind::CameraInfo][c as usize % 6] }
    /// (inputs, outputs) and the formats of each side.
    fn ports(self) -> (usize, usize, Vec<u32>, Vec<u32>) {
        let raw: Vec<u32> = video::YUV_FORMATS.iter().chain(video::RGB_FORMATS).copied().collect();
        let decoded = vec![video::I420, video::YV12, video::NV12, video::NV21, video::RGB16];
        let yuv420 = vec![video::I420, video::YV12, video::NV12, video::NV21];
        match self {
            Kind::Decode => (1, 1, vec![video::H264], decoded),
            Kind::Encode => (1, 1, raw, vec![video::H264]),
            Kind::Isp => (1, 3, raw.clone(), raw),
            Kind::ImageFx => (1, 1, yuv420.clone(), yuv420),
            Kind::ImageEncode => (1, 1, raw, vec![video::JPEG]),
            Kind::CameraInfo => (0, 0, Vec::new(), Vec::new()),
        }
    }
}

/// `es.video` of a port: width and height of the buffer (stride in pixels, rows), crop
/// rectangle, frame rate, pixel aspect ratio, colour space.
const ES_W: usize = 0;
const ES_H: usize = 1;
const ES_CROP_W: usize = 4;
const ES_CROP_H: usize = 5;
const ES_COLOUR: usize = 10;

#[derive(Clone)]
struct Port {
    ptype: u32,
    index: u32,
    handle: u32,
    enabled: bool,
    encoding: u32,
    variant: u32,
    bitrate: u32,
    flags: u32,
    es: [u32; 11],
    extradata: Vec<u8>,
    buffer_num: u32,
    buffer_size: u32,
    userdata: u32,
    zero_copy: bool,
    supported: Vec<u32>,
    params: Vec<(u32, Vec<u8>)>,
    /// buffers Linux gave us (their `BUFFER_FROM_HOST` messages)
    bufs: VecDeque<Vec<u8>>,
}

impl Port {
    fn new(ptype: u32, index: u32, handle: u32, supported: Vec<u32>) -> Port {
        Port { ptype, index, handle, enabled: false, encoding: supported.first().copied().unwrap_or(0), variant: 0, bitrate: 0, flags: 0, es: [0; 11],
               extradata: Vec::new(), buffer_num: 1, buffer_size: 0, userdata: 0, zero_copy: false, supported, params: Vec::new(), bufs: VecDeque::new() }
    }
    fn raw(&self) -> bool { video::frame_size(self.encoding, 16, 16).is_some() }
    /// What one buffer must hold: a whole picture for raw formats, anything for streams.
    fn size_min(&self) -> u32 { if self.raw() { video::frame_size(self.encoding, self.es[ES_W], self.es[ES_H]).unwrap_or(0) } else { 0 } }
    /// The picture the port carries: (stride, rows, visible width, visible height).
    fn geometry(&self) -> (u32, u32, u32, u32) {
        let (w, h) = (self.es[ES_W], self.es[ES_H]);
        let (cw, ch) = (self.es[ES_CROP_W], self.es[ES_CROP_H]);
        (w, h, if cw == 0 || cw > w { w } else { cw }, if ch == 0 || ch > h { h } else { ch })
    }
    /// `struct mmal_port` (64 bytes).
    fn bytes(&self, component: u32) -> Vec<u8> {
        let mut v = words(&[0, 0, self.ptype]);
        v.extend_from_slice(&(self.index as u16).to_le_bytes());
        v.extend_from_slice(&(self.index as u16).to_le_bytes());
        let min = self.size_min();
        v.extend(words(&[self.enabled as u32, 0, 1, min, 16, 3, min, self.buffer_num, self.buffer_size.max(min), component, self.userdata, 0]));
        v
    }
    /// `struct mmal_es_format` (32 bytes) + `es` (44) + extradata (128).
    fn format_bytes(&self) -> Vec<u8> {
        let es_type = if self.ptype == PORT_CONTROL { ES_TYPE_CONTROL } else { ES_TYPE_VIDEO };
        let mut v = words(&[es_type, self.encoding, self.variant, 0, self.bitrate, self.flags, self.extradata.len() as u32, 0]);
        v.extend(words(&self.es));
        let mut x = self.extradata.clone();
        x.resize(128, 0);
        v.extend(x);
        v
    }
    fn param(&self, id: u32) -> Option<&Vec<u8>> { self.params.iter().find(|p| p.0 == id).map(|p| &p.1) }
}

struct Component {
    handle: u32,
    /// the VCHIQ port (ours) of the connection that created it, and Linux's index for it
    owner: u32,
    client: u32,
    kind: Kind,
    enabled: bool,
    /// control port, inputs, outputs
    ports: Vec<Port>,
    inputs: usize,
    decoder: Option<Box<dyn VideoDecoder>>,
    /// decoded pictures waiting for an output buffer
    frames: VecDeque<Picture>,
    /// the output format last announced with a format-changed event (coded and visible size)
    announced: Option<(u32, u32, u32, u32)>,
    /// end of stream seen on the input, to pass on once the pictures before it are out
    eos: bool,
    /// input buffers whose pictures are not all out yet (the one with the end of stream until
    /// that has gone out too): a client polling the codec sees the decoder busy rather than
    /// idle with nothing to show while it sets up for a new format
    held: VecDeque<Vec<u8>>,
}

impl Component {
    fn port_mut(&mut self, handle: u32) -> Option<&mut Port> { self.ports.iter_mut().find(|p| p.handle == handle) }
    fn port_index(&self, ptype: u32, index: u32) -> Option<usize> { self.ports.iter().position(|p| p.ptype == ptype && p.index == index) }
}

pub struct Mmal {
    components: Vec<Component>,
    next_handle: u32,
    pub smem: Smem,
    /// pictures decoded, buffers returned (diagnostics)
    pub pictures: u64,
    pub buffers: u64,
    pub trace: bool,
    /// where `ril.video_decode` gets its decoder (`video::decoder_for`; the tests pin the stand-in)
    pub decoders: fn(u32) -> Option<Box<dyn VideoDecoder>>,
}

impl Default for Mmal { fn default() -> Self { Self::new() } }

/// A reply to `req`: its header with status 0, then `body`.
fn reply(req: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = req[..HDR.min(req.len())].to_vec();
    v.resize(HDR, 0);
    put32(&mut v, 16, 0);
    v.extend_from_slice(body);
    v
}

impl Mmal {
    pub fn new() -> Self {
        Mmal { components: Vec::new(), next_handle: 1, smem: Smem::default(), pictures: 0, buffers: 0, trace: std::env::var_os("SBC_MMAL_TRACE").is_some(), decoders: video::decoder_for }
    }

    /// The connection on `port` closed: its components go.
    pub fn closed(&mut self, port: u32) { self.components.retain(|c| c.owner != port); }

    /// Diagnostics: the components alive, by kind.
    pub fn component_kinds(&self) -> Vec<Kind> { self.components.iter().map(|c| c.kind).collect() }

    /// Nothing runs later than the message that made it possible; kept for components that
    /// will work in the background (a decoder on its own thread).
    pub fn tick(&mut self, _ram: &mut [u8], _now: u64, _out: &mut Outbox) {}

    pub fn message(&mut self, port: u32, m: &[u8], ram: &mut [u8], _now: u64, out: &mut Outbox) {
        if m.len() < HDR || get32(m, 0) != MAGIC { return; }
        let t = get32(m, 4);
        let b = &m[HDR..];
        let w = |k: usize| get32(b, 4 * k);
        if self.trace && t != BUFFER_FROM_HOST { eprintln!("[mmal] port {port}: message {t} {:08x?}", &b.chunks(4).take(6).map(|c| get32(c, 0)).collect::<Vec<_>>()); }
        match t {
            GET_VERSION => out.data(port, reply(m, &words(&[0, 15, 0, 10]))),
            COMPONENT_CREATE => {
                let name: String = b[4..132.min(b.len())].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
                match Kind::from_name(&name) {
                    Some(kind) => {
                        let handle = self.next_handle;
                        self.next_handle += 1;
                        let (ni, no, fin, fout) = kind.ports();
                        let mut ports = vec![Port::new(PORT_CONTROL, 0, handle << 4, Vec::new())];
                        for k in 0..ni { ports.push(Port::new(PORT_INPUT, k as u32, handle << 4 | (1 + k as u32), fin.clone())); }
                        for k in 0..no {
                            // the ISP's third output carries statistics, not pictures
                            let f = if kind == Kind::Isp && k == 2 { vec![STATS] } else { fout.clone() };
                            ports.push(Port::new(PORT_OUTPUT, k as u32, handle << 4 | (1 + ni + k) as u32, f));
                        }
                        for p in ports.iter_mut().filter(|p| p.ptype != PORT_CONTROL) { p.es[ES_COLOUR] = video::BT601; p.es[8] = 1; p.es[9] = 1; }
                        // a new component moves buffers as soon as its ports are enabled: Linux's
                        // mmal-vchiq reuses its component slots without clearing their `enabled`
                        // flag, so the second codec session after a boot never sends
                        // COMPONENT_ENABLE (and works on a real Pi all the same)
                        self.components.push(Component { handle, owner: port, client: w(0), kind, enabled: true, ports, inputs: ni, decoder: None, frames: VecDeque::new(), announced: None, eos: false, held: VecDeque::new() });
                        if self.trace { eprintln!("[mmal] {name}: component {handle}"); }
                        out.data(port, reply(m, &words(&[ST_SUCCESS, handle, ni as u32, no as u32, 0])));
                    }
                    None => {
                        if self.trace { eprintln!("[mmal] {name}: no such component"); }
                        out.data(port, reply(m, &words(&[ST_ENOENT, 0, 0, 0, 0])));
                    }
                }
            }
            COMPONENT_DESTROY => {
                let n = self.components.len();
                self.components.retain(|c| c.handle != w(0));
                out.data(port, reply(m, &words(&[if self.components.len() < n { ST_SUCCESS } else { ST_EINVAL }])));
            }
            COMPONENT_ENABLE | COMPONENT_DISABLE => {
                let Some(k) = self.components.iter().position(|c| c.handle == w(0)) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                self.components[k].enabled = t == COMPONENT_ENABLE;
                out.data(port, reply(m, &words(&[ST_SUCCESS])));
                self.run(k, ram, out);
            }
            PORT_INFO_GET => {
                let (ch, ptype, index) = (w(0), w(1), w(2));
                let found = self.components.iter().find(|c| c.handle == ch).and_then(|c| c.port_index(ptype, index).map(|i| (c, &c.ports[i])));
                match found {
                    Some((c, p)) => {
                        let mut body = words(&[ST_SUCCESS, ch, ptype, index, 1, p.handle]);
                        body.extend(p.bytes(c.handle));
                        body.extend(p.format_bytes());
                        out.data(port, reply(m, &body));
                    }
                    None => out.data(port, reply(m, &words(&[ST_EINVAL, ch, ptype, index, 0, 0]))),
                }
            }
            PORT_INFO_SET => {
                let (ch, ptype, index) = (w(0), w(1), w(2));
                let Some(k) = self.components.iter().position(|c| c.handle == ch) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                let Some(i) = self.components[k].port_index(ptype, index) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                let p = &mut self.components[k].ports[i];
                // the port struct at 12, the format at 76, es at 108, extradata at 152
                let enc = get32(b, 76 + 4);
                let status = if enc != 0 && !p.supported.is_empty() && !p.supported.contains(&enc) { ST_EINVAL } else {
                    if enc != 0 { p.encoding = enc; }
                    p.buffer_num = get32(b, 12 + 44).max(1);
                    p.buffer_size = get32(b, 12 + 48);
                    p.userdata = get32(b, 12 + 56);
                    p.variant = get32(b, 76 + 8);
                    p.bitrate = get32(b, 76 + 16);
                    p.flags = get32(b, 76 + 20);
                    for (j, e) in p.es.iter_mut().enumerate() { *e = get32(b, 108 + 4 * j); }
                    let xn = (get32(b, 76 + 24) as usize).min(128);
                    p.extradata = b.get(152..152 + xn).map_or(Vec::new(), |x| x.to_vec());
                    ST_SUCCESS
                };
                if self.trace { eprintln!("[mmal] component {ch} port {ptype}/{index}: {} {:?} -> status {status}", crate::vchiq::fourcc_str(enc.swap_bytes()), &p.es[..6]); }
                let mut body = words(&[status, ch, ptype, index, 1, p.handle]);
                let c = self.components[k].handle;
                let p = &self.components[k].ports[i];
                body.extend(p.bytes(c));
                body.extend(p.format_bytes());
                out.data(port, reply(m, &body));
                self.run(k, ram, out);
            }
            PORT_ACTION => {
                let (ch, ph, action) = (w(0), w(1), w(2));
                let Some(k) = self.components.iter().position(|c| c.handle == ch) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                let Some(i) = self.components[k].ports.iter().position(|p| p.handle == ph) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                let status = match action {
                    ACTION_ENABLE => {
                        let p = &mut self.components[k].ports[i];
                        p.enabled = true;
                        let (num, size) = (get32(b, 12 + 44), get32(b, 12 + 48));
                        if num != 0 { p.buffer_num = num; }
                        if size != 0 { p.buffer_size = size; }
                        ST_SUCCESS
                    }
                    ACTION_DISABLE | ACTION_FLUSH => {
                        // every buffer goes back, empty, before the answer
                        let c = &mut self.components[k];
                        if action == ACTION_DISABLE { c.ports[i].enabled = false; }
                        let mut bufs: Vec<Vec<u8>> = c.ports[i].bufs.drain(..).collect();
                        if c.ports[i].ptype == PORT_INPUT {
                            bufs.extend(c.held.drain(..));
                            if let Some(d) = c.decoder.as_mut() { d.reset(); }
                            c.frames.clear();
                            c.eos = false;
                            c.announced = None;
                        }
                        for bf in bufs { self.buffer_to_host(k, i, bf, 0, 0, None, ram, out, &[]); }
                        ST_SUCCESS
                    }
                    ACTION_CONNECT => ST_ENOSYS,
                    ACTION_DISCONNECT => ST_SUCCESS,
                    _ => ST_EINVAL,
                };
                if self.trace { eprintln!("[mmal] component {ch} port {ph:#x}: action {action} -> {status}"); }
                out.data(port, reply(m, &words(&[status])));
                self.run(k, ram, out);
            }
            BUFFER_FROM_HOST => {
                let (ch, ph) = (get32(m, buf::DRV_COMPONENT), get32(m, buf::DRV_PORT));
                let found = self.components.iter().position(|c| c.handle == ch).and_then(|k| Some((k, self.components[k].ports.iter().position(|p| p.handle == ph)?)));
                if self.trace { eprintln!("[mmal] buffer for component {ch} port {ph:#x}: {}", if found.is_some() { "queued" } else { "no such port" }); }
                let Some((k, i)) = found else { return };
                let mut msg = m.to_vec();
                msg.resize(buf::SIZE, 0);
                self.components[k].ports[i].bufs.push_back(msg);
                self.run(k, ram, out);
            }
            PORT_PARAMETER_SET => {
                let (ch, ph, id, size) = (w(0), w(1), w(2), w(3));
                let value = b.get(16..16 + (size as usize).saturating_sub(8)).unwrap_or(&[]).to_vec();
                let Some(p) = self.components.iter_mut().find(|c| c.handle == ch).and_then(|c| c.port_mut(ph)) else { out.data(port, reply(m, &words(&[ST_EINVAL]))); return };
                if id == PARAM_ZERO_COPY { p.zero_copy = get32(&value, 0) != 0; }
                p.params.retain(|e| e.0 != id);
                p.params.push((id, value));
                out.data(port, reply(m, &words(&[ST_SUCCESS])));
            }
            PORT_PARAMETER_GET => {
                let (ch, ph, id, size) = (w(0), w(1), w(2), w(3));
                let room = (size as usize).saturating_sub(8);
                let comp = self.components.iter().find(|c| c.handle == ch);
                let Some((c, p)) = comp.and_then(|c| c.ports.iter().find(|p| p.handle == ph).map(|p| (c, p))) else { out.data(port, reply(m, &words(&[ST_EINVAL, id, 8]))); return };
                let value: Option<Vec<u8>> = match id {
                    PARAM_SUPPORTED_ENCODINGS => Some(words(&p.supported)),
                    PARAM_ZERO_COPY => Some(words(&[p.zero_copy as u32])),
                    // no camera: zero cameras, zero flashes
                    PARAM_CAMERA_INFO if c.kind == Kind::CameraInfo => Some(vec![0; room]),
                    _ => p.param(id).cloned(),
                };
                let body = match value {
                    Some(v) => {
                        let status = if v.len() > room { ST_ENOSPC } else { ST_SUCCESS };
                        let mut body = words(&[status, id, 8 + v.len() as u32]);
                        body.extend_from_slice(&v[..v.len().min(96 * 4)]);
                        body
                    }
                    None => words(&[ST_ENOSYS, id, 8]),
                };
                out.data(port, reply(m, &body));
            }
            _ => {
                let mut r = reply(m, &words(&[ST_ENOSYS]));
                put32(&mut r, 16, ST_ENOSYS);
                out.data(port, r);
            }
        }
    }

    /// Where a buffer's data lives: (physical address, usable bytes). Buffers pass by vc-sm-cma
    /// handle when the port is zero-copy (bcm2835-codec and bcm2835-isp always are).
    fn buffer_mem(&self, k: usize, i: usize, b: &[u8]) -> Option<(u64, usize)> {
        let p = &self.components[k].ports[i];
        let data = get32(b, buf::DATA);
        let (a, size) = self.smem.lookup(data).filter(|_| p.zero_copy || data < 0x1_0000)?;
        let alloc = get32(b, buf::ALLOC_SIZE).min(size);
        Some((a, alloc as usize))
    }

    /// Hand a buffer back to Linux with `length` bytes of `flags` data; the data itself is in
    /// the buffer's memory already (zero copy) or follows as a bulk transfer (`bulk`).
    #[allow(clippy::too_many_arguments)]
    fn buffer_to_host(&mut self, k: usize, i: usize, mut b: Vec<u8>, length: u32, flags: u32, pts: Option<i64>, ram: &mut [u8], out: &mut Outbox, bulk: &[u8]) {
        let _ = ram;
        // a zero-copy port's buffers come back zero-copy even when they carry no handle (the
        // codec's own end-of-stream buffer): Linux must not wait for bulk data
        let zero_copy = self.components[k].ports[i].zero_copy || self.buffer_mem(k, i, &b).is_some();
        put32(&mut b, 4, BUFFER_TO_HOST);
        put32(&mut b, 16, 0);
        put32(&mut b, buf::CMD, 0);
        put32(&mut b, buf::LENGTH, length);
        put32(&mut b, buf::OFFSET, 0);
        put32(&mut b, buf::FLAGS, flags);
        if let Some(t) = pts { put64(&mut b, buf::PTS, t); put64(&mut b, buf::DTS, t); }
        else if self.components[k].ports[i].ptype == PORT_OUTPUT { put64(&mut b, buf::PTS, TIME_UNKNOWN); put64(&mut b, buf::DTS, TIME_UNKNOWN); }
        put32(&mut b, buf::IS_ZERO_COPY, zero_copy as u32);
        put32(&mut b, buf::PAYLOAD_IN_MESSAGE, 0);
        let owner = self.components[k].owner;
        out.data(owner, b);
        if !zero_copy && length > 0 {
            // Linux follows with a bulk receive of the length rounded up to 4 bytes
            let mut d = bulk.to_vec();
            d.resize((length as usize + 3) & !3, 0);
            out.bulk(owner, d);
        }
        self.buffers += 1;
    }

    /// An input buffer's data.
    fn input_data(&self, k: usize, i: usize, b: &[u8], ram: &[u8]) -> Vec<u8> {
        let (len, off) = (get32(b, buf::LENGTH) as usize, get32(b, buf::OFFSET) as usize);
        match self.buffer_mem(k, i, b) {
            Some((a, size)) => read_bytes(ram, a + off as u64, len.min(size.saturating_sub(off))),
            None => Vec::new(),
        }
    }

    /// Write `data` (a picture already laid out) into an output buffer and return it.
    fn emit(&mut self, k: usize, i: usize, b: Vec<u8>, data: &[u8], flags: u32, pts: Option<i64>, ram: &mut [u8], out: &mut Outbox) {
        let n = match self.buffer_mem(k, i, &b) {
            Some((a, size)) => { let n = data.len().min(size); write_bytes(ram, a, &data[..n]); n }
            None => data.len().min(get32(&b, buf::ALLOC_SIZE) as usize),
        };
        self.buffer_to_host(k, i, b, n as u32, flags, pts, ram, out, &data[..n]);
    }

    /// Move data through component `k` as far as its buffers allow.
    fn run(&mut self, k: usize, ram: &mut [u8], out: &mut Outbox) {
        if !self.components[k].enabled { return; }
        match self.components[k].kind {
            Kind::Decode => self.run_decoder(k, ram, out),
            Kind::Isp | Kind::ImageFx => self.run_isp(k, ram, out),
            Kind::Encode | Kind::ImageEncode => self.run_encoder(k, ram, out),
            Kind::CameraInfo => {}
        }
    }

    fn run_decoder(&mut self, k: usize, ram: &mut [u8], out: &mut Outbox) {
        let inp = 1;
        while self.components[k].ports[inp].enabled {
            let Some(b) = self.components[k].ports[inp].bufs.pop_front() else { break };
            let data = self.input_data(k, inp, &b, ram);
            let flags = get32(&b, buf::FLAGS);
            let pts = Some(get64(&b, buf::PTS)).filter(|&t| t != TIME_UNKNOWN);
            let trace = self.trace;
            let c = &mut self.components[k];
            if c.decoder.is_none() { c.decoder = (self.decoders)(c.ports[inp].encoding); }
            if let Some(d) = c.decoder.as_mut() {
                if !data.is_empty() { d.push(&data, pts); }
                if flags & FLAG_EOS != 0 { d.flush(); }
                let before = c.frames.len();
                while let Some(p) = d.pop() { c.frames.push_back(p); }
                if trace { eprintln!("[mmal] component {}: {} bytes in (flags {flags:#x}, pts {pts:?}), {} pictures out", c.handle, data.len(), c.frames.len() - before); }
            }
            if flags & FLAG_EOS != 0 { c.eos = true; }
            // the buffer comes back once what it gave is out (after the format-changed event,
            // as the firmware does)
            c.held.push_back(b);
            self.decoder_output(k, ram, out);
        }
        self.decoder_output(k, ram, out);
    }

    /// The format-changed event for pictures of `w` x `h` (`visible` of it shown), unless that
    /// is the format announced last: the output port then has it.
    fn announce(&mut self, k: usize, w: u32, h: u32, visible: (u32, u32), out: &mut Outbox) {
        let outp = 2;
        let c = &mut self.components[k];
        let coded = ((w + 31) & !31, h);
        let key = (coded.0, coded.1, visible.0, visible.1);
        if c.announced == Some(key) { return; }
        c.announced = Some(key);
        let enc = c.ports[outp].encoding;
        let po = &mut c.ports[outp];
        po.es[ES_W] = coded.0;
        po.es[ES_H] = coded.1;
        po.es[2] = 0;
        po.es[3] = 0;
        po.es[ES_CROP_W] = visible.0;
        po.es[ES_CROP_H] = visible.1;
        let size = video::frame_size(enc, coded.0, coded.1).unwrap_or(0);
        let mut ev = words(&[size, 1, size, 3, 0]);
        ev.extend(po.format_bytes());
        let (client, owner) = (c.client, c.owner);
        out.data(owner, event(client, PORT_OUTPUT, 0, EVENT_FORMAT_CHANGED, &ev));
    }

    /// Decoded pictures into the output port's buffers; the format-changed event first when the
    /// stream's size is new; the end of stream once every picture before it is out.
    fn decoder_output(&mut self, k: usize, ram: &mut [u8], out: &mut Outbox) {
        let outp = 2;
        loop {
            let c = &mut self.components[k];
            let Some(p) = c.frames.front() else { break };
            let (w, h, visible) = (p.width, p.height, p.visible);
            self.announce(k, w, h, visible, out);
            let c = &self.components[k];
            let p = c.frames.front().unwrap();
            let po = &c.ports[outp];
            let (stride, rows, _, _) = po.geometry();
            if !po.enabled || stride < p.visible.0 || rows < p.visible.1 { break; }
            let Some(front) = po.bufs.front() else { break };
            let Some(size) = video::frame_size(po.encoding, stride, rows) else { break };
            // a buffer too small for the new format waits for Linux to reallocate (after the event)
            let room = self.buffer_mem(k, outp, front).map_or(get32(front, buf::ALLOC_SIZE) as usize, |m| m.1);
            if room < size as usize { break; }
            let c = &mut self.components[k];
            let b = c.ports[outp].bufs.pop_front().unwrap();
            let p = c.frames.pop_front().unwrap();
            let mut data = vec![0u8; size as usize];
            let enc = c.ports[outp].encoding;
            p.write(enc, stride, rows, &mut data);
            let flags = FLAG_FRAME_END | if p.keyframe { FLAG_KEYFRAME } else { 0 };
            self.pictures += 1;
            self.emit(k, outp, b, &data, flags, p.pts, ram, out);
        }
        // the pictures to come: their format as soon as the decoder knows it
        let c = &self.components[k];
        if let (true, Some((w, h, visible))) = (c.frames.is_empty(), c.decoder.as_ref().and_then(|d| d.format())) {
            self.announce(k, w, h, visible, out);
        }
        let c = &mut self.components[k];
        if c.eos && c.frames.is_empty() && c.ports[outp].enabled {
            if let Some(b) = c.ports[outp].bufs.pop_front() {
                c.eos = false;
                self.buffer_to_host(k, outp, b, 0, FLAG_EOS, None, ram, out, &[]);
            }
        }
        // input buffers whose pictures are all out go back; one with the end of stream once
        // that has been passed on
        let c = &mut self.components[k];
        if c.frames.is_empty() {
            let mut back = Vec::new();
            while let Some(b) = c.held.front() {
                if get32(b, buf::FLAGS) & FLAG_EOS != 0 && c.eos { break; }
                back.push(c.held.pop_front().unwrap());
            }
            for b in back {
                let (f, pts) = (get32(&b, buf::FLAGS) & FLAG_EOS, Some(get64(&b, buf::PTS)).filter(|&t| t != TIME_UNKNOWN));
                self.buffer_to_host(k, 1, b, 0, f, pts, ram, out, &[]);
            }
        }
    }

    fn run_isp(&mut self, k: usize, ram: &mut [u8], out: &mut Outbox) {
        let inp = 1;
        let first_out = 1 + self.components[k].inputs;
        loop {
            let c = &self.components[k];
            let ready = c.ports[inp].enabled && !c.ports[inp].bufs.is_empty() && c.ports[first_out].enabled && !c.ports[first_out].bufs.is_empty();
            if !ready { break; }
            let b = self.components[k].ports[inp].bufs.pop_front().unwrap();
            let flags = get32(&b, buf::FLAGS);
            let pts = Some(get64(&b, buf::PTS)).filter(|&t| t != TIME_UNKNOWN);
            let data = self.input_data(k, inp, &b, ram);
            let pi = &self.components[k].ports[inp];
            let (stride, rows, vw, vh) = pi.geometry();
            let img = if data.is_empty() { None } else { Image::read(pi.encoding, stride, rows, vw, vh, &data) };
            for o in first_out..self.components[k].ports.len() {
                let po = &self.components[k].ports[o];
                if !po.enabled || po.bufs.is_empty() { continue; }
                let b = self.components[k].ports[o].bufs.pop_front().unwrap();
                let po = &self.components[k].ports[o];
                let (ostride, orows, ow, oh) = po.geometry();
                let pic = img.as_ref().filter(|_| po.encoding != STATS).and_then(|im| {
                    let size = video::frame_size(po.encoding, ostride, orows)?;
                    let mut d = vec![0u8; size as usize];
                    im.scale(ow, oh).write(po.encoding, ostride, orows, &mut d)?;
                    Some(d)
                });
                let f = (flags & FLAG_EOS) | if pic.is_some() { FLAG_FRAME_END } else { 0 };
                match pic {
                    Some(d) => self.emit(k, o, b, &d, f, pts, ram, out),
                    None => self.buffer_to_host(k, o, b, 0, f, pts, ram, out, &[]),
                }
            }
            self.buffer_to_host(k, inp, b, 0, flags & FLAG_EOS, pts, ram, out, &[]);
        }
    }

    fn run_encoder(&mut self, k: usize, ram: &mut [u8], out: &mut Outbox) {
        let (inp, outp) = (1, 2);
        loop {
            let c = &self.components[k];
            if !(c.ports[inp].enabled && !c.ports[inp].bufs.is_empty() && c.ports[outp].enabled && !c.ports[outp].bufs.is_empty()) { break; }
            let b = self.components[k].ports[inp].bufs.pop_front().unwrap();
            let o = self.components[k].ports[outp].bufs.pop_front().unwrap();
            let flags = get32(&b, buf::FLAGS);
            let pts = Some(get64(&b, buf::PTS)).filter(|&t| t != TIME_UNKNOWN);
            self.buffer_to_host(k, outp, o, 0, FLAG_FRAME_END | (flags & FLAG_EOS), pts, ram, out, &[]);
            self.buffer_to_host(k, inp, b, 0, flags & FLAG_EOS, pts, ram, out, &[]);
        }
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("mmal-v1");
        w.u32(self.next_handle);
        w.u32(self.smem.next);
        w.u32(self.smem.handles.len() as u32);
        for (h, a, n) in &self.smem.handles { w.u32(*h); w.u64(*a); w.u32(*n); }
        w.u32(self.components.len() as u32);
        for c in &self.components {
            w.u32(c.handle); w.u32(c.owner); w.u32(c.client); w.u8(c.kind.code()); w.bool(c.enabled); w.u32(c.inputs as u32);
            w.bool(c.eos);
            w.u32(c.held.len() as u32);
            for b in &c.held { w.bytes(b); }
            w.bool(c.announced.is_some());
            let a = c.announced.unwrap_or_default();
            for v in [a.0, a.1, a.2, a.3] { w.u32(v); }
            w.u32(c.ports.len() as u32);
            for p in &c.ports {
                for v in [p.ptype, p.index, p.handle, p.encoding, p.variant, p.bitrate, p.flags, p.buffer_num, p.buffer_size, p.userdata] { w.u32(v); }
                w.bool(p.enabled); w.bool(p.zero_copy);
                for v in p.es { w.u32(v); }
                w.bytes(&p.extradata);
                w.bytes(&words(&p.supported));
                w.u32(p.params.len() as u32);
                for (id, v) in &p.params { w.u32(*id); w.bytes(v); }
                w.u32(p.bufs.len() as u32);
                for b in &p.bufs { w.bytes(b); }
            }
        }
    }

    /// Decoders restart from the next parameter sets after a restore (their state is not saved).
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("mmal-v1")?;
        self.next_handle = r.u32()?;
        self.smem.next = r.u32()?;
        self.smem.handles.clear();
        for _ in 0..r.u32()? { let h = r.u32()?; let a = r.u64()?; self.smem.handles.push((h, a, r.u32()?)); }
        self.components.clear();
        for _ in 0..r.u32()? {
            let (handle, owner, client) = (r.u32()?, r.u32()?, r.u32()?);
            let kind = Kind::from_code(r.u8()?);
            let enabled = r.bool()?;
            let inputs = r.u32()? as usize;
            let eos = r.bool()?;
            let mut held = VecDeque::new();
            for _ in 0..r.u32()? { held.push_back(r.bytes()?.to_vec()); }
            let has = r.bool()?;
            let a = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
            let mut ports = Vec::new();
            for _ in 0..r.u32()? {
                let mut v = [0u32; 10];
                for x in v.iter_mut() { *x = r.u32()?; }
                let mut p = Port::new(v[0], v[1], v[2], Vec::new());
                (p.encoding, p.variant, p.bitrate, p.flags, p.buffer_num, p.buffer_size, p.userdata) = (v[3], v[4], v[5], v[6], v[7], v[8], v[9]);
                p.enabled = r.bool()?; p.zero_copy = r.bool()?;
                for e in p.es.iter_mut() { *e = r.u32()?; }
                p.extradata = r.bytes()?.to_vec();
                p.supported = r.bytes()?.chunks(4).map(|c| get32(c, 0)).collect();
                for _ in 0..r.u32()? { let id = r.u32()?; p.params.push((id, r.bytes()?.to_vec())); }
                for _ in 0..r.u32()? { p.bufs.push_back(r.bytes()?.to_vec()); }
                ports.push(p);
            }
            self.components.push(Component { handle, owner, client, kind, enabled, ports, inputs, decoder: None, frames: VecDeque::new(), announced: has.then_some(a), eos, held });
        }
        Ok(())
    }
}

/// An `EVENT_TO_HOST` message for Linux's component `client`, port (`ptype`, `num`).
fn event(client: u32, ptype: u32, num: u32, cmd: u32, data: &[u8]) -> Vec<u8> {
    let mut m = words(&[MAGIC, EVENT_TO_HOST, 0, 0, 0, 0, client, ptype, num, cmd, data.len() as u32]);
    let mut d = data.to_vec();
    d.resize(256, 0);
    m.extend(d);
    m.extend(words(&[0]));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vchiq::Out;

    /// A Linux-side MMAL client: messages as `mmal-vchiq.c` builds them.
    struct Client { mmal: Mmal, ram: Vec<u8>, ctx: u32 }

    impl Client {
        fn new() -> Client {
            // these tests are about the plumbing: the stand-in's one picture per coded picture,
            // with or without the h264 feature
            let mut mmal = Mmal::new();
            mmal.decoders = |e| (e == video::H264).then(|| Box::new(video::TestPatternDecoder::default()) as Box<dyn VideoDecoder>);
            Client { mmal, ram: vec![0; 16 << 20], ctx: 0 }
        }
        fn msg(&mut self, t: u32, body: &[u8]) -> Vec<u8> {
            self.ctx += 1;
            let mut m = words(&[MAGIC, t, 0, self.ctx, 0, 0]);
            m.extend_from_slice(body);
            m
        }
        fn call(&mut self, t: u32, body: &[u8]) -> Vec<Out> {
            let m = self.msg(t, body);
            let mut out = Outbox::default();
            self.mmal.message(1, &m, &mut self.ram, 0, &mut out);
            out.0.into_iter().map(|(p, o)| { assert_eq!(p, 1); o }).collect()
        }
        /// A synchronous call: the one reply's body.
        fn sync(&mut self, t: u32, body: &[u8]) -> Vec<u8> {
            let outs = self.call(t, body);
            let Some(Out::Data(r)) = outs.into_iter().last() else { panic!("no reply") };
            assert_eq!(get32(&r, 4), t);
            assert_eq!(get32(&r, 12), self.ctx, "the context comes back");
            r[HDR..].to_vec()
        }
        fn create(&mut self, name: &str) -> (u32, u32, u32) {
            let mut b = words(&[7]);
            let mut n = name.as_bytes().to_vec();
            n.resize(128, 0);
            b.extend(n);
            b.extend(words(&[0]));
            let r = self.sync(COMPONENT_CREATE, &b);
            assert_eq!(get32(&r, 0), ST_SUCCESS, "{name}");
            (get32(&r, 4), get32(&r, 8), get32(&r, 12))
        }
        fn port(&mut self, c: u32, ptype: u32, index: u32) -> Vec<u8> {
            let r = self.sync(PORT_INFO_GET, &words(&[c, ptype, index]));
            assert_eq!(get32(&r, 0), ST_SUCCESS);
            r
        }
        /// PORT_INFO_SET of a video format.
        fn set_format(&mut self, c: u32, ptype: u32, index: u32, enc: u32, w: u32, h: u32, size: u32) -> u32 {
            let mut b = words(&[c, ptype, index]);
            let mut port = vec![0u8; 64];
            put32(&mut port, 44, 2);
            put32(&mut port, 48, size);
            b.extend(port);
            b.extend(words(&[3, enc, 0, 0, 0, 0, 0, 0]));
            b.extend(words(&[w, h, 0, 0, w, h, 30, 1, 1, 1, 0]));
            b.extend(vec![0u8; 128]);
            let r = self.sync(PORT_INFO_SET, &b);
            get32(&r, 0)
        }
        fn param_set(&mut self, c: u32, p: u32, id: u32, v: &[u8]) {
            let mut b = words(&[c, p, id, 8 + v.len() as u32]);
            b.extend_from_slice(v);
            assert_eq!(get32(&self.sync(PORT_PARAMETER_SET, &b), 0), ST_SUCCESS);
        }
        fn action(&mut self, c: u32, p: u32, a: u32) -> Vec<Out> {
            let mut b = words(&[c, p, a]);
            b.extend(vec![0u8; 64]);
            self.call(PORT_ACTION, &b)
        }
        /// Import `size` bytes at physical `addr` through SMEM; the VideoCore handle.
        fn import(&mut self, addr: u32, size: u32) -> u32 {
            let mut out = Outbox::default();
            let mut m = words(&[8, 99, 0, 0xc000_0000 | addr, size, 1, 0]);
            m.extend(vec![0u8; 32]);
            self.mmal.smem.message(2, &m, &mut out);
            let Out::Data(r) = &out.0[0].1 else { panic!() };
            assert_eq!(get32(r, 0), 99);
            get32(r, 4)
        }
        fn buffer(&mut self, c: u32, p: u32, handle: u32, alloc: u32, length: u32, flags: u32, pts: i64) -> Vec<Out> {
            let mut m = vec![0xbcu8; buf::SIZE];
            m[..HDR].copy_from_slice(&words(&[MAGIC, BUFFER_FROM_HOST, 0, 0, 0, 0]));
            m[0x18..0x28].copy_from_slice(&words(&[MAGIC, c, p, 1234]));
            put32(&mut m, buf::CMD, 0);
            put32(&mut m, buf::DATA, handle);
            put32(&mut m, buf::ALLOC_SIZE, alloc);
            put32(&mut m, buf::LENGTH, length);
            put32(&mut m, buf::OFFSET, 0);
            put32(&mut m, buf::FLAGS, flags);
            put64(&mut m, buf::PTS, pts);
            put32(&mut m, buf::PAYLOAD_IN_MESSAGE, 0);
            let mut out = Outbox::default();
            self.mmal.message(1, &m, &mut self.ram, 0, &mut out);
            out.0.into_iter().map(|(_, o)| o).collect()
        }
    }

    fn port_handle(r: &[u8]) -> u32 { get32(r, 20) }

    #[test]
    fn components_ports_and_encodings() {
        let mut c = Client::new();
        let (dec, ni, no) = c.create("ril.video_decode");
        assert_eq!((ni, no), (1, 1));
        let r = c.port(dec, PORT_INPUT, 0);
        assert_eq!(get32(&r, 24 + 64 + 4), video::H264, "input encoding");
        let ph = port_handle(&r);
        let rep = c.sync(PORT_PARAMETER_GET, &words(&[dec, ph, PARAM_SUPPORTED_ENCODINGS, 8 + 4 * 20]));
        assert_eq!(get32(&rep, 0), ST_SUCCESS);
        assert_eq!(get32(&rep, 8), 12, "one encoding: H.264 only (no licensed MPEG-2 / VC-1)");
        assert_eq!(c.create("ril.isp").2, 3);
        // no camera behind the firmware
        let (ci, _, _) = c.create("camera_info");
        let ctl = port_handle(&c.port(ci, PORT_CONTROL, 0));
        let rep = c.sync(PORT_PARAMETER_GET, &words(&[ci, ctl, PARAM_CAMERA_INFO, 8 + 72]));
        assert_eq!((get32(&rep, 0), get32(&rep, 12)), (ST_SUCCESS, 0), "num_cameras 0");
        let mut b = words(&[0]);
        b.extend(b"ril.camera".iter().copied().chain(std::iter::repeat(0)).take(128));
        b.extend(words(&[0]));
        assert_eq!(get32(&c.sync(COMPONENT_CREATE, &b), 0), ST_ENOENT);
        // an unsupported format is refused
        assert_eq!(c.set_format(dec, PORT_INPUT, 0, mmal_fourcc(b"WVC1"), 0, 0, 0), ST_EINVAL);
        // an unknown parameter: ENOSYS
        let rep = c.sync(PORT_PARAMETER_GET, &words(&[dec, ph, 0x2_002e, 12]));
        assert_eq!(get32(&rep, 0), ST_ENOSYS);
    }

    #[test]
    fn decode_through_the_stub_with_format_change_and_eos() {
        let mut c = Client::new();
        let (dec, _, _) = c.create("ril.video_decode");
        let pin = port_handle(&c.port(dec, PORT_INPUT, 0));
        let pout = port_handle(&c.port(dec, PORT_OUTPUT, 0));
        assert_eq!(c.set_format(dec, PORT_INPUT, 0, video::H264, 0, 0, 512 << 10), ST_SUCCESS);
        // the application's guess: 640x480 I420
        assert_eq!(c.set_format(dec, PORT_OUTPUT, 0, video::I420, 640, 480, 640 * 480 * 3 / 2), ST_SUCCESS);
        for p in [pin, pout] { c.param_set(dec, p, PARAM_ZERO_COPY, &words(&[1])); }
        assert!(matches!(c.sync(COMPONENT_ENABLE, &words(&[dec]))[..4], [0, 0, 0, 0]));
        c.action(dec, pin, ACTION_ENABLE);
        c.action(dec, pout, ACTION_ENABLE);
        let stream = video::tests::stream(320, 240, 2);
        let in_h = c.import(0x10_0000, 1 << 20);
        write_bytes(&mut c.ram, 0x10_0000, &stream);
        let out_h = c.import(0x40_0000, 1 << 20);
        c.buffer(dec, pout, out_h, 1 << 20, 0, 0, TIME_UNKNOWN);
        let outs = c.buffer(dec, pin, in_h, 1 << 20, stream.len() as u32, 0, 40_000);
        // the first picture announces 320x240 on the output port, and the buffers allocated for
        // 640x480 are large enough, so it is written at once; then the input comes back
        let msgs: Vec<&Vec<u8>> = outs.iter().map(|o| match o { Out::Data(d) => d, _ => panic!("zero copy: no bulk") }).collect();
        assert_eq!(get32(msgs[2], 4), BUFFER_TO_HOST);
        assert_eq!(get32(msgs[2], buf::DRV_PORT), pin);
        let ev = msgs[0];
        assert_eq!((get32(ev, 4), get32(ev, 24), get32(ev, 28), get32(ev, 36)), (EVENT_TO_HOST, 7, PORT_OUTPUT, EVENT_FORMAT_CHANGED));
        let d = &ev[44..];
        assert_eq!((get32(d, 0x14), get32(d, 0x18)), (3, video::I420), "video, the output port's encoding");
        assert_eq!((get32(d, 0x34), get32(d, 0x38), get32(d, 0x44), get32(d, 0x48)), (320, 240, 320, 240));
        assert_eq!(get32(d, 0), 320 * 240 * 3 / 2, "buffer_size_min");
        let pic = msgs[1];
        assert_eq!((get32(pic, buf::DRV_PORT), get32(pic, buf::LENGTH)), (pout, 320 * 240 * 3 / 2), "in the announced format");
        assert_ne!(get32(pic, buf::FLAGS) & FLAG_KEYFRAME, 0);
        assert_eq!(get32(pic, buf::IS_ZERO_COPY), 1);
        assert_eq!(get64(pic, buf::PTS), 40_000);
        // bars at the new 320-pixel stride: white at the left, black at the right
        assert_eq!(c.ram[0x40_0000 + 320 * 10 + 2], 180);
        assert_eq!(c.ram[0x40_0000 + 320 * 10 + 318], 16);
        // end of stream: the held-back picture, then an empty EOS buffer
        c.buffer(dec, pout, out_h, 1 << 20, 0, 0, TIME_UNKNOWN);
        c.buffer(dec, pout, out_h, 1 << 20, 0, 0, TIME_UNKNOWN);
        let outs = c.buffer(dec, pin, in_h, 1 << 20, 0, FLAG_EOS, TIME_UNKNOWN);
        let flags: Vec<(u32, u32)> = outs.iter().map(|o| match o { Out::Data(d) => (get32(d, buf::DRV_PORT), get32(d, buf::FLAGS)), _ => (0, 0) }).collect();
        assert_eq!(flags, vec![(pout, FLAG_FRAME_END), (pout, FLAG_EOS), (pin, FLAG_EOS)], "the end-of-stream input comes back after the output's");
        // an end of stream with no output buffer queued: its input buffer waits for one
        let outs = c.buffer(dec, pin, in_h, 1 << 20, 0, FLAG_EOS, TIME_UNKNOWN);
        assert!(outs.is_empty(), "held until the end of stream has gone out");
        let outs = c.buffer(dec, pout, out_h, 1 << 20, 0, 0, TIME_UNKNOWN);
        let ports: Vec<u32> = outs.iter().map(|o| match o { Out::Data(d) => get32(d, buf::DRV_PORT), _ => 0 }).collect();
        assert_eq!(ports, vec![pout, pin]);
        // disabling a port returns what it holds
        c.buffer(dec, pout, out_h, 1 << 20, 0, 0, TIME_UNKNOWN);
        let outs = c.action(dec, pout, ACTION_DISABLE);
        assert_eq!(outs.len(), 2, "the buffer, then the reply");
    }

    #[test]
    fn decoder_holds_input_until_its_pictures_are_out() {
        let mut c = Client::new();
        let (dec, _, _) = c.create("ril.video_decode");
        let pin = port_handle(&c.port(dec, PORT_INPUT, 0));
        let pout = port_handle(&c.port(dec, PORT_OUTPUT, 0));
        for p in [pin, pout] { c.param_set(dec, p, PARAM_ZERO_COPY, &words(&[1])); }
        c.sync(COMPONENT_ENABLE, &words(&[dec]));
        c.action(dec, pin, ACTION_ENABLE);
        c.action(dec, pout, ACTION_ENABLE);
        let stream = video::tests::stream(64, 64, 3);
        let in_h = c.import(0x10_0000, 1 << 16);
        write_bytes(&mut c.ram, 0x10_0000, &stream);
        // no output buffers yet (the client waits for the source change): only the event
        let outs = c.buffer(dec, pin, in_h, 1 << 16, stream.len() as u32, 0, 0);
        assert_eq!(outs.len(), 1);
        let Out::Data(ev) = &outs[0] else { panic!() };
        assert_eq!(get32(ev, 4), EVENT_TO_HOST);
        // two pictures out, then the input buffer
        let out_h = c.import(0x20_0000, 1 << 16);
        assert_eq!(c.buffer(dec, pout, out_h, 1 << 16, 0, 0, TIME_UNKNOWN).len(), 1);
        let outs = c.buffer(dec, pout, out_h, 1 << 16, 0, 0, TIME_UNKNOWN);
        let ports: Vec<u32> = outs.iter().map(|o| match o { Out::Data(d) => get32(d, buf::DRV_PORT), _ => 0 }).collect();
        assert_eq!(ports, vec![pout, pin]);
    }

    /// With the real decoder: the format is announced once the first picture starts, though
    /// reordering holds that picture back (as the firmware does; v4l2-ctl and the codec driver
    /// set up the capture side on that event, and would not after the end of stream).
    #[cfg(feature = "h264")]
    #[test]
    fn h264_announces_its_format_before_the_first_picture_is_out() {
        let mut c = Client::new();
        c.mmal.decoders = video::decoder_for;
        let (dec, _, _) = c.create("ril.video_decode");
        let pin = port_handle(&c.port(dec, PORT_INPUT, 0));
        let pout = port_handle(&c.port(dec, PORT_OUTPUT, 0));
        assert_eq!(c.set_format(dec, PORT_INPUT, 0, video::H264, 0, 0, 512 << 10), ST_SUCCESS);
        for p in [pin, pout] { c.param_set(dec, p, PARAM_ZERO_COPY, &words(&[1])); }
        c.action(dec, pin, ACTION_ENABLE);
        c.action(dec, pout, ACTION_ENABLE);
        // our 96x64 High-profile stream (B-pyramid): its first access unit alone
        let stream: &[u8] = include_bytes!("../tests/data/h264-high-96x64.264");
        let aud: Vec<usize> = (0..stream.len() - 4).filter(|&k| stream[k..k + 4] == [0, 0, 1, 9]).collect();
        let first = aud[1] - 1;
        let in_h = c.import(0x10_0000, 1 << 16);
        write_bytes(&mut c.ram, 0x10_0000, &stream[..first]);
        let outs = c.buffer(dec, pin, in_h, 1 << 16, first as u32, 0, 0);
        let msgs: Vec<&Vec<u8>> = outs.iter().map(|o| match o { Out::Data(d) => d, _ => panic!("zero copy") }).collect();
        assert_eq!(msgs.len(), 2, "the event and the input buffer back, no picture yet");
        let ev = msgs[0];
        assert_eq!((get32(ev, 4), get32(ev, 36)), (EVENT_TO_HOST, EVENT_FORMAT_CHANGED));
        let d = &ev[44..];
        assert_eq!((get32(d, 0x34), get32(d, 0x38), get32(d, 0x44), get32(d, 0x48)), (96, 64, 96, 64));
        assert_eq!(get32(msgs[1], buf::DRV_PORT), pin);
        // the rest and the end of stream: all 30 pictures, then the end of stream
        write_bytes(&mut c.ram, 0x10_0000, &stream[first..]);
        c.buffer(dec, pin, in_h, 1 << 16, (stream.len() - first) as u32, 0, 0);
        c.buffer(dec, pin, in_h, 1 << 16, 0, FLAG_EOS, TIME_UNKNOWN);
        let out_h = c.import(0x40_0000, 1 << 16);
        let (mut pictures, mut eos) = (0, false);
        for _ in 0..31 {
            for o in c.buffer(dec, pout, out_h, 1 << 16, 0, 0, TIME_UNKNOWN) {
                let Out::Data(m) = o else { continue };
                if get32(&m, 4) != BUFFER_TO_HOST || get32(&m, buf::DRV_PORT) != pout { continue; }
                if get32(&m, buf::LENGTH) == 96 * 64 * 3 / 2 { pictures += 1; }
                if get32(&m, buf::FLAGS) & FLAG_EOS != 0 { eos = true; }
            }
        }
        assert_eq!((pictures, eos), (30, true));
    }

    #[test]
    fn isp_converts_and_scales() {
        let mut c = Client::new();
        let (isp, _, _) = c.create("ril.isp");
        let pin = port_handle(&c.port(isp, PORT_INPUT, 0));
        let pout = port_handle(&c.port(isp, PORT_OUTPUT, 0));
        assert_eq!(c.set_format(isp, PORT_INPUT, 0, video::RGB24, 64, 32, 64 * 32 * 3), ST_SUCCESS);
        assert_eq!(c.set_format(isp, PORT_OUTPUT, 0, video::NV12, 32, 16, 32 * 16 * 3 / 2), ST_SUCCESS);
        for p in [pin, pout] { c.param_set(isp, p, PARAM_ZERO_COPY, &words(&[1])); }
        c.sync(COMPONENT_ENABLE, &words(&[isp]));
        c.action(isp, pin, ACTION_ENABLE);
        c.action(isp, pout, ACTION_ENABLE);
        // a red input picture
        let red: Vec<u8> = (0..64 * 32).flat_map(|_| [255u8, 0, 0]).collect();
        write_bytes(&mut c.ram, 0x10_0000, &red);
        let (hi, ho) = (c.import(0x10_0000, 64 * 32 * 3), c.import(0x20_0000, 32 * 16 * 3 / 2));
        c.buffer(isp, pout, ho, 32 * 16 * 3 / 2, 0, 0, TIME_UNKNOWN);
        let outs = c.buffer(isp, pin, hi, 64 * 32 * 3, 64 * 32 * 3, 0, 5);
        assert_eq!(outs.len(), 2);
        let Out::Data(o) = &outs[0] else { panic!() };
        assert_eq!(get32(o, buf::LENGTH), 32 * 16 * 3 / 2);
        // BT.601 red: Y 82, U 90, V 240
        assert_eq!(c.ram[0x20_0000], 82);
        assert_eq!(&c.ram[0x20_0000 + 32 * 16..0x20_0000 + 32 * 16 + 2], &[90, 240]);
    }
}
