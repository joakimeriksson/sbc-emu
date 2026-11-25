/* A lit, spinning cube drawn by NVIDIA's stock JetPack OpenGL ES driver on the emulated
 * Orin GPU (Ampere GA10B), copied frame by frame into the Linux framebuffer that the
 * emulator's VNC server shows. Renders at 320x180 and doubles each pixel into 640x360.
 * Built freestanding against glibc: the GL entry points come from eglGetProcAddress. */
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);

#define RW 320
#define RH 180
#define FW 640
#define FH 360

typedef void *(*get_proc_t)(const char *);
static get_proc_t get_proc;
#define GL(ret, name, ...) static ret (*name)(__VA_ARGS__); name = get_proc(#name)

static const char *vs =
    "attribute vec3 pos; attribute vec3 nrm; attribute vec3 col;\n"
    "uniform mat4 mvp; uniform mat3 rot; varying vec3 v;\n"
    "void main() {\n"
    "  gl_Position = mvp * vec4(pos, 1.0);\n"
    "  vec3 n = normalize(rot * nrm);\n"
    "  float d = max(dot(n, normalize(vec3(0.4, 0.7, 1.0))), 0.0);\n"
    "  v = col * (0.25 + 0.75 * d) + vec3(0.6) * pow(d, 24.0);\n"
    "}\n";
static const char *fs =
    "precision mediump float; varying vec3 v;\n"
    "void main() { gl_FragColor = vec4(v, 1.0); }\n";

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

static void mul(float *r, const float *a, const float *b) {
    float t[16];
    for (int c = 0; c < 4; c++)
        for (int k = 0; k < 4; k++)
            t[c * 4 + k] = a[k] * b[c * 4] + a[4 + k] * b[c * 4 + 1] + a[8 + k] * b[c * 4 + 2] + a[12 + k] * b[c * 4 + 3];
    for (int i = 0; i < 16; i++) r[i] = t[i];
}

