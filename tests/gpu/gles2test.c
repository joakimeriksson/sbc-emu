/* OpenGL ES 2.0 test scenes for GPUs without ES 3 (the Orange Pi One's Mali-400),
 * compared against Mesa's softpipe (llvmpipe's generated code needs a working
 * armhf LLVM). The ES 2 counterpart of gltest.c.
 *
 *   gles2test run DIR [SCENE]   render every scene into DIR/<scene>.rgba (128x128 RGBA8)
 *   gles2test cmp A B           compare two runs: per-scene mismatch statistics, PPMs
 *
 * Run once on the GPU and once with GALLIUM_DRIVER=softpipe LIBGL_ALWAYS_SOFTWARE=1.
 * Built in the guest: gcc -O1 -o gles2test gles2test.c -lEGL -lGLESv2 -lm
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <signal.h>
#include <ucontext.h>
#include <unistd.h>

#if defined(__arm__)
/* Diagnostics for crashes inside the GL driver (llvmpipe's generated code, say). */
static void crash(int sig, siginfo_t *si, void *uc_) {
    ucontext_t *uc = uc_;
    mcontext_t *m = &uc->uc_mcontext;
    unsigned long *r = &m->arm_r0;
    fprintf(stderr, "signal %d at pc %08lx (cpsr %08lx) fault %p\n", sig, m->arm_pc, m->arm_cpsr, si->si_addr);
    for (int i = 0; i < 16; i++) fprintf(stderr, "r%-2d %08lx%s", i, r[i], i % 4 == 3 ? "\n" : "  ");
    /* the code before the return address (a call through a bad pointer lands at pc 0) */
    unsigned long at = m->arm_pc > 0x10000 ? m->arm_pc : m->arm_lr;
    unsigned *w = (unsigned *)(at & ~3ul);
    fprintf(stderr, "code at %08lx:\n", at);
    for (int i = -8; i < 2; i++) fprintf(stderr, "%08lx: %08x%s\n", (unsigned long)(w + i), w[i], i == 0 ? " <" : "");
    unsigned bl = w[-1];
    if ((bl & 0x0f000000) == 0x0b000000) {
        unsigned *t = (unsigned *)((unsigned long)(w - 1) + 8 + ((int)(bl << 8) >> 6));
        fprintf(stderr, "bl target %p:", (void *)t);
        for (int i = 0; i < 6; i++) fprintf(stderr, " %08x", t[i]);
        fprintf(stderr, "\n");
    }
    _exit(128 + sig);
}
#endif

#define W 128
#define H 128

static GLuint prog(const char *vs, const char *fs) {
    GLuint p = glCreateProgram();
    const char *src[2] = {vs, fs};
    GLenum ty[2] = {GL_VERTEX_SHADER, GL_FRAGMENT_SHADER};
    for (int i = 0; i < 2; i++) {
        GLuint s = glCreateShader(ty[i]);
        glShaderSource(s, 1, &src[i], NULL);
        glCompileShader(s);
        GLint ok; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
        if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); exit(1); }
        glAttachShader(p, s);
    }
    glBindAttribLocation(p, 0, "pos");
    glLinkProgram(p);
    GLint ok; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[2048]; glGetProgramInfoLog(p, sizeof log, NULL, log); fprintf(stderr, "link: %s\n", log); exit(1); }
    glUseProgram(p);
    return p;
}

static void attr(GLuint p, const char *name, int n, const float *data) {
    GLint loc = glGetAttribLocation(p, name);
    if (loc < 0) return;
    glEnableVertexAttribArray(loc);
    glVertexAttribPointer(loc, n, GL_FLOAT, GL_FALSE, 0, data);
}

static void unattr(void) {
    for (int i = 0; i < 8; i++) glDisableVertexAttribArray(i);
}

static const char *VS_COL =
    "attribute vec4 pos; attribute vec4 col; varying vec4 v;\n"
    "void main() { gl_Position = pos; v = col; }\n";
static const char *FS_COL =
    "precision mediump float; varying vec4 v;\n"
    "void main() { gl_FragColor = v; }\n";
static const char *VS_TEX =
    "attribute vec4 pos; attribute vec2 uv; varying vec2 t;\n"
    "void main() { gl_Position = pos; t = uv; }\n";
static const char *FS_TEX =
    "precision mediump float; varying vec2 t; uniform sampler2D s;\n"
    "void main() { gl_FragColor = texture2D(s, t); }\n";

static void clear(float r, float g, float b) {
    glClearColor(r, g, b, 1); glClearDepthf(1); glClearStencil(0);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
}

