//! OpenGL ES 3.2 demos on the screen, without a window system.
//!     nano-gl [DEMO...] [--mode WxH] [--seconds N] [--step S] [--shot FILE --time T]
//! --step S: each picture moves the demo on S seconds, whatever the time (for recordings); the demos are
//! then shown once each, --seconds long.
mod gl;
mod kms;
mod scenes;
mod sys;

use std::time::Instant;
use sys::*;

// rustc's own linker brings no C start-up code: this is all of it
std::arch::global_asm!(
    ".globl _start",
    ".type _start,%function",
    "_start:",
    "mov x29, #0",
    "mov x30, #0",
    "mov x5, x0",
    "ldr x1, [sp]",
    "add x2, sp, #8",
    "mov x6, sp",
    "adrp x0, main",
    "add x0, x0, :lo12:main",
    "mov x3, #0",
    "mov x4, #0",
    "bl __libc_start_main",
    "brk #0",
);

struct Options {
    demos: Vec<String>,
    mode: Option<(u32, u32)>,
    seconds: f32,
    shot: Option<String>,
    time: f32,
    step: Option<f32>,
}

fn options() -> Result<Options, String> {
    let mut o = Options { demos: Vec::new(), mode: None, seconds: 12.0, shot: None, time: 2.0, step: None };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or(format!("{a} what?"));
        match a.as_str() {
            "--mode" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--mode WxH")?;
                o.mode = Some((w.parse().map_err(|_| "--mode WxH")?, h.parse().map_err(|_| "--mode WxH")?)).filter(|&(w, h)| (1..=8192).contains(&w) && (1..=8192).contains(&h));
                o.mode.ok_or("--mode WxH, up to 8192")?;
            }
            "--seconds" => o.seconds = value()?.parse().map_err(|_| "--seconds N")?,
            "--shot" => o.shot = Some(value()?),
            "--time" => o.time = value()?.parse().map_err(|_| "--time T")?,
            "--step" => o.step = Some(value()?.parse().ok().filter(|&s: &f32| s > 0.0 && s.is_finite()).ok_or("--step S, S > 0")?),
            d if scenes::NAMES.contains(&d) => o.demos.push(a),
            _ => return Err(format!("usage: gpu-demo [{}]... [--mode WxH] [--seconds N] [--step S]", scenes::NAMES.join("|"))),
        }
    }
    Ok(o)
}

fn name() -> String {
    // SAFETY: strings that GL keeps
    unsafe { [0x1f01, 0x1f02].map(|s| std::ffi::CStr::from_ptr(glGetString(s)).to_string_lossy().into_owned()).join(", ") }
}

/// One picture of each demo, into a file.
fn shot(o: &Options, file: &str) -> Result<(), String> {
    if o.demos.len() > 1 && !file.contains('%') {
        return Err("--shot FILE: with several demos, % in the name stands for theirs".into());
    }
    let _gpu = kms::Gpu::any()?;
    let (w, h) = o.mode.unwrap_or((640, 480));
    let to = scenes::Target { fb: kms::offscreen(w, h), w: w as i32, h: h as i32, up: 1.0 };
    let (w, h) = (to.w, to.h);
    for d in &o.demos {
        let mut s = scenes::make(d).unwrap();
        s.start();
        // (some start from where they were)
        for k in 1..=30 {
            s.draw(o.time * k as f32 / 30.0, o.time / 30.0, &to);
        }
        let mut px = vec![0u8; (w * h * 4) as usize];
        // SAFETY: the pixels have their size
        unsafe {
            glBindFramebuffer(FRAMEBUFFER, to.fb);
            glReadPixels(0, 0, w, h, 0x1908, 0x1401, px.as_mut_ptr().cast());
        }
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for row in px.chunks_exact(w as usize * 4).rev() {
            out.extend(row.chunks_exact(4).flat_map(|p| &p[..3]));
        }
        let path = file.replace('%', d);
        std::fs::write(&path, out).map_err(|e| format!("{path}: {e}"))?;
        // SAFETY: a query
        println!("{path}: {}, GL error {:#x}", name(), unsafe { glGetError() });
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut o = options()?;
    if o.demos.is_empty() {
        o.demos = scenes::NAMES.iter().map(|n| n.to_string()).collect();
    }
    if let Some(file) = &o.shot {
        return shot(&o, file);
    }
    let gpu = kms::Gpu::open()?;
    let mut screen = kms::Screen::open(&gpu, o.mode)?;
    println!("{} on {} x {}", name(), screen.width, screen.height);
    // each made once, when it is first its turn
    let mut made: Vec<Option<Box<dyn scenes::Scene>>> = o.demos.iter().map(|_| None).collect();
    let rounds = if o.step.is_some() { o.demos.len() } else { usize::MAX };
    for k in 0..rounds {
        let k = k % o.demos.len();
        let d = &o.demos[k];
        let s = made[k].get_or_insert_with(|| scenes::make(d).unwrap());
        s.start();
        println!("{d}");
        let start = Instant::now();
        let (mut last, mut frames) = (0.0, 0);
        loop {
            let t = o.step.map_or_else(|| start.elapsed().as_secs_f32(), |s| frames as f32 * s);
            let to = scenes::Target { fb: screen.target(), w: screen.width as i32, h: screen.height as i32, up: -1.0 };
            s.draw(t, (t - last).min(0.1), &to);
            screen.show()?;
            last = t;
            frames += 1;
            // (with --step: as many pictures as there are steps in --seconds)
            let done = match o.step {
                Some(s) => frames as f32 * s + s / 2.0 > o.seconds,
                None => o.demos.len() > 1 && t > o.seconds,
            };
            if done {
                break;
            }
        }
        println!("{:.0} frames/s", frames as f32 / start.elapsed().as_secs_f32());
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("gpu-demo: {e}");
        std::process::exit(1);
    }
}
