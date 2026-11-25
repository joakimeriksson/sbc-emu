//! `--camera-source SPEC`: pictures for the emulated USB camera from the host, through `ffmpeg`
//! (which must be on the PATH), scaled to the camera's 640x480 YUYV:
//!
//! - `avfoundation:N`: the Mac's camera N (`ffmpeg -f avfoundation -list_devices true -i ""`
//!   lists them; macOS asks once whether the terminal may use the camera);
//! - `v4l2:/dev/videoN`: a camera on Linux;
//! - anything else: a video or picture file, played in real time and looped.
//!
//! A host thread reads ffmpeg's frames and keeps the latest; the camera sends it at its own
//! 30 frames a second of guest time, so a slow guest skips pictures rather than lagging.
use emu_core::usb::{FrameSource, CAM_FRAME, CAM_H, CAM_W};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// ffmpeg's input arguments for a source.
fn input(spec: &str) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    if let Some(n) = spec.strip_prefix("avfoundation:") {
        // (macOS cameras deliver 30 frames a second at 640x480 or more; ffmpeg scales)
        let mut v = s(&["-f", "avfoundation", "-framerate", "30", "-i"]);
        v.push(n.to_string());
        v
    } else if let Some(dev) = spec.strip_prefix("v4l2:") {
        let mut v = s(&["-f", "v4l2", "-i"]);
        v.push(dev.to_string());
        v
    } else {
        let mut v = s(&["-re", "-stream_loop", "-1", "-i"]);
        v.push(spec.to_string());
        v
    }
}

/// Start ffmpeg on `spec`; the frames it delivers appear in the returned source.
pub fn start(spec: &str) -> Result<FrameSource, String> {
    let fit = format!("scale={CAM_W}:{CAM_H}:force_original_aspect_ratio=decrease,pad={CAM_W}:{CAM_H}:(ow-iw)/2:(oh-ih)/2");
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(input(spec))
        .args(["-an", "-vf", &fit, "-r", "30", "-pix_fmt", "yuyv422", "-f", "rawvideo", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("--camera-source: cannot run ffmpeg: {e}"))?;
    let mut out = child.stdout.take().ok_or("--camera-source: no output from ffmpeg")?;
    let source: FrameSource = Arc::new(Mutex::new(None));
    let latest = source.clone();
    let spec = spec.to_string();
    std::thread::Builder::new()
        .name("camera-source".into())
        .spawn(move || {
            let mut frame = vec![0u8; CAM_FRAME];
            while out.read_exact(&mut frame).is_ok() {
                if let Ok(mut l) = latest.lock() {
                    *l = Some(frame.clone());
                }
            }
            let _ = child.wait();
            eprintln!("[camera] {spec}: ffmpeg ended; the camera keeps its last picture");
        })
        .map_err(|e| e.to_string())?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs() {
        assert_eq!(input("avfoundation:0")[..4], ["-f", "avfoundation", "-framerate", "30"]);
        assert_eq!(input("avfoundation:0").last().unwrap(), "0");
        assert_eq!(input("v4l2:/dev/video2"), ["-f", "v4l2", "-i", "/dev/video2"]);
        assert_eq!(input("street.mp4"), ["-re", "-stream_loop", "-1", "-i", "street.mp4"]);
    }
}