static void quad(GLuint p, float scale, float uvscale) {
    float pos[] = {-scale, -scale, 0, 1, scale, -scale, 0, 1, -scale, scale, 0, 1, scale, scale, 0, 1};
    float uv[] = {0, 0, uvscale, 0, 0, uvscale, uvscale, uvscale};
    attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static GLuint texture(int w, int h, GLenum format, GLenum type, const void *data, GLenum filter) {
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, format, w, h, 0, format, type, data);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, filter);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, filter);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    return tex;
}

static void s_clear(void) { clear(0.2f, 0.4f, 0.6f); }

static void s_tri_color(void) {
    clear(0.1f, 0.1f, 0.3f);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.8f, -0.8f, 0, 1, 0.8f, -0.6f, 0, 1, 0.1f, 0.85f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}

static void s_depth(void) {
    clear(0, 0, 0);
    glEnable(GL_DEPTH_TEST); glDepthFunc(GL_LESS);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.8f, -0.8f, -0.5f, 1, 0.6f, -0.8f, -0.5f, 1, -0.1f, 0.7f, -0.5f, 1,
                   -0.6f, -0.6f, 0.5f, 1, 0.9f, -0.4f, 0.5f, 1, 0.3f, 0.9f, -0.9f, 1};
    float col[] = {1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glDisable(GL_DEPTH_TEST);
}

static void s_blend(void) {
    clear(0.0f, 0.0f, 0.5f);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.9f, -0.9f, 0, 1, 0.5f, -0.9f, 0, 1, -0.2f, 0.6f, 0, 1,
                   -0.5f, -0.5f, 0, 1, 0.9f, -0.5f, 0, 1, -0.5f, 0.9f, 0, 1, 0.9f, -0.5f, 0, 1, 0.9f, 0.9f, 0, 1, -0.5f, 0.9f, 0, 1};
    float col[] = {1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1,
                   1, 0, 0, 0.5f, 1, 0, 0, 0.5f, 1, 0, 0, 0.5f, 0, 1, 1, 0.25f, 0, 1, 1, 0.75f, 0, 1, 1, 0.5f};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glEnable(GL_BLEND); glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
    glDrawArrays(GL_TRIANGLES, 3, 6);
    glDisable(GL_BLEND);
}

static void s_blend_modes(void) {
    clear(0.3f, 0.6f, 0.9f);
    GLuint p = prog(VS_COL, FS_COL);
    glEnable(GL_BLEND);
    /* four bands, each a different equation / factor pair */
    for (int i = 0; i < 4; i++) {
        float x0 = -1 + i * 0.5f, x1 = x0 + 0.5f;
        float pos[] = {x0, -1, 0, 1, x1, -1, 0, 1, x0, 1, 0, 1, x1, 1, 0, 1};
        float col[] = {0.8f, 0.2f, 0.4f, 0.3f, 0.8f, 0.2f, 0.4f, 0.3f, 0.1f, 0.9f, 0.5f, 0.9f, 0.1f, 0.9f, 0.5f, 0.9f};
        attr(p, "pos", 4, pos); attr(p, "col", 4, col);
        switch (i) {
        case 0: glBlendEquation(GL_FUNC_ADD); glBlendFunc(GL_ONE, GL_ONE); break;
        case 1: glBlendEquation(GL_FUNC_SUBTRACT); glBlendFunc(GL_ONE, GL_ONE); break;
        case 2: glBlendEquation(GL_FUNC_REVERSE_SUBTRACT); glBlendFunc(GL_DST_COLOR, GL_SRC_ALPHA); break;
        case 3: glBlendEquation(GL_FUNC_ADD); glBlendColor(0.5f, 0.25f, 0.75f, 0.5f);
                glBlendFuncSeparate(GL_CONSTANT_COLOR, GL_ONE_MINUS_DST_ALPHA, GL_ONE, GL_ZERO); break;
        }
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    glBlendEquation(GL_FUNC_ADD); glBlendFunc(GL_ONE, GL_ZERO);
    glDisable(GL_BLEND);
}

static void s_colormask(void) {
    clear(0.5f, 0.5f, 0.5f);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.8f, -0.8f, 0, 1, 0.8f, -0.8f, 0, 1, -0.8f, 0.8f, 0, 1, 0.8f, 0.8f, 0, 1};
    float col[] = {1, 0, 1, 1, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glColorMask(GL_TRUE, GL_FALSE, GL_TRUE, GL_FALSE);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
}

static void s_tex_nearest(void) {
    clear(0, 0, 0);
    unsigned char img[8 * 8 * 4];
    for (int y = 0; y < 8; y++) for (int x = 0; x < 8; x++) {
        unsigned char *q = img + (y * 8 + x) * 4;
        int c = (x + y) & 1;
        q[0] = c ? 255 : 30; q[1] = x * 32; q[2] = y * 32; q[3] = 255;
    }
    texture(8, 8, GL_RGBA, GL_UNSIGNED_BYTE, img, GL_NEAREST);
    GLuint p = prog(VS_TEX, FS_TEX);
    quad(p, 0.9f, 1.0f);
}

static void s_tex_linear_mip(void) {
    clear(0, 0, 0);
    static unsigned char img[64 * 64 * 4];
    for (int y = 0; y < 64; y++) for (int x = 0; x < 64; x++) {
        unsigned char *q = img + (y * 64 + x) * 4;
        q[0] = x * 4; q[1] = y * 4; q[2] = ((x / 8 + y / 8) & 1) ? 255 : 0; q[3] = 255;
    }
    GLuint tex = texture(64, 64, GL_RGBA, GL_UNSIGNED_BYTE, img, GL_LINEAR);
    glBindTexture(GL_TEXTURE_2D, tex);
    glGenerateMipmap(GL_TEXTURE_2D);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_MIRRORED_REPEAT);
    GLuint p = prog(VS_TEX, FS_TEX);
    quad(p, 0.9f, 3.0f);
}

