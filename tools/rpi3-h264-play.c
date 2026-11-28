/*
 * rpi3-h264-play: play an H.264 file (Annex B) on a Raspberry Pi with the firmware's decoder and
 * the display's YUV planes, the way a media player does it, with nothing but the kernel's
 * headers. The stream goes through the V4L2 stateful decoder (bcm2835-codec, /dev/video10);
 * each decoded picture's buffer is exported as a dma-buf, imported by the vc4 DRM driver as a
 * YUV420 framebuffer and put on a KMS overlay plane (the HVS scans the decoder's buffer, no
 * copy), one picture per frame period of the stream's rate.
 *
 *   gcc -O2 -o play rpi3-h264-play.c && ./play FILE.264 [FPS]
 *   ./play -o OUT.yuv FILE.264      (or -o - for stdout): no display, as fast as it decodes,
 *                                   the visible part of each picture written as I420
 *
 * Prints PLAYED frames=N seconds=S fps=F late=L size=WxH at the end, to stderr with -o (time
 * from the first picture shown to the last, on CLOCK_MONOTONIC; late: pictures that missed
 * their slot).
 * Used by scripts/rpi3-h264.txt in the emulator; BSD-3-Clause like the rest of sbc-emu.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <linux/videodev2.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

#define OUT V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE
#define CAP V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE
#define NOUT 4
#define NCAP 6
#define CHUNK (512 * 1024)
#define FOURCC(a, b, c, d) ((uint32_t)(a) | (uint32_t)(b) << 8 | (uint32_t)(c) << 16 | (uint32_t)(d) << 24)

static int vfd, dfd;
static void die(const char *what) { fprintf(stderr, "FAIL %s: %s\n", what, strerror(errno)); exit(1); }
static void xioctl(int fd, unsigned long req, void *arg, const char *what) {
    while (ioctl(fd, req, arg) < 0) { if (errno != EINTR) die(what); }
}
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

/* the stream */
static uint8_t *data; static size_t len, pos;
/* output (bitstream) buffers */
static void *omap[NOUT]; static uint32_t olen[NOUT];
/* capture (picture) buffers and their framebuffers */
static int ncap; static uint32_t fb[NCAP];
static uint8_t *cmap[NCAP]; static FILE *dump;
static uint32_t width, height, bpl, vis_w, vis_h;
/* display */
static uint32_t crtc_id, plane_id, mode_w, mode_h;

static void queue_out(int i) {
    struct v4l2_plane pl = {0};
    struct v4l2_buffer b = {0};
    size_t n = len - pos < olen[i] ? len - pos : olen[i];
    if (n > CHUNK) n = CHUNK;
    memcpy(omap[i], data + pos, n);
    pos += n;
    pl.bytesused = n;
    b.type = OUT; b.memory = V4L2_MEMORY_MMAP; b.index = i; b.m.planes = &pl; b.length = 1;
    b.timestamp.tv_sec = pos / CHUNK; /* any increasing value */
    xioctl(vfd, VIDIOC_QBUF, &b, "QBUF output");
}

static void queue_cap(int i) {
    struct v4l2_plane pl = {0};
    struct v4l2_buffer b = {0};
    b.type = CAP; b.memory = V4L2_MEMORY_MMAP; b.index = i; b.m.planes = &pl; b.length = 1;
    xioctl(vfd, VIDIOC_QBUF, &b, "QBUF capture");
}