int main(int argc, char **argv) {
    int frames = argc > 1 ? atoi(argv[1]) : 0; /* 0: forever */
    int fb = open("/dev/fb0", O_RDWR);
    uint32_t *screen = fb < 0 ? NULL : mmap(NULL, FW * FH * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
    if (screen == MAP_FAILED || !screen) {
        printf("no /dev/fb0\n");
        return 1;
    }
    void *egl = dlopen("libEGL.so.1", 2);
    if (!egl)
        return 2;
    get_proc = dlsym(egl, "eglGetProcAddress");
    unsigned (*query_devices)(int, void **, int *) = get_proc("eglQueryDevicesEXT");
    void *(*platform_display)(unsigned, void *, const int *) = get_proc("eglGetPlatformDisplayEXT");
    unsigned (*initialize)(void *, int *, int *) = dlsym(egl, "eglInitialize");
    unsigned (*bind_api)(unsigned) = dlsym(egl, "eglBindAPI");
    unsigned (*choose_config)(void *, const int *, void **, int, int *) = dlsym(egl, "eglChooseConfig");
    void *(*create_context)(void *, void *, void *, const int *) = dlsym(egl, "eglCreateContext");
    void *(*create_pbuffer)(void *, void *, const int *) = dlsym(egl, "eglCreatePbufferSurface");
    unsigned (*make_current)(void *, void *, void *, void *) = dlsym(egl, "eglMakeCurrent");
    int count = 0, major, minor;
    void *devices[8];
    query_devices(8, devices, &count);
    void *display = platform_display(0x313f, devices[0], 0);
    if (!count || !initialize(display, &major, &minor))
        return 3;
    const int config_attribs[] = {0x3033, 1, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3025, 16, 0x3038};
    void *config;
    bind_api(0x30a0);
    if (!choose_config(display, config_attribs, &config, 1, &count) || !count)
        return 4;
    const int context_attribs[] = {0x3098, 2, 0x3038};
    const int surface_attribs[] = {0x3057, RW, 0x3056, RH, 0x3038};
    void *context = create_context(display, config, 0, context_attribs);
    void *surface = create_pbuffer(display, config, surface_attribs);
    if (!context || !surface || !make_current(display, surface, surface, context))
        return 5;

    GL(unsigned, glCreateShader, unsigned);
    GL(void, glShaderSource, unsigned, int, const char **, const int *);
    GL(void, glCompileShader, unsigned);
    GL(unsigned, glCreateProgram, void);
    GL(void, glAttachShader, unsigned, unsigned);
    GL(void, glBindAttribLocation, unsigned, unsigned, const char *);
    GL(void, glLinkProgram, unsigned);
    GL(void, glUseProgram, unsigned);
    GL(int, glGetUniformLocation, unsigned, const char *);
    GL(void, glUniformMatrix4fv, int, int, unsigned char, const float *);
    GL(void, glUniformMatrix3fv, int, int, unsigned char, const float *);
    GL(void, glVertexAttribPointer, unsigned, int, unsigned, unsigned char, int, const void *);
    GL(void, glEnableVertexAttribArray, unsigned);
    GL(void, glEnable, unsigned);
    GL(void, glViewport, int, int, int, int);
    GL(void, glClearColor, float, float, float, float);
    GL(void, glClear, unsigned);
    GL(void, glDrawArrays, unsigned, int, int);
    GL(void, glReadPixels, int, int, int, int, unsigned, unsigned, void *);
    GL(const unsigned char *, glGetString, unsigned);

    unsigned program = glCreateProgram();
    const char *sources[2] = {vs, fs};
    for (int i = 0; i < 2; i++) {
        unsigned s = glCreateShader(i ? 0x8b30 : 0x8b31);
        glShaderSource(s, 1, &sources[i], 0);
        glCompileShader(s);
        glAttachShader(program, s);
    }
    glBindAttribLocation(program, 0, "pos");
    glBindAttribLocation(program, 1, "nrm");
    glBindAttribLocation(program, 2, "col");
    glLinkProgram(program);
    glUseProgram(program);
    int mvp_at = glGetUniformLocation(program, "mvp"), rot_at = glGetUniformLocation(program, "rot");

    /* six faces, two triangles each: position, normal, colour */
    static const float N[6][3] = {{0, 0, 1}, {0, 0, -1}, {1, 0, 0}, {-1, 0, 0}, {0, 1, 0}, {0, -1, 0}};
    static const float C[6][3] = {{0.9, 0.2, 0.2}, {0.2, 0.8, 0.3}, {0.2, 0.4, 0.95}, {0.95, 0.8, 0.2}, {0.8, 0.3, 0.9}, {0.2, 0.85, 0.9}};
    static float data[36][9];
    for (int f = 0, v = 0; f < 6; f++) {
        int a = f / 2, s = f & 1 ? -1 : 1;
        int u = (a + 1) % 3, w = (a + 2) % 3;
        float corner[4][2] = {{-1, -1}, {1, -1}, {1, 1}, {-1, 1}};
        int order[6] = {0, 1, 2, 0, 2, 3};
        for (int k = 0; k < 6; k++, v++) {
            float p[3];
            p[a] = s;
            p[u] = corner[order[k]][0] * s;
            p[w] = corner[order[k]][1];
            for (int c = 0; c < 3; c++) {
                data[v][c] = p[c];
                data[v][3 + c] = N[f][c];
                data[v][6 + c] = C[f][c];
            }
        }
    }
    for (int i = 0; i < 3; i++) {
        glVertexAttribPointer(i, 3, 0x1406, 0, 36, &data[0][3 * i]);
        glEnableVertexAttribArray(i);
    }
    glEnable(0x0b71); /* depth test */
    glViewport(0, 0, RW, RH);
    printf("GL renderer: %s\n", glGetString(0x1f01));

    static uint32_t pixels[RW * RH];
    double start = now(), last = start;
    for (int frame = 0; !frames || frame < frames; frame++) {
        float t = (float)(now() - start) * 0.8f;
        float cy = cosf(t), sy = sinf(t), cx = cosf(t * 0.7f), sx = sinf(t * 0.7f);
        /* rotation about y then x, column-major */
        float r[16] = {cy, sx * sy, -cx * sy, 0, 0, cx, sx, 0, sy, -sx * cy, cx * cy, 0, 0, 0, 0, 1};
        float view[16] = {1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, -5, 1};
        float n = 1, fa = 20, aspect = (float)RW / RH, f1 = 1.0f / tanf(0.45f);
        float proj[16] = {f1 / aspect, 0, 0, 0, 0, f1, 0, 0, 0, 0, (fa + n) / (n - fa), -1, 0, 0, 2 * fa * n / (n - fa), 0};
        float m[16];
        mul(m, view, r);
        mul(m, proj, m);
        float rot[9] = {r[0], r[1], r[2], r[4], r[5], r[6], r[8], r[9], r[10]};
        glUniformMatrix4fv(mvp_at, 1, 0, m);
        glUniformMatrix3fv(rot_at, 1, 0, rot);
        glClearColor(0.05f, 0.07f, 0.12f, 1);
        glClear(0x4100);
        glDrawArrays(4, 0, 36);
        glReadPixels(0, 0, RW, RH, 0x1908, 0x1401, pixels);
        /* RGBA bottom-up to XRGB top-down, each pixel doubled */
        for (int y = 0; y < FH; y++) {
            const uint32_t *src = pixels + (RH - 1 - y / 2) * RW;
            uint32_t *dst = screen + y * FW;
            for (int x = 0; x < FW; x++) {
                uint32_t p = src[x / 2];
                dst[x] = (p & 0xff) << 16 | (p & 0xff00) | (p >> 16 & 0xff);
            }
        }
        double t1 = now();
        if (t1 - last >= 5 || frame == 0) {
            printf("frame %d, %.2f frames/s\n", frame, (frame + 1) / (t1 - start));
            fflush(stdout);
            last = t1;
        }
    }
    printf("ORIN_GL_DEMO_ DONE\n");
    return 0;
}