static void s_tex_formats(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, FS_TEX);
    static unsigned short u16[16 * 16];
    static unsigned char u8[16 * 16 * 3];
    for (int i = 0; i < 16 * 16; i++) { int x = i % 16, y = i / 16; u16[i] = (unsigned short)(x * 4096 + y * 256 + (x ^ y) * 17); }
    for (int i = 0; i < 16 * 16 * 3; i++) u8[i] = (unsigned char)(i * 7);
    struct { GLenum f, t; const void *d; } fmts[] = {
        {GL_RGB, GL_UNSIGNED_SHORT_5_6_5, u16}, {GL_RGBA, GL_UNSIGNED_SHORT_4_4_4_4, u16},
        {GL_RGBA, GL_UNSIGNED_SHORT_5_5_5_1, u16}, {GL_RGB, GL_UNSIGNED_BYTE, u8},
        {GL_LUMINANCE, GL_UNSIGNED_BYTE, u8}, {GL_ALPHA, GL_UNSIGNED_BYTE, u8},
        {GL_LUMINANCE_ALPHA, GL_UNSIGNED_BYTE, u8}, {GL_RGBA, GL_UNSIGNED_BYTE, u8},
    };
    glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ZERO);
    for (int i = 0; i < 8; i++) {
        texture(16, 16, fmts[i].f, fmts[i].t, fmts[i].d, GL_NEAREST);
        float x0 = -1 + (i % 4) * 0.5f, y0 = -1 + (i / 4) * 1.0f;
        float pos[] = {x0, y0, 0, 1, x0 + 0.5f, y0, 0, 1, x0, y0 + 1, 0, 1, x0 + 0.5f, y0 + 1, 0, 1};
        float uv[] = {0, 0, 1, 0, 0, 1, 1, 1};
        attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    glDisable(GL_BLEND);
}

static void s_tex_etc1(void) {
    clear(0, 0, 0);
    static unsigned char blocks[16 * 8];
    unsigned x = 12345;
    for (int i = 0; i < 16 * 8; i++) { x = x * 1103515245u + 12345u; blocks[i] = (unsigned char)(x >> 16); }
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glCompressedTexImage2D(GL_TEXTURE_2D, 0, GL_ETC1_RGB8_OES, 16, 16, 0, sizeof blocks, blocks);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint p = prog(VS_TEX, FS_TEX);
    quad(p, 0.9f, 1.0f);
}