static void find_display(void) {
    char path[32];
    for (int c = 0; c < 4; c++) {
        snprintf(path, sizeof path, "/dev/dri/card%d", c);
        if ((dfd = open(path, O_RDWR | O_CLOEXEC)) < 0) continue;
        struct drm_mode_card_res r = {0};
        if (ioctl(dfd, DRM_IOCTL_MODE_GETRESOURCES, &r) == 0 && r.count_crtcs > 0) {
            uint32_t crtcs[16];
            uint32_t n = r.count_crtcs < 16 ? r.count_crtcs : 16;
            struct drm_mode_card_res r2 = {0};
            r2.crtc_id_ptr = (uintptr_t)crtcs; r2.count_crtcs = n;
            xioctl(dfd, DRM_IOCTL_MODE_GETRESOURCES, &r2, "GETRESOURCES");
            for (uint32_t k = 0; k < n; k++) {
                struct drm_mode_crtc cr = {0};
                cr.crtc_id = crtcs[k];
                if (ioctl(dfd, DRM_IOCTL_MODE_GETCRTC, &cr) || !cr.mode_valid) continue;
                crtc_id = crtcs[k]; mode_w = cr.mode.hdisplay; mode_h = cr.mode.vdisplay;
                /* an overlay plane on this CRTC that takes YUV420 */
                struct drm_mode_get_plane_res pr = {0};
                uint32_t planes[32];
                xioctl(dfd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr, "GETPLANERESOURCES");
                pr.count_planes = pr.count_planes < 32 ? pr.count_planes : 32;
                pr.plane_id_ptr = (uintptr_t)planes;
                xioctl(dfd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pr, "GETPLANERESOURCES");
                for (uint32_t p = 0; p < pr.count_planes; p++) {
                    uint32_t fmts[64];
                    struct drm_mode_get_plane gp = {0};
                    gp.plane_id = planes[p];
                    xioctl(dfd, DRM_IOCTL_MODE_GETPLANE, &gp, "GETPLANE");
                    if (!(gp.possible_crtcs & 1u << k)) continue;
                    gp.count_format_types = gp.count_format_types < 64 ? gp.count_format_types : 64;
                    gp.format_type_ptr = (uintptr_t)fmts;
                    xioctl(dfd, DRM_IOCTL_MODE_GETPLANE, &gp, "GETPLANE");
                    for (uint32_t f = 0; f < gp.count_format_types; f++)
                        if (fmts[f] == FOURCC('Y', 'U', '1', '2')) { plane_id = planes[p]; break; }
                    if (plane_id) break;
                }
                if (plane_id) { printf("display: %s crtc %u %ux%u, YUV420 plane %u\n", path, crtc_id, mode_w, mode_h, plane_id); return; }
            }
        }
        close(dfd);
    }
    errno = ENODEV; die("no CRTC with a YUV420 overlay plane");
}

/* The decoder found the stream's format: capture buffers, each exported to DRM. */
static void setup_capture(void) {
    struct v4l2_format f = {0};
    f.type = CAP;
    xioctl(vfd, VIDIOC_G_FMT, &f, "G_FMT capture");
    width = f.fmt.pix_mp.width; height = f.fmt.pix_mp.height;
    bpl = f.fmt.pix_mp.plane_fmt[0].bytesperline;
    vis_w = width; vis_h = height;
    struct v4l2_selection s = {0};
    s.type = V4L2_BUF_TYPE_VIDEO_CAPTURE; s.target = V4L2_SEL_TGT_COMPOSE;
    if (ioctl(vfd, VIDIOC_G_SELECTION, &s) == 0 && s.r.width && s.r.height) { vis_w = s.r.width; vis_h = s.r.height; }
    fprintf(dump ? stderr : stdout, "decoder: %ux%u (%ux%u visible), %u bytes per line, %c%c%c%c\n", width, height, vis_w, vis_h, bpl,
           f.fmt.pix_mp.pixelformat & 255, f.fmt.pix_mp.pixelformat >> 8 & 255, f.fmt.pix_mp.pixelformat >> 16 & 255, f.fmt.pix_mp.pixelformat >> 24);
    struct v4l2_requestbuffers rb = {0};
    rb.count = NCAP; rb.type = CAP; rb.memory = V4L2_MEMORY_MMAP;
    xioctl(vfd, VIDIOC_REQBUFS, &rb, "REQBUFS capture");
    ncap = rb.count < NCAP ? rb.count : NCAP;
    for (int i = 0; i < ncap; i++) {
        if (dump) {
            struct v4l2_plane pl = {0};
            struct v4l2_buffer b = {0};
            b.type = CAP; b.memory = V4L2_MEMORY_MMAP; b.index = i; b.m.planes = &pl; b.length = 1;
            xioctl(vfd, VIDIOC_QUERYBUF, &b, "QUERYBUF capture");
            cmap[i] = mmap(NULL, pl.length, PROT_READ, MAP_SHARED, vfd, pl.m.mem_offset);
            if (cmap[i] == MAP_FAILED) die("mmap capture");
            queue_cap(i);
            continue;
        }
        struct v4l2_exportbuffer e = {0};
        e.type = CAP; e.index = i; e.flags = O_RDONLY | O_CLOEXEC;
        xioctl(vfd, VIDIOC_EXPBUF, &e, "EXPBUF");
        struct drm_prime_handle h = {0};
        h.fd = e.fd;
        xioctl(dfd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &h, "PRIME_FD_TO_HANDLE");
        struct drm_mode_fb_cmd2 c = {0};
        c.width = vis_w; c.height = vis_h; c.pixel_format = FOURCC('Y', 'U', '1', '2');
        c.handles[0] = c.handles[1] = c.handles[2] = h.handle;
        c.pitches[0] = bpl; c.pitches[1] = c.pitches[2] = bpl / 2;
        c.offsets[0] = 0; c.offsets[1] = bpl * height; c.offsets[2] = bpl * height + (bpl / 2) * (height / 2);
        xioctl(dfd, DRM_IOCTL_MODE_ADDFB2, &c, "ADDFB2");
        fb[i] = c.fb_id;
        close(e.fd);
        queue_cap(i);
    }
    int t = CAP;
    xioctl(vfd, VIDIOC_STREAMON, &t, "STREAMON capture");
}

