# H.264 decoder for the Raspberry Pi 3: evaluation

The Raspberry Pi 3's firmware decodes H.264 for Linux (`bcm2835-codec`, `/dev/video10`); in
sbc-emu the stream is decoded on the host by an H.264 decoder in Rust, natively and in the
browser build ([plan](rpi3-plan.md#graphics-and-video), decided 2026-11-27). This page is the
evaluation of the candidate crates the plan names, done 2026-11-28, and the decision.

**Decision: [`rust_h264`](https://crates.io/crates/rust_h264) 0.6.1** (MIT OR Apache-2.0, used
under MIT; pinned `=0.6.1` in `bcm2837/Cargo.toml`, its notice in `THIRD-PARTY-LICENSES`). It is
the only candidate that decodes High profile with CABAC correctly, and it decodes every 8-bit
4:2:0 conformance stream it is given exactly as FFmpeg does, interlaced ones included (176 of
176), so no decoder of our own (`h264/`) was written. It is behind the `h264` cargo feature,
off by default ([Raspberry Pi 3](raspberry-pi-3.md#video-decode)).

## The candidates

| | rusty_h264 / rusty_h264-decoder 0.17.4 | rust_h264 0.6.1 | yscv-video 0.2.0 |
|---|---|---|---|
| Licence | BSD-2-Clause | MIT OR Apache-2.0 | MIT OR Apache-2.0 |
| Dependencies | rusty_h264-common, -accel, rusty_alloc, safe_arch, wide (a build script that sets cfgs) | none | rayon, yscv-tensor (a build script that links Accelerate on macOS), yscv-cpu, bytes, … |
| Output | I420 frames, display order (`decode_stream`) or decode order | 8/10-bit 4:2:0, 4:2:2 planes, display order (`OrderedDecoder`) with per-picture metadata | RGB; YUV only through a conformance switch (`dump_yuv`) |
| `wasm32-unknown-unknown` build | yes | yes | yes |
| Runs as WebAssembly (Node 22) | yes, same output as native | yes, same output as native | **no pictures** (it spawns threads) |
| Conformance, 8-bit 4:2:0 progressive (103 streams) | 84 | **103** | 38 |
| Conformance, interlaced: field, PAFF, MBAFF (73 streams) | 0 (refused) | **73** | 0 |
| High profile, CABAC and 8×8 transforms (24 FRExt streams) | 16 | **24** | 0 |
| Our libx264 streams (8, see below) | 7 (not the interlaced one) | **8** | 4 |
| 1080p, one core (x264 High, CABAC, B-frames, 8 Mbit/s) | 117 fps | 85 fps | 96 fps on 1.3 cores |
| 1080p as WebAssembly (Node 22, same stream) | 57 fps | 45 fps | — |

FFmpeg 8.1's own decoder (`-threads 1`) does 126 fps on the same stream and machine (Apple
silicon, shared with other work). At 85 fps natively and 45 fps in the browser, rust_h264 keeps
up with 1080p30 on the emulator's thread.

rusty_h264's own list of gaps holds: it has no interlaced coding (it refuses all 73 such
streams), and of the progressive ones it refuses or differs on 19: CABAC `I_PCM` (`CAPCM1`), `CAPM3`,
several multi-reference and MMCO streams (`MR1_BT_A`, `MR3`–`MR5_TANDBERG`, `FRExt_MMCO4`),
slice-type switching (`CABAST3`, `CABASTBR3`), `CVWP2`, `CVFC1` (frame cropping), and eight High-profile ones (`HCAFR2`/`3`, `HCHP1`/`2`, `HCMP1`, `HPCAMOLQ`,
`HPCAQ2LQ`, `HPCVMOLQ`). yscv-video decodes intra pictures correctly but drifts from the first
inter picture in most P and B streams, refuses its CAVLC High path on our 1080p stream, and gives
no pictures as WebAssembly.

## Method

- **Bitstreams**: the ITU-T H.264.1 conformance suite as published by the JVT
  (`https://www.itu.int/wftp3/av-arch/jvt-site/draft_conformance/`, directories `AVCv1/` and
  `FRExt/`): all 135 AVCv1 streams (Baseline, Extended, Main) and 51 of the 69 FRExt streams
  (the 18 `Hi422*` ones, 10-bit 4:2:2, are left out: the Pi's decoder is 8-bit 4:2:0). The
  `.zip` files unpack to one directory per stream; the bitstream is the `.264`/`.jsv`/`.avc`/
  `.26l`/`.jvt`/`.h264` file in it.
- **Our streams**: libx264 (through FFmpeg on the host) from FFmpeg's `testsrc2` and `life`
  sources: Baseline, Main and High at 720p (`--preset veryfast`, `medium`, `slower`), High at
  1080p (8 Mbit/s; with four slices, weighted prediction, B-pyramid and four references; with
  CAVLC; interlaced with `--tff`), and High at 350×198 (cropping on both axes).
- **Reference**: FFmpeg 8.1's decoder on the host, `ffmpeg -i FILE -f rawvideo -pix_fmt yuv420p
  -fps_mode passthrough`, compared picture by picture (MD5 of each picture; for the first that
  differs, the luma PSNR). A stream passes when every picture is identical and the counts agree.
  For the 10-bit and 4:2:2 streams the comparison was also made in the native format
  (`yuv420p10le`, `yuv422p`), where rust_h264 matches FFmpeg exactly.
- **Speed**: decode time of the whole stream, one thread (`/usr/bin/time` user time), best of three.
- **WebAssembly**: each crate built for `wasm32-unknown-unknown` as a `cdylib` with one export that
  decodes a buffer, run under Node 22, the output's checksum compared with the native build's.
- **Pi OS's own encoders**: not available in the guest (no camera, and the firmware's encoder is
  not modelled); FFmpeg and GStreamer are not on Pi OS Lite. Files from libx264 stand in for them.

## Results by group

Streams per group, and how many each decoder decodes exactly as FFmpeg does. "pi3" is the
decoder as the Raspberry Pi 3 model uses it (`bcm2837::h264`, rust_h264 behind the
`video::VideoDecoder` interface, the stream pushed in pieces of 65521 bytes).

| Group | Streams | rusty_h264 | rust_h264 | yscv-video | pi3 |
|---|---:|---:|---:|---:|---:|
| Baseline and Extended, progressive | 38 | 33 | 38 | 26 | 38 |
| Main, CAVLC, progressive | 14 | 12 | 14 | 4 | 14 |
| Main, CABAC, progressive | 27 | 23 | 27 | 8 | 27 |
| High (FRExt), progressive, 8-bit 4:2:0 | 24 | 16 | 24 | 0 | 24 |
| Interlaced (field pictures, PAFF, MBAFF), all profiles | 73 | 0 | 73 | 0 | 73 |
| High 10, High 4:2:2 (outside the Pi's decoder) | 5 | 0 | 5 (native format) | 0 | — |
| FMO/ASO, SP/SI slices | 5 | 0 | 0 | 0 | 0 |
| Our libx264 streams | 8 | 7 | 8 | 4 | 8 |

The five streams no candidate decodes use flexible macroblock ordering (`FM1_BT_B`, `FM1_FT_E`,
`FM2_SVA_C`) or switching slices (`SP1_BT_A`, `sp2_bt_b`); FFmpeg does not decode them either
(it gives no pictures for two of them), and neither did the Pi's firmware decoder, whose profiles
are Constrained Baseline, Main and High. The decoder refuses them slice by slice rather than
showing wrong pictures.

So for the Pi 3: **176 of 176** 8-bit 4:2:0 conformance streams pass through the model's
decoder, progressive and interlaced, Baseline, Main and High (all listed in
`bcm2837/tests/data/h264-conformance.txt`), and all 8 of our libx264 streams, whether the stream
arrives in pieces of 7 bytes, 4096 bytes or one NAL unit at a time.

## Running the conformance test

The bitstreams are not in git (they are the JVT's, and large). Fetch the zip files of the
streams listed in `bcm2837/tests/data/h264-conformance.txt` from the two directories above and
unpack each into its own directory, as named in the list:

```sh
mkdir -p cache/h264-conformance && cd cache/h264-conformance
for n in $(grep -v '^#' ../../bcm2837/tests/data/h264-conformance.txt | cut -d' ' -f1); do
  for d in AVCv1 FRExt; do
    curl -sfO https://www.itu.int/wftp3/av-arch/jvt-site/draft_conformance/$d/$n.zip && break
  done
  mkdir -p $n && unzip -qo -j $n.zip -x '*.yuv' -d $n && rm $n.zip
done
cd ../..
cargo test --release -p bcm2837 --features h264 --test h264_conformance -- --nocapture
```

(`H264_CONFORMANCE=DIR` points the test elsewhere.) Without the files the test passes with a
note; with them it decodes each stream through `video::decoder_for(H264)` and compares the MD5 of
all its pictures with FFmpeg's (176 streams, about 25 s in a release build). The unit tests in
`bcm2837/src/h264.rs` run without downloads: our own 96×64 High-profile stream
(`bcm2837/tests/data/h264-high-96x64.264`) in pieces of 1 to 4096 bytes, timestamps through
B-pyramid reordering, reset, and corrupt input.

## In the guest

`scripts/rpi3-h264.txt` on the unmodified Raspberry Pi OS Lite image, with an emulator built
with the `h264` feature:

- `v4l2-ctl` decodes our 320×240 High-profile stream (`bcm2837/tests/data/h264-high-320x240.264`:
  CABAC, 8×8 transforms, weighted prediction, B-pyramid, 60 pictures) through `/dev/video10` to
  YU12: MD5 `5de407d726168b09997594653af1d7aa`, the same as FFmpeg's decode on the host.
- `tools/rpi3-h264-play.c`, built in the guest with its gcc, decodes a 1920×1080 High-profile
  libx264 stream (10 s at 30 fps, 6 Mbit/s): every picture's visible part has the MD5 of
  FFmpeg's decode.
- The same program plays the stream as a player would: the decoder's capture buffers exported
  as dma-bufs, imported by `vc4` as YUV420 framebuffers and shown on an HVS overlay plane (the
  KMS state lists the plane's `YU12` framebuffer), one per frame period: 300 pictures in 9.98 s,
  29.96 fps, none late. The whole run (boot, first-boot wizard, the checks, ten seconds of
  playback) takes about two minutes on the host.

## Per stream

"pass": every picture identical to FFmpeg's; "N/M match": pictures that are, with the number the
decoder gave when it differs; "refused": the decoder returned errors and no pictures.
"interlaced" covers field pictures, picture-adaptive frame/field (PAFF) and MBAFF streams.

| Stream | Profile | Size | Pictures | Coding | rusty_h264 | rust_h264 | yscv-video |
|---|---|---|---:|---|---|---|---|
| alphaconformanceG | Main | 960x544 | 40 | CAVLC | pass | pass | pass |
| AUD_MW_E | Baseline | 176x144 | 100 | CAVLC | pass | pass | pass |
| BA1_FT_C | Baseline | 352x288 | 299 | CAVLC | pass | pass | 3/299 match |
| BA1_Sony_D | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| BA2_Sony_F | Baseline | 176x144 | 300 | CAVLC | pass | pass | pass |
| BA3_SVA_C | Extended | 176x144 | 33 | CAVLC | pass | pass | 31/33 match |
| BA_MW_D | Baseline | 176x144 | 100 | CAVLC | pass | pass | pass |
| BAMQ1_JVC_C | Baseline | 176x144 | 30 | CAVLC | pass | pass | pass |
| BAMQ2_JVC_C | Baseline | 176x144 | 30 | CAVLC | pass | pass | pass |
| BANM_MW_D | Baseline | 176x144 | 100 | CAVLC | pass | pass | pass |
| BASQP1_Sony_C | Baseline | 176x144 | 4 | CAVLC | pass | pass | pass |
| brcm_freh10 | High | 352x288 | 100 | CABAC, 8x8, interlaced | refused | pass | 0/100 match, 42 out |
| brcm_freh11 | High | 352x288 | 100 | CABAC, 8x8, interlaced | refused | pass | 0/100 match, 30 out |
| brcm_freh3 | High | 352x288 | 100 | CABAC, 8x8 | pass | pass | 0/100 match |
| brcm_freh4 | High | 352x288 | 100 | CABAC, 8x8, interlaced | refused | pass | 0/100 match, 30 out |
| brcm_freh5 | High | 352x288 | 100 | CABAC, 8x8, MBAFF, interlaced | refused | pass | 0/100 match, 0 out |
| brcm_freh6 | High | 352x288 | 100 | CAVLC, 8x8, interlaced | refused | pass | 0/100 match, 200 out |
| brcm_freh8 | High | 352x288 | 100 | CABAC, 8x8 | pass | pass | 0/100 match |
| brcm_freh9 | High | 352x288 | 100 | CABAC, 8x8 | pass | pass | 0/100 match |
| CABA1_Sony_D | Main | 176x144 | 50 | CABAC | pass | pass | pass |
| CABA1_SVA_B | Main | 176x144 | 17 | CABAC | pass | pass | pass |
| CABA2_Sony_E | Main | 176x144 | 300 | CABAC | pass | pass | 20/300 match |
| CABA2_SVA_B | Main | 176x144 | 17 | CABAC | pass | pass | 2/17 match |
| CABA3_Sony_C | Main | 176x144 | 300 | CABAC | pass | pass | 20/300 match |
| CABA3_SVA_B | Main | 176x144 | 33 | CABAC | pass | pass | 4/33 match |
| CABA3_TOSHIBA_E | Main | 176x144 | 300 | CABAC | pass | pass | pass |
| cabac_mot_fld0_full | Main | 720x480 | 30 | CABAC, interlaced | refused | pass | 0/30 match, 0 out |
| cabac_mot_frm0_full | Main | 720x480 | 30 | CABAC | pass | pass | 3/30 match |
| cabac_mot_mbaff0_full | Main | 720x480 | 30 | CABAC, MBAFF, interlaced | refused | pass | 0/30 match, 0 out |
| cabac_mot_picaff0_full | Main | 720x480 | 30 | CABAC, interlaced | refused | pass | 0/30 match, 0 out |
| CABACI3_Sony_B | Main | 176x144 | 300 | CABAC | pass | pass | 4/300 match, 262 out |
| CABAST3_Sony_E | Main | 352x288 | 25 | CABAC | refused | pass | 1/25 match, 12 out |
| CABASTBR3_Sony_B | Main | 352x288 | 25 | CABAC | refused | pass | 1/25 match, 15 out |
| CABREF3_Sand_D | Main | 352x288 | 50 | CABAC, interlaced | refused | pass | 0/50 match, 2 out |
| CACQP3_Sony_D | Main | 176x144 | 50 | CABAC | pass | pass | 5/50 match |
| CAFI1_SVA_C | Main | 720x480 | 33 | CABAC, interlaced | refused | pass | 0/33 match, 0 out |
| CAMA1_Sony_C | Main | 720x480 | 5 | CABAC, MBAFF, interlaced | refused | pass | 0/5 match, 0 out |
| CAMA1_TOSHIBA_B | Main | 352x288 | 90 | CABAC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| cama1_vtc_c | Main | 720x480 | 4 | CABAC, MBAFF, interlaced | refused | pass | 0/4 match, 0 out |
| cama2_vtc_b | Main | 720x576 | 4 | CABAC, MBAFF, interlaced | refused | pass | 0/4 match, 0 out |
| CAMA3_Sand_E | Main | 352x288 | 50 | CABAC, MBAFF, interlaced | refused | pass | 0/50 match, 0 out |
| cama3_vtc_b | Main | 720x576 | 4 | CABAC, MBAFF, interlaced | refused | pass | 0/4 match, 0 out |
| CAMACI3_Sony_C | Main | 192x128 | 17 | CABAC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CAMANL1_TOSHIBA_B | Main | 352x288 | 90 | CABAC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| CAMANL2_TOSHIBA_B | Main | 352x288 | 90 | CABAC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| CAMANL3_Sand_E | Main | 352x288 | 50 | CABAC, MBAFF, interlaced | refused | pass | 0/50 match, 0 out |
| CAMASL3_Sony_B | Main | 192x128 | 17 | CABAC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CAMP_MOT_MBAFF_L30 | Main | 720x480 | 30 | CABAC, MBAFF, interlaced | refused | pass | 0/30 match, 0 out |
| CAMP_MOT_MBAFF_L31 | Main | 720x480 | 30 | CABAC, MBAFF, interlaced | refused | pass | 0/30 match, 0 out |
| CANL1_Sony_E | Main | 176x144 | 50 | CABAC | pass | pass | pass |
| CANL1_SVA_B | Main | 176x144 | 17 | CABAC | pass | pass | pass |
| CANL1_TOSHIBA_G | Main | 176x144 | 300 | CABAC | pass | pass | pass |
| CANL2_Sony_E | Main | 176x144 | 300 | CABAC | pass | pass | 20/300 match |
| CANL2_SVA_B | Main | 176x144 | 17 | CABAC | pass | pass | pass |
| CANL3_Sony_C | Main | 176x144 | 300 | CABAC | pass | pass | 20/300 match |
| CANL3_SVA_B | Main | 176x144 | 17 | CABAC | pass | pass | 2/17 match |
| CANL4_SVA_B | Main | 176x144 | 33 | CABAC | pass | pass | 6/33 match |
| CANLMA2_Sony_C | Main | 720x480 | 17 | CABAC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CANLMA3_Sony_C | Main | 720x480 | 17 | CABAC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CAPA1_TOSHIBA_B | Main | 352x288 | 90 | CABAC, interlaced | refused | pass | 0/90 match, 18 out |
| CAPAMA3_Sand_F | Main | 352x288 | 50 | CABAC, MBAFF, interlaced | refused | pass | 0/50 match, 0 out |
| CAPCM1_Sand_E | Main | 352x288 | 30 | CABAC | 0/30 match | pass | 0/30 match, 3 out |
| CAPCMNL1_Sand_E | Main | 352x288 | 30 | CABAC | pass | pass | 0/30 match, 3 out |
| CAPM3_Sony_D | Main | 176x144 | 300 | CABAC | 1/300 match | pass | 0/300 match, 210 out |
| CAQP1_Sony_B | Main | 176x144 | 50 | CABAC | pass | pass | pass |
| cavlc_mot_fld0_full_B | Main | 720x480 | 30 | CAVLC, interlaced | refused | pass | 0/30 match, 60 out |
| cavlc_mot_frm0_full_B | Main | 720x480 | 30 | CAVLC | pass | pass | 24/30 match |
| cavlc_mot_mbaff0_full_B | Main | 720x480 | 30 | CAVLC, MBAFF, interlaced | refused | pass | 0/30 match, 0 out |
| cavlc_mot_picaff0_full_B | Main | 720x480 | 30 | CAVLC, interlaced | refused | pass | 0/30 match |
| CAWP1_TOSHIBA_E | Main | 352x288 | 90 | CABAC, WP | pass | pass | 8/90 match |
| CAWP5_TOSHIBA_E | Main | 352x288 | 90 | CABAC, WP | pass | pass | 7/90 match |
| CI1_FT_B | Baseline | 352x288 | 291 | CAVLC | pass | pass | 2/291 match |
| CI_MW_D | Baseline | 176x144 | 100 | CAVLC | pass | pass | 47/100 match |
| CVBS3_Sony_C | Main | 176x144 | 300 | CAVLC | pass | pass | 146/300 match |
| CVCANLMA2_Sony_C | Main | 720x480 | 17 | CABAC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CVFC1_Sony_C | Baseline | 300x168 | 50 | CAVLC | 0/50 match, 73 out | pass | 0/50 match |
| CVFI1_Sony_D | Main | 720x480 | 17 | CAVLC, interlaced | refused | pass | 0/17 match, 34 out |
| CVFI1_SVA_C | Main | 720x480 | 7 | CAVLC, interlaced | refused | pass | 0/7 match, 14 out |
| CVFI2_Sony_H | Main | 720x480 | 17 | CAVLC, interlaced | refused | pass | 0/17 match, 34 out |
| CVFI2_SVA_C | Main | 720x480 | 13 | CAVLC, interlaced | refused | pass | 0/13 match, 26 out |
| CVMA1_Sony_D | Main | 720x480 | 5 | CAVLC, MBAFF, interlaced | refused | pass | 0/5 match, 0 out |
| CVMA1_TOSHIBA_B | Main | 352x288 | 90 | CAVLC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| CVMANL1_TOSHIBA_B | Main | 352x288 | 90 | CAVLC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| CVMANL2_TOSHIBA_B | Main | 352x288 | 90 | CAVLC, MBAFF, interlaced | refused | pass | 0/90 match, 0 out |
| CVMAPAQP3_Sony_E | Main | 720x480 | 8 | CAVLC, MBAFF, interlaced | refused | pass | 0/8 match, 12 out |
| CVMAQP2_Sony_G | Main | 192x128 | 17 | CAVLC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CVMAQP3_Sony_D | Main | 192x128 | 17 | CAVLC, MBAFF, interlaced | refused | pass | 0/17 match, 0 out |
| CVMP_MOT_FLD_L30_B | Main | 720x480 | 30 | CAVLC, interlaced | refused | pass | 0/30 match, 60 out |
| CVMP_MOT_FRM_L31_B | Main | 720x480 | 30 | CAVLC, interlaced | refused | pass | 0/30 match, 60 out |
| CVNLFI1_Sony_C | Main | 720x480 | 17 | CAVLC, interlaced | refused | pass | 0/17 match, 34 out |
| CVNLFI2_Sony_H | Main | 720x480 | 17 | CAVLC, interlaced | refused | pass | 0/17 match, 34 out |
| CVPA1_TOSHIBA_B | Main | 352x288 | 90 | CAVLC, interlaced | refused | pass | 0/90 match, 96 out |
| CVPCMNL1_SVA_C | Main | 352x288 | 30 | CAVLC | pass | pass | 0/30 match |
| CVPCMNL2_SVA_C | Main | 1280x720 | 2 | CAVLC | pass | pass | 0/2 match |
| CVSE2_Sony_B | Baseline | 176x144 | 278 | CAVLC | pass | pass | pass |
| CVSE3_Sony_H | Main | 176x144 | 278 | CAVLC | pass | pass | 139/278 match |
| CVSEFDFT3_Sony_E | Main | 176x144 | 200 | CAVLC | pass | pass | 99/200 match |
| CVWP1_TOSHIBA_E | Main | 352x288 | 90 | CAVLC, WP | pass | pass | pass |
| CVWP2_TOSHIBA_E | Main | 352x288 | 90 | CAVLC, WP | refused | pass | 33/90 match |
| CVWP3_TOSHIBA_E | Main | 352x288 | 90 | CAVLC, WP | pass | pass | 32/90 match |
| CVWP5_TOSHIBA_E | Main | 352x288 | 90 | CAVLC, WP | pass | pass | pass |
| FI1_Sony_E | Main | 192x128 | 17 | CAVLC, interlaced | refused | pass | 0/17 match, 34 out |
| FM1_BT_B | Baseline | 176x144 | 0 | CAVLC, FMO | refused | 0/0 match | 0/0 match, 280 out |
| FM1_FT_E | Baseline | 176x144 | 305 | CAVLC, FMO | refused | 118/305 match, 136 out | 119/305 match, 272 out |
| FM2_SVA_C | Baseline (?) | 0x0 | 0 | CAVLC, FMO | refused | 0/0 match | 0/0 match |
| FREH10-1 | High 10 (yuv420p10le) | 1920x1088 | 360 | CAVLC | refused | pass (native format) | 0/360 match |
| FREH10-2 | High 10 (yuv420p10le) | 1920x1088 | 358 | CAVLC | refused | pass (native format) | 0/358 match |
| freh12_b | High | 352x288 | 100 | CABAC, 8x8 | pass | pass | 0/100 match, 51 out |
| freh1_b | High | 352x288 | 100 | CAVLC, 8x8 | pass | pass | 0/100 match |
| freh2_b | High | 352x288 | 100 | CABAC, 8x8 | pass | pass | 0/100 match, 40 out |
| freh7_b | High | 352x288 | 100 | CABAC, 8x8, interlaced | refused | pass | 0/100 match, 38 out |
| FREXT01_JVC_D | High | 352x288 | 16 | CABAC, MBAFF, interlaced | refused | pass | 0/16 match, 0 out |
| FREXT02_JVC_C | High | 352x288 | 16 | CAVLC, MBAFF, interlaced | refused | pass | 0/16 match, 0 out |
| FRExt1_Panasonic_D | High | 352x288 | 8 | CAVLC, 8x8 | pass | pass | 0/8 match, 0 out |
| FREXT1_TANDBERG_A | High 4:2:2 (yuv422p) | 352x288 | 5 | CAVLC | refused | pass (native format) | 0/5 match |
| FRExt2_Panasonic_C | High | 352x288 | 15 | CAVLC, 8x8, MBAFF, interlaced | refused | pass | 0/15 match, 6 out |
| FREXT2_TANDBERG_A | High 4:2:2 (yuv422p) | 352x288 | 5 | CAVLC | refused | pass (native format) | 0/5 match |
| FRExt3_Panasonic_E | High | 352x288 | 11 | CABAC, 8x8 | pass | pass | 0/11 match |
| FREXT3_TANDBERG_A | High 4:2:2 (yuv422p) | 352x288 | 5 | CAVLC | refused | pass (native format) | 0/5 match |
| FRExt4_Panasonic_B | High | 352x288 | 10 | CABAC, 8x8, MBAFF, interlaced | refused | pass | 0/10 match, 0 out |
| FRExt_MMCO4_Sony_B | High | 352x288 | 60 | CABAC, 8x8 | 0/60 match | pass | 0/60 match |
| HCAFF1_HHI_B | High | 352x288 | 10 | CABAC, interlaced | refused | pass | 0/10 match, 0 out |
| HCAFR1_HHI_C | High | 352x288 | 10 | CABAC | pass | pass | 0/10 match |
| HCAFR2_HHI_A | High | 352x288 | 10 | CABAC | 8/10 match | pass | 0/10 match, 5 out |
| HCAFR3_HHI_A | High | 352x288 | 10 | CABAC | 9/10 match | pass | 0/10 match, 9 out |
| HCAFR4_HHI_A | High | 352x288 | 10 | CABAC | pass | pass | 0/10 match |
| HCAMFF1_HHI_B | High | 352x288 | 10 | CABAC, MBAFF, interlaced | refused | pass | 0/10 match, 0 out |
| HCBP1_HHI_A | Baseline | 352x288 | 250 | CAVLC | pass | pass | pass |
| HCBP2_HHI_A | Baseline | 352x288 | 250 | CAVLC | pass | pass | pass |
| HCHP1_HHI_B | High | 352x288 | 250 | CABAC, 8x8 | 156/250 match | pass | 6/250 match |
| HCHP2_HHI_A | High | 352x288 | 250 | CABAC, 8x8 | 249/250 match | pass | 3/250 match |
| HCHP3_HHI_A | High | 720x576 | 250 | CABAC, MBAFF, interlaced | refused | pass | 0/250 match, 0 out |
| HCMP1_HHI_A | Main | 352x288 | 250 | CAVLC | 249/250 match | pass | 16/250 match |
| HPCA_BRCM_C | High | 352x288 | 300 | CABAC | pass | pass | 24/300 match |
| HPCADQ_BRCM_B | High | 352x288 | 300 | CABAC, 8x8 | pass | pass | 0/300 match |
| HPCAFL_BRCM_C | High | 352x288 | 300 | CABAC, interlaced | refused | pass | 0/300 match, 120 out |
| HPCAFLNL_BRCM_C | High | 352x288 | 300 | CABAC, interlaced | refused | pass | 0/300 match, 124 out |
| HPCALQ_BRCM_B | High | 352x288 | 300 | CABAC, 8x8 | pass | pass | 0/300 match |
| HPCAMAPALQ_BRCM_B | High | 352x288 | 300 | CABAC, 8x8, MBAFF, interlaced | refused | pass | 0/300 match, 60 out |
| HPCAMOLQ_BRCM_B | High | 352x288 | 100 | CABAC, 8x8 | refused | pass | panic |
| HPCANL_BRCM_C | High | 352x288 | 300 | CABAC | pass | pass | 24/300 match |
| HPCAQ2LQ_BRCM_B | High | 352x288 | 100 | CABAC, 8x8 | 0/100 match | pass | 0/100 match |
| HPCV_BRCM_A | High | 352x288 | 300 | CAVLC | pass | pass | 0/300 match |
| HPCVFL_BRCM_A | High | 352x288 | 300 | CAVLC, interlaced | refused | pass | 0/300 match, 600 out |
| HPCVFLNL_BRCM_A | High | 352x288 | 300 | CAVLC, interlaced | refused | pass | 0/300 match, 600 out |
| HPCVMOLQ_BRCM_B | High | 352x288 | 100 | CAVLC, 8x8 | refused | pass | panic |
| HPCVNL_BRCM_A | High | 352x288 | 300 | CAVLC | pass | pass | 0/300 match |
| HVLCFI0_Sony_B | High | 720x480 | 19 | CAVLC, 8x8, interlaced | refused | pass | 0/19 match, 38 out |
| HVLCMFF0_Sony_B | High | 720x480 | 19 | CAVLC, 8x8, MBAFF, interlaced | refused | pass | 0/19 match, 22 out |
| HVLCPFF0_Sony_B | High | 720x480 | 19 | CAVLC, 8x8, interlaced | refused | pass | 0/19 match, 22 out |
| LS_SVA_D | Baseline | 176x144 | 1700 | CAVLC | pass | pass | pass |
| MIDR_MW_D | Baseline | 176x144 | 100 | CAVLC | pass | pass | pass |
| MPS_MW_A | Baseline | 176x144 | 150 | CAVLC | pass | pass | 0/150 match, 119 out |
| MR1_BT_A | Baseline | 176x144 | 62 | CAVLC | refused | pass | 5/62 match |
| MR1_MW_A | Baseline | 176x144 | 150 | CAVLC | pass | pass | pass |
| MR2_MW_A | Baseline | 176x144 | 300 | CAVLC | pass | pass | 203/300 match |
| MR2_TANDBERG_E | Baseline | 176x144 | 300 | CAVLC | pass | pass | 8/300 match |
| MR3_TANDBERG_B | Baseline | 176x144 | 300 | CAVLC | refused | pass | 48/300 match |
| MR4_TANDBERG_C | Baseline | 176x144 | 300 | CAVLC | refused | pass | 16/300 match |
| MR5_TANDBERG_C | Baseline | 176x144 | 300 | CAVLC | refused | pass | 9/300 match |
| MR6_BT_B | Extended | 192x128 | 60 | CAVLC, interlaced | refused | pass | 0/60 match, 120 out |
| MR7_BT_B | Extended | 192x128 | 60 | CAVLC, interlaced | refused | pass | 0/60 match, 12 out |
| MR8_BT_B | Extended | 192x128 | 58 | CAVLC, interlaced | refused | pass | 0/58 match, 116 out |
| MR9_BT_B | Main | 192x128 | 58 | CABAC, MBAFF, interlaced | refused | pass | 0/58 match, 6 out |
| MV1_BRCM_D | Main | 720x480 | 257 | CABAC | pass | pass | 1/257 match |
| NL1_Sony_D | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| NL2_Sony_H | Baseline | 176x144 | 300 | CAVLC | pass | pass | pass |
| NL3_SVA_E | Main | 176x144 | 33 | CAVLC | pass | pass | pass |
| NLMQ1_JVC_C | Baseline | 176x144 | 30 | CAVLC | pass | pass | pass |
| NLMQ2_JVC_C | Baseline | 176x144 | 30 | CAVLC | pass | pass | pass |
| NRF_MW_E | Baseline | 176x144 | 100 | CAVLC | pass | pass | pass |
| Sharp_MP_Field_1_B | Main | 720x480 | 15 | CAVLC, interlaced | refused | pass | 0/15 match, 30 out |
| Sharp_MP_Field_2_B | Main | 720x480 | 15 | CAVLC, interlaced | refused | pass | 0/15 match, 30 out |
| Sharp_MP_Field_3_B | Main | 720x480 | 15 | CAVLC, interlaced | refused | pass | 0/15 match, 30 out |
| Sharp_MP_PAFF_1r2 | Main | 720x480 | 15 | CAVLC, interlaced | refused | pass | 0/15 match, 16 out |
| Sharp_MP_PAFF_2r | Main | 720x480 | 15 | CABAC, interlaced | refused | pass | 0/15 match, 0 out |
| SL1_SVA_B | Main | 176x144 | 33 | CAVLC | pass | pass | 17/33 match |
| SP1_BT_A | Extended | 176x144 | 400 | CAVLC, SP | refused | 9/400 match, 322 out | 10/400 match, 361 out |
| sp2_bt_b | Extended | 176x144 | 400 | CAVLC, SP | refused | 9/400 match, 322 out | 10/400 match, 361 out |
| SVA_BA1_B | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| SVA_BA2_D | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| SVA_Base_B | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| SVA_CL1_E | Baseline | 176x144 | 50 | CAVLC | pass | pass | pass |
| SVA_FM1_E | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| SVA_NL1_B | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
| SVA_NL2_E | Baseline | 176x144 | 17 | CAVLC | pass | pass | pass |