static void s_cube(void) {
    clear(0.5f, 0.5f, 0.5f);
    glEnable(GL_DEPTH_TEST); glEnable(GL_CULL_FACE);
    GLuint p = prog(
        "attribute vec4 pos; attribute vec3 nrm; attribute vec3 col; uniform mat4 mvp; uniform mat4 mv; varying vec3 v;\n"
        "void main() { gl_Position = mvp * pos; vec3 n = normalize(mat3(mv[0].xyz, mv[1].xyz, mv[2].xyz) * nrm); vec4 p4 = mv * pos;\n"
        "  vec3 l = normalize(vec3(2.0, 2.0, 20.0) - p4.xyz / p4.w); v = col * max(0.0, dot(n, l)); }\n",
        "precision mediump float; varying vec3 v; void main() { gl_FragColor = vec4(v, 1.0); }\n");
    static const float P[6][4][3] = {
        {{-1,-1,1},{1,-1,1},{-1,1,1},{1,1,1}}, {{1,-1,-1},{-1,-1,-1},{1,1,-1},{-1,1,-1}},
        {{1,-1,1},{1,-1,-1},{1,1,1},{1,1,-1}}, {{-1,-1,-1},{-1,-1,1},{-1,1,-1},{-1,1,1}},
        {{-1,1,1},{1,1,1},{-1,1,-1},{1,1,-1}}, {{-1,-1,-1},{1,-1,-1},{-1,-1,1},{1,-1,1}}};
    static const float N[6][3] = {{0,0,1},{0,0,-1},{1,0,0},{-1,0,0},{0,1,0},{0,-1,0}};
    static const float C[6][3] = {{0,0,1},{1,0,1},{1,0,0},{0,1,0},{1,1,0},{0,1,1}};
    float pos[96], nrm[72], col[72];
    for (int f = 0; f < 6; f++) for (int v = 0; v < 4; v++) {
        for (int k = 0; k < 3; k++) { pos[(f * 4 + v) * 4 + k] = P[f][v][k]; nrm[(f * 4 + v) * 3 + k] = N[f][k]; col[(f * 4 + v) * 3 + k] = C[f][k]; }
        pos[(f * 4 + v) * 4 + 3] = 1;
    }
    float a = 0.6f, b = 0.8f, ca = cosf(a), sa = sinf(a), cb = cosf(b), sb = sinf(b);
    /* mv = translate(0,0,-6) * ry * rx, column-major */
    float mv[16] = {cb, sa * sb, -ca * sb, 0, 0, ca, sa, 0, sb, -sa * cb, ca * cb, 0, 0, 0, -6, 1};
    float n = 4, fa = 10, t = 1.2f;
    float pr[16] = {n / t,0,0,0, 0,n / t,0,0, 0,0,-(fa + n) / (fa - n),-1, 0,0,-2 * fa * n / (fa - n),0};
    float mvp[16];
    for (int i = 0; i < 4; i++) for (int j = 0; j < 4; j++) {
        mvp[j * 4 + i] = 0;
        for (int k = 0; k < 4; k++) mvp[j * 4 + i] += pr[k * 4 + i] * mv[j * 4 + k];
    }
    glUniformMatrix4fv(glGetUniformLocation(p, "mv"), 1, GL_FALSE, mv);
    glUniformMatrix4fv(glGetUniformLocation(p, "mvp"), 1, GL_FALSE, mvp);
    attr(p, "pos", 4, pos); attr(p, "nrm", 3, nrm); attr(p, "col", 3, col);
    for (int f = 0; f < 6; f++) glDrawArrays(GL_TRIANGLE_STRIP, f * 4, 4);
    glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);
}

static void s_stencil(void) {
    clear(0, 0, 0);
    glEnable(GL_STENCIL_TEST);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.7f, -0.7f, 0, 1, 0.7f, -0.7f, 0, 1, 0, 0.7f, 0, 1,
                   -1, -1, 0, 1, 1, -1, 0, 1, -1, 1, 0, 1, 1, 1, 0, 1};
    float col[] = {0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 1, 0.5f, 0, 1, 1, 0.5f, 0, 1, 1, 0.5f, 0, 1, 1, 0.5f, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glStencilFunc(GL_ALWAYS, 1, 0xff); glStencilOp(GL_KEEP, GL_KEEP, GL_REPLACE);
    glColorMask(GL_FALSE, GL_FALSE, GL_FALSE, GL_FALSE);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glStencilFunc(GL_NOTEQUAL, 1, 0xff); glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    glDrawArrays(GL_TRIANGLE_STRIP, 3, 4);
    glDisable(GL_STENCIL_TEST);
}

static void s_points_lines(void) {
    clear(0, 0, 0);
    GLuint p = prog("attribute vec4 pos; attribute vec4 col; varying vec4 v;\nvoid main(){gl_Position=pos; gl_PointSize=6.0; v=col;}\n", FS_COL);
    float pos[] = {-0.8f, -0.8f, 0, 1, -0.4f, -0.2f, 0, 1, 0.0f, 0.5f, 0, 1, 0.6f, -0.6f, 0, 1,
                   -0.9f, 0.8f, 0, 1, 0.9f, 0.6f, 0, 1, -0.5f, -0.9f, 0, 1, 0.7f, 0.9f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0.5f, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_POINTS, 0, 4);
    glDrawArrays(GL_LINES, 4, 4);
    glDrawArrays(GL_LINE_STRIP, 0, 4);
}