/* the visible part of picture i as I420 */
static void write_picture(int i) {
    const uint8_t *p = cmap[i];
    for (uint32_t r = 0; r < vis_h; r++) fwrite(p + r * bpl, 1, vis_w, dump);
    for (int c = 0; c < 2; c++) {
        const uint8_t *q = p + bpl * height + c * (bpl / 2) * (height / 2);
        for (uint32_t r = 0; r < (vis_h + 1) / 2; r++) fwrite(q + r * (bpl / 2), 1, (vis_w + 1) / 2, dump);
    }
}

static void show(int i) {
    struct drm_mode_set_plane sp = {0};
    /* the picture scaled to the screen's height, centred */
    uint32_t h = mode_h, w = (uint64_t)vis_w * mode_h / vis_h;
    if (w > mode_w) { w = mode_w; h = (uint64_t)vis_h * mode_w / vis_w; }
    sp.plane_id = plane_id; sp.crtc_id = crtc_id; sp.fb_id = fb[i];
    sp.crtc_x = (mode_w - w) / 2; sp.crtc_y = (mode_h - h) / 2; sp.crtc_w = w; sp.crtc_h = h;
    sp.src_w = vis_w << 16; sp.src_h = vis_h << 16;
    xioctl(dfd, DRM_IOCTL_MODE_SETPLANE, &sp, "SETPLANE");
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "-o")) {
        dump = strcmp(argv[2], "-") ? fopen(argv[2], "wb") : stdout;
        if (!dump) die(argv[2]);
        argv += 2; argc -= 2;
    }
    if (argc < 2) { fprintf(stderr, "usage: %s [-o OUT.yuv] FILE.264 [FPS]\n", argv[0]); return 2; }
    double fps = argc > 2 ? atof(argv[2]) : 30.0;
    int in = open(argv[1], O_RDONLY);
    struct stat st;
    if (in < 0 || fstat(in, &st)) die(argv[1]);
    len = st.st_size;
    data = mmap(NULL, len, PROT_READ, MAP_PRIVATE, in, 0);
    if (data == MAP_FAILED) die("mmap input");
    if (!dump) find_display();

    if ((vfd = open("/dev/video10", O_RDWR | O_NONBLOCK)) < 0) die("/dev/video10");
    struct v4l2_format f = {0};
    f.type = OUT;
    f.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_H264;
    f.fmt.pix_mp.width = 1920; f.fmt.pix_mp.height = 1080;
    f.fmt.pix_mp.num_planes = 1;
    f.fmt.pix_mp.plane_fmt[0].sizeimage = CHUNK;
    xioctl(vfd, VIDIOC_S_FMT, &f, "S_FMT output");
    struct v4l2_requestbuffers rb = {0};
    rb.count = NOUT; rb.type = OUT; rb.memory = V4L2_MEMORY_MMAP;
    xioctl(vfd, VIDIOC_REQBUFS, &rb, "REQBUFS output");
    for (int i = 0; i < NOUT; i++) {
        struct v4l2_plane pl = {0};
        struct v4l2_buffer b = {0};
        b.type = OUT; b.memory = V4L2_MEMORY_MMAP; b.index = i; b.m.planes = &pl; b.length = 1;
        xioctl(vfd, VIDIOC_QUERYBUF, &b, "QUERYBUF");
        omap[i] = mmap(NULL, pl.length, PROT_READ | PROT_WRITE, MAP_SHARED, vfd, pl.m.mem_offset);
        if (omap[i] == MAP_FAILED) die("mmap output");
        olen[i] = pl.length;
    }
    struct v4l2_event_subscription sub = {0};
    sub.type = V4L2_EVENT_SOURCE_CHANGE;
    xioctl(vfd, VIDIOC_SUBSCRIBE_EVENT, &sub, "SUBSCRIBE source change");
    sub.type = V4L2_EVENT_EOS;
    xioctl(vfd, VIDIOC_SUBSCRIBE_EVENT, &sub, "SUBSCRIBE eos");
    int queued = 0;
    for (int i = 0; i < NOUT && pos < len; i++) { queue_out(i); queued++; }
    int t = OUT;
    xioctl(vfd, VIDIOC_STREAMON, &t, "STREAMON output");

    int shown = -1, frames = 0, late = 0, stopped = 0, done = 0;
    double t0 = 0, last = 0;
    while (!done) {
        struct pollfd p = { vfd, POLLIN | POLLOUT | POLLPRI, 0 };
        if (poll(&p, 1, 5000) <= 0) { fprintf(stderr, "FAIL timeout after %d pictures\n", frames); return 1; }
        if (p.revents & POLLPRI) {
            struct v4l2_event ev = {0};
            while (ioctl(vfd, VIDIOC_DQEVENT, &ev) == 0) {
                if (ev.type == V4L2_EVENT_SOURCE_CHANGE && !ncap) setup_capture();
            }
        }
        if (p.revents & POLLOUT) {
            struct v4l2_plane pl = {0};
            struct v4l2_buffer b = {0};
            b.type = OUT; b.memory = V4L2_MEMORY_MMAP; b.m.planes = &pl; b.length = 1;
            while (ioctl(vfd, VIDIOC_DQBUF, &b) == 0) {
                if (pos < len) queue_out(b.index);
                else if (!stopped) {
                    struct v4l2_decoder_cmd c = {0};
                    c.cmd = V4L2_DEC_CMD_STOP;
                    xioctl(vfd, VIDIOC_DECODER_CMD, &c, "DECODER_CMD stop");
                    stopped = 1;
                }
            }
        }
        if ((p.revents & POLLIN) && ncap) {
            struct v4l2_plane pl = {0};
            struct v4l2_buffer b = {0};
            b.type = CAP; b.memory = V4L2_MEMORY_MMAP; b.m.planes = &pl; b.length = 1;
            while (ioctl(vfd, VIDIOC_DQBUF, &b) == 0) {
                if (pl.bytesused && dump) {
                    if (!frames) t0 = now();
                    write_picture(b.index);
                    last = now();
                    queue_cap(b.index);
                    frames++;
                } else if (pl.bytesused) {
                    /* the picture's slot: one frame period after the previous one's */
                    double due = frames ? t0 + frames / fps : now();
                    double n = now();
                    if (!frames) t0 = n;
                    else if (n > due + 0.5 / fps) late++;
                    else while ((n = now()) < due) usleep((useconds_t)((due - n) * 1e6));
                    show(b.index);
                    last = now();
                    if (shown >= 0) queue_cap(shown);
                    shown = b.index;
                    frames++;
                } else {
                    queue_cap(b.index);
                }
                if (b.flags & V4L2_BUF_FLAG_LAST) { done = 1; break; }
            }
        }
    }
    double secs = frames > 1 ? last - t0 : 0;
    if (dump) fflush(dump);
    fprintf(dump ? stderr : stdout, "PLAYED frames=%d seconds=%.2f fps=%.2f late=%d size=%ux%u\n", frames, secs, frames > 1 ? (frames - 1) / secs : 0.0, late, vis_w, vis_h);
    (void)queued;
    return 0;
}