static void s_scissor(void) {
    clear(0, 0, 0);
    glEnable(GL_SCISSOR_TEST);
    glScissor(20, 30, 50, 40); glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glScissor(60, 10, 50, 100);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-1, -1, 0, 1, 1, -1, 0, 1, -1, 1, 0, 1, 1, 1, 0, 1};
    float col[] = {0, 1, 0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisable(GL_SCISSOR_TEST);
}

static void s_indexed(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.9f, -0.9f, 0, 1, -0.1f, -0.9f, 0, 1, -0.9f, -0.1f, 0, 1, -0.1f, -0.1f, 0, 1, -0.5f, 0.5f, 0, 1,
                   0.5f, 0.5f, 0, 1, 0.9f, 0.5f, 0, 1, 0.8f, 0.85f, 0, 1, 0.5f, 0.95f, 0, 1, 0.2f, 0.8f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 1, 1,
                   1, 1, 1, 1, 1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    GLushort strip[] = {0, 1, 2, 3, 4};
    glDrawElements(GL_TRIANGLE_STRIP, 5, GL_UNSIGNED_SHORT, strip);
    GLubyte fan[] = {5, 6, 7, 8, 9};
    glDrawElements(GL_TRIANGLE_FAN, 5, GL_UNSIGNED_BYTE, fan);
    GLuint tri[] = {1, 6, 3};
    glDrawElements(GL_TRIANGLES, 3, GL_UNSIGNED_INT, tri);
}

static void s_discard_fragcoord(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_COL, "precision highp float; varying vec4 v;\n"
                            "void main(){ if (mod(floor(gl_FragCoord.x / 8.0) + floor(gl_FragCoord.y / 8.0), 2.0) < 0.5) discard;\n"
                            "  gl_FragColor = vec4(gl_FragCoord.x / 128.0, gl_FragCoord.y / 128.0, gl_FrontFacing ? 1.0 : 0.0, 1.0); }\n");
    float pos[] = {-0.9f, -0.9f, 0, 1, 0.9f, -0.9f, 0, 1, -0.9f, 0.9f, 0, 1, 0.9f, 0.9f, 0, 1};
    float col[16] = {0};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static void s_derivatives_math(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#extension GL_OES_standard_derivatives : enable\nprecision highp float; varying vec2 t;\n"
                            "void main(){ float a = sin(t.x * 12.0) * 0.5 + 0.5; float b = exp2(t.y * 2.0) / 4.0; float c = length(vec2(dFdx(t.x), dFdy(t.y))) * 64.0;\n"
                            "  gl_FragColor = vec4(a, b, clamp(c, 0.0, 1.0), 1.0) * inversesqrt(1.0 + t.x) + vec4(pow(t.y, 2.2) * 0.2, log2(1.0 + t.x) * 0.3, 0.0, 0.0); }\n");
    quad(p, 0.95f, 1.0f);
}

static void s_branches(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "precision highp float; varying vec2 t; uniform vec4 k[4];\n"
                            "void main(){ vec3 c = vec3(0.0); float x = t.x;\n"
                            "  for (int i = 0; i < 4; i++) { if (x > k[i].x) c += k[i].yzw; else c -= k[i].yzw * 0.25; }\n"
                            "  if (t.y > 0.5) c = c.zxy; gl_FragColor = vec4(fract(c * 0.5 + 0.5), 1.0); }\n");
    float k[16] = {0.2f, 0.3f, 0.1f, 0.0f, 0.4f, 0.0f, 0.4f, 0.1f, 0.6f, 0.2f, 0.2f, 0.5f, 0.8f, 0.1f, 0.0f, 0.3f};
    glUniform4fv(glGetUniformLocation(p, "k"), 4, k);
    quad(p, 0.95f, 1.0f);
}

static void s_vs_math(void) {
    clear(0, 0, 0);
    /* a grid whose vertex colours come from vertex-shader maths (rcp, rsqrt, exp2, log2, floor, sign, min/max, select) */
    GLuint p = prog("attribute vec4 pos; varying vec4 v; uniform float u;\n"
                    "void main(){ float a = pos.x * 0.5 + 0.5, b = pos.y * 0.5 + 0.5; gl_Position = pos;\n"
                    "  v = vec4(fract(1.0 / (0.3 + a) * u), inversesqrt(0.2 + b) * 0.5, exp2(a) * 0.3 + log2(1.0 + b) * 0.4,\n"
                    "           a > b ? floor(a * 4.0) / 4.0 : max(sign(b - 0.5), 0.0) * min(a, 0.6)); }\n", FS_COL);
    glUniform1f(glGetUniformLocation(p, "u"), 1.7f);
    float pos[17 * 17 * 4]; GLushort idx[16 * 16 * 6]; int n = 0;
    for (int y = 0; y <= 16; y++) for (int x = 0; x <= 16; x++) {
        float *q = pos + (y * 17 + x) * 4; q[0] = -0.95f + x * 0.11875f; q[1] = -0.95f + y * 0.11875f; q[2] = 0; q[3] = 1;
    }
    for (int y = 0; y < 16; y++) for (int x = 0; x < 16; x++) {
        GLushort a = y * 17 + x;
        idx[n++] = a; idx[n++] = a + 1; idx[n++] = a + 17; idx[n++] = a + 1; idx[n++] = a + 18; idx[n++] = a + 17;
    }
    attr(p, "pos", 4, pos);
    glDrawElements(GL_TRIANGLES, n, GL_UNSIGNED_SHORT, idx);
}

static void s_cubemap(void) {
    clear(0, 0, 0);
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_CUBE_MAP, tex);
    for (int f = 0; f < 6; f++) {
        unsigned char img[4 * 4 * 4];
        for (int i = 0; i < 16; i++) { img[i * 4] = (f & 1) ? 255 : 40; img[i * 4 + 1] = (f & 2) ? 255 : 40; img[i * 4 + 2] = (f & 4) ? 255 : 40 + i * 10; img[i * 4 + 3] = 255; }
        glTexImage2D(GL_TEXTURE_CUBE_MAP_POSITIVE_X + f, 0, GL_RGBA, 4, 4, 0, GL_RGBA, GL_UNSIGNED_BYTE, img);
    }
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint p = prog(VS_TEX, "precision highp float; varying vec2 t; uniform samplerCube s;\n"
                            "void main(){ vec2 a = (t - 0.5) * 6.2831; vec3 d = vec3(cos(a.x) * cos(a.y * 0.5), sin(a.y * 0.5), sin(a.x) * cos(a.y * 0.5)); gl_FragColor = textureCube(s, d); }\n");
    quad(p, 0.95f, 1.0f);
    glBindTexture(GL_TEXTURE_CUBE_MAP, 0);
}

static void s_fbo_texture(void) {
    /* render a triangle into a 64x64 texture, then draw that texture (linear, wrapped) */
    GLint outer; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &outer);
    GLuint tex = texture(64, 64, GL_RGBA, GL_UNSIGNED_BYTE, NULL, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    GLuint fb; glGenFramebuffers(1, &fb); glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    glViewport(0, 0, 64, 64);
    glClearColor(0.2f, 0.2f, 0.0f, 1); glClear(GL_COLOR_BUFFER_BIT);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.8f, -0.8f, 0, 1, 0.8f, -0.5f, 0, 1, -0.3f, 0.9f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unattr();
    glBindFramebuffer(GL_FRAMEBUFFER, outer);
    glViewport(0, 0, W, H);
    clear(0, 0, 0);
    glBindTexture(GL_TEXTURE_2D, tex);
    p = prog(VS_TEX, FS_TEX);
    quad(p, 0.9f, 1.5f);
    glDeleteFramebuffers(1, &fb);
}

static void s_near_clip(void) {
    /* a floor plane running from in front of the camera to behind it */
    clear(0.2f, 0.3f, 0.4f);
    glEnable(GL_DEPTH_TEST);
    GLuint p = prog("attribute vec4 pos; attribute vec2 uv; uniform mat4 mvp; varying vec2 t;\n"
                    "void main(){ gl_Position = mvp * pos; t = uv; }\n",
                    "precision highp float; varying vec2 t;\n"
                    "void main(){ gl_FragColor = vec4(t, 1.0 - t.x * t.y, 1.0); }\n");
    float n = 0.5f, f = 50, s = 0.5f;
    /* perspective * translate(0, -1, 0): column-major */
    float mvp[16] = {n / s, 0, 0, 0, 0, n / s, 0, 0, 0, 0, -(f + n) / (f - n), -1, 0, -n / s, -2 * f * n / (f - n), 0};
    glUniformMatrix4fv(glGetUniformLocation(p, "mvp"), 1, GL_FALSE, mvp);
    float pos[] = {-4, 0, 3, 1, 4, 0, 3, 1, -4, 0, -20, 1, 4, 0, -20, 1};
    float uv[] = {0, 0, 1, 0, 0, 1, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisable(GL_DEPTH_TEST);
}

static void s_many_tris(void) {
    /* 20000 small triangles: more polygon list than the driver's first tile heap */
    clear(0, 0, 0);
    GLuint p = prog(VS_COL, FS_COL);
    static float pos[20000 * 3 * 4], col[20000 * 3 * 4];
    for (int i = 0; i < 20000; i++) {
        float cx = -0.95f + (i % 160) * 0.012f, cy = -0.95f + (i / 160) * 0.0152f;
        for (int v = 0; v < 3; v++) {
            float *q = pos + (i * 3 + v) * 4, *c = col + (i * 3 + v) * 4;
            q[0] = cx + (v == 1 ? 0.011f : 0); q[1] = cy + (v == 2 ? 0.014f : 0); q[2] = 0; q[3] = 1;
            c[0] = (i % 7) / 6.0f; c[1] = (i % 13) / 12.0f; c[2] = v / 2.0f; c[3] = 1;
        }
    }
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 20000 * 3);
}

static void s_polygon_offset(void) {
    clear(0, 0, 0);
    glEnable(GL_DEPTH_TEST); glDepthFunc(GL_LEQUAL);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-0.8f, -0.8f, 0.1f, 1, 0.8f, -0.8f, 0.3f, 1, -0.8f, 0.8f, 0.2f, 1, 0.8f, 0.8f, 0.4f, 1};
    float red[16] = {1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1};
    float green[16] = {0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, red);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glEnable(GL_POLYGON_OFFSET_FILL); glPolygonOffset(0, -4);
    attr(p, "col", 4, green);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glDisable(GL_POLYGON_OFFSET_FILL);
    glDisable(GL_DEPTH_TEST); glDepthFunc(GL_LESS);
}

static void s_loop_fragdepth(void) {
    /* a loop with a uniform trip count (not unrolled: a backward branch), a 32-entry uniform
     * array, and gl_FragDepthEXT deciding which of two overlapping quads wins */
    clear(0, 0, 0);
    glEnable(GL_DEPTH_TEST); glDepthFunc(GL_LESS);
    GLuint p = prog(VS_TEX, "#extension GL_EXT_frag_depth : enable\nprecision highp float; varying vec2 t;\n"
                            "uniform float n; uniform vec4 k[32];\n"
                            "void main(){ vec3 c = vec3(0.0); for (int i = 0; i < 100; i++) { if (float(i) >= n) break; c += k[i].xyz * t.x; }\n"
                            "  gl_FragColor = vec4(fract(c), 1.0); gl_FragDepthEXT = t.y; }\n");
    float k[32 * 4];
    for (int i = 0; i < 32 * 4; i++) k[i] = (float)((i * 37) % 23) / 23.0f;
    glUniform4fv(glGetUniformLocation(p, "k"), 32, k);
    glUniform1f(glGetUniformLocation(p, "n"), 27.0f);
    quad(p, 0.9f, 1.0f);
    /* a second quad, depth from 1 - t.y: it covers the upper half only */
    GLuint q = prog(VS_TEX, "#extension GL_EXT_frag_depth : enable\nprecision highp float; varying vec2 t;\n"
                            "void main(){ gl_FragColor = vec4(0.2, 0.8, 0.3, 1.0); gl_FragDepthEXT = 1.0 - t.y; }\n");
    quad(q, 0.7f, 1.0f);
    glDisable(GL_DEPTH_TEST);
}

struct scene { const char *name; void (*fn)(void); };
static const struct scene scenes[] = {
    {"clear", s_clear}, {"tri_color", s_tri_color}, {"depth", s_depth}, {"blend", s_blend},
    {"blend_modes", s_blend_modes}, {"colormask", s_colormask}, {"tex_nearest", s_tex_nearest},
    {"tex_linear_mip", s_tex_linear_mip}, {"tex_formats", s_tex_formats}, {"tex_etc1", s_tex_etc1}, {"cube", s_cube},
    {"stencil", s_stencil}, {"points_lines", s_points_lines}, {"scissor", s_scissor},
    {"indexed", s_indexed}, {"discard_fragcoord", s_discard_fragcoord},
    {"derivatives_math", s_derivatives_math}, {"branches", s_branches}, {"vs_math", s_vs_math},
    {"cubemap", s_cubemap}, {"fbo_texture", s_fbo_texture}, {"near_clip", s_near_clip},
    {"many_tris", s_many_tris}, {"polygon_offset", s_polygon_offset},
    {"loop_fragdepth", s_loop_fragdepth},
};
#define NSCENES (sizeof scenes / sizeof scenes[0])

static int run(const char *dir, const char *only) {
#if defined(__arm__)
    struct sigaction sa = {0};
    sa.sa_sigaction = crash; sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL); sigaction(SIGBUS, &sa, NULL); sigaction(SIGILL, &sa, NULL);
#endif
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay d = gpd(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    EGLint maj, min;
    if (!eglInitialize(d, &maj, &min)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (!ctx) { fprintf(stderr, "no GLES 2 context\n"); return 1; }
    eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
    printf("renderer: %s / %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));
    GLuint fb, rb[2];
    glGenFramebuffers(1, &fb); glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glGenRenderbuffers(2, rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[0]); glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, W, H);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[1]); glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8_OES, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, rb[1]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, rb[1]);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { fprintf(stderr, "framebuffer incomplete\n"); return 1; }
    glViewport(0, 0, W, H);
    static unsigned char px[W * H * 4];
    for (unsigned i = 0; i < NSCENES; i++) {
        if (only && strcmp(only, scenes[i].name)) continue;
        scenes[i].fn();
        unattr();
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
        char path[512]; snprintf(path, sizeof path, "%s/%s.rgba", dir, scenes[i].name);
        FILE *f = fopen(path, "wb"); fwrite(px, 1, sizeof px, f); fclose(f);
        GLenum e = glGetError();
        printf("scene %-20s done%s\n", scenes[i].name, e ? " (GL error)" : "");
        fflush(stdout);
    }
    return 0;
}

static int cmp(const char *a, const char *b) {
    static unsigned char pa[W * H * 4], pb[W * H * 4];
    int fails = 0;
    for (unsigned i = 0; i < NSCENES; i++) {
        char path[512];
        snprintf(path, sizeof path, "%s/%s.rgba", a, scenes[i].name);
        FILE *f = fopen(path, "rb"); if (!f || fread(pa, 1, sizeof pa, f) != sizeof pa) { printf("%-20s missing in %s\n", scenes[i].name, a); fails++; if (f) fclose(f); continue; } fclose(f);
        snprintf(path, sizeof path, "%s/%s.rgba", b, scenes[i].name);
        f = fopen(path, "rb"); if (!f || fread(pb, 1, sizeof pb, f) != sizeof pb) { printf("%-20s missing in %s\n", scenes[i].name, b); fails++; if (f) fclose(f); continue; } fclose(f);
        int maxd = 0, off = 0; double sum = 0;
        for (int p = 0; p < W * H; p++) {
            int pd = 0;
            for (int c = 0; c < 4; c++) { int dd = abs(pa[p * 4 + c] - pb[p * 4 + c]); if (dd > pd) pd = dd; sum += dd; }
            if (pd > maxd) maxd = pd;
            if (pd > 12) off++;
        }
        double pct = 100.0 * off / (W * H);
        /* line and point rasterisation rules are implementation-defined in GL ES */
        double limit = !strcmp(scenes[i].name, "points_lines") ? 3.0 : 1.0;
        int ok = pct < limit;
        if (!ok) fails++;
        printf("%-20s %s  pixels off by >12: %6.2f%%  max diff %3d  mean diff %.3f\n", scenes[i].name, ok ? "PASS" : "FAIL", pct, maxd, sum / (W * H * 4));
        snprintf(path, sizeof path, "%s/%s.ppm", a, scenes[i].name);
        f = fopen(path, "wb");
        fprintf(f, "P6 %d %d 255\n", W * 3, H);
        for (int y = H - 1; y >= 0; y--) for (int s = 0; s < 3; s++) for (int x = 0; x < W; x++) {
            int p = (y * W + x) * 4;
            unsigned char o[3];
            for (int c = 0; c < 3; c++) o[c] = s == 0 ? pa[p + c] : s == 1 ? pb[p + c] : (unsigned char)(abs(pa[p + c] - pb[p + c]) * 4 > 255 ? 255 : abs(pa[p + c] - pb[p + c]) * 4);
            fwrite(o, 1, 3, f);
        }
        fclose(f);
    }
    printf("%d of %u scenes differ\n", fails, (unsigned)NSCENES);
    return fails;
}

int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "run")) return run(argv[2], argc > 3 ? argv[3] : NULL);
    if (argc >= 4 && !strcmp(argv[1], "cmp")) return cmp(argv[2], argv[3]);
    fprintf(stderr, "usage: gles2test run DIR [SCENE] | gles2test cmp A B\n");
    return 2;
}
