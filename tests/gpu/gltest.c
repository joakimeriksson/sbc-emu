/* Offscreen OpenGL ES test scenes for the emulated V3D, compared against Mesa's llvmpipe.
 *
 *   gltest run DIR       render every scene into DIR/<scene>.rgba (128x128 RGBA8)
 *   gltest cmp A B       compare two runs: per-scene mismatch statistics, side-by-side PPMs
 *
 * Run once on the GPU and once with LIBGL_ALWAYS_SOFTWARE=1 (llvmpipe), then compare.
 * Built in the guest: gcc -O1 -o gltest gltest.c -lEGL -lGLESv2 -lm
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

static const char *VS_COL =
    "#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\n"
    "void main() { gl_Position = pos; v = col; }\n";
static const char *FS_COL =
    "#version 300 es\nprecision mediump float; in vec4 v; out vec4 o;\n"
    "void main() { o = v; }\n";

static void clear(float r, float g, float b) {
    glClearColor(r, g, b, 1); glClearDepthf(1); glClearStencil(0);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
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

static void s_flat(void) {
    clear(0, 0, 0);
    GLuint p = prog("#version 300 es\nin vec4 pos; in vec4 col; flat out vec4 v;\nvoid main(){gl_Position=pos; v=col;}\n",
                    "#version 300 es\nprecision mediump float; flat in vec4 v; out vec4 o;\nvoid main(){o=v;}\n");
    float pos[] = {-0.9f, -0.9f, 0, 1, 0.9f, -0.9f, 0, 1, -0.9f, 0.9f, 0, 1, 0.9f, 0.9f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static void s_depth(void) {
    clear(0, 0, 0);
    glEnable(GL_DEPTH_TEST); glDepthFunc(GL_LESS);
    GLuint p = prog(VS_COL, FS_COL);
    /* near red triangle first, then a far green one overlapping: green only where red is absent */
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
    glBlendFunc(GL_ONE, GL_ONE); glBlendEquation(GL_FUNC_ADD);
    glDisable(GL_BLEND);
}

static const char *VS_TEX =
    "#version 300 es\nin vec4 pos; in vec2 uv; out vec2 t;\n"
    "void main() { gl_Position = pos; t = uv; }\n";
static const char *FS_TEX =
    "#version 300 es\nprecision mediump float; in vec2 t; uniform sampler2D s; out vec4 o;\n"
    "void main() { o = texture(s, t); }\n";

static void quad(GLuint p, float scale, float uvscale) {
    float pos[] = {-scale, -scale, 0, 1, scale, -scale, 0, 1, -scale, scale, 0, 1, scale, scale, 0, 1};
    float uv[] = {0, 0, uvscale, 0, 0, uvscale, uvscale, uvscale};
    attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static void s_tex_nearest(void) {
    clear(0, 0, 0);
    unsigned char img[8 * 8 * 4];
    for (int y = 0; y < 8; y++) for (int x = 0; x < 8; x++) {
        unsigned char *q = img + (y * 8 + x) * 4;
        int c = (x + y) & 1;
        q[0] = c ? 255 : 30; q[1] = x * 32; q[2] = y * 32; q[3] = 255;
    }
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 8, 8, 0, GL_RGBA, GL_UNSIGNED_BYTE, img);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
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
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, img);
    glGenerateMipmap(GL_TEXTURE_2D);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_MIRRORED_REPEAT);
    GLuint p = prog(VS_TEX, FS_TEX);
    quad(p, 0.9f, 3.0f);
}

static void mat_mul(float *r, const float *a, const float *b) {
    float t[16];
    for (int i = 0; i < 4; i++) for (int j = 0; j < 4; j++) {
        t[j * 4 + i] = 0;
        for (int k = 0; k < 4; k++) t[j * 4 + i] += a[k * 4 + i] * b[j * 4 + k];
    }
    memcpy(r, t, sizeof t);
}

static void s_cube(void) {
    clear(0.5f, 0.5f, 0.5f);
    glEnable(GL_DEPTH_TEST); glEnable(GL_CULL_FACE);
    GLuint p = prog(
        "#version 300 es\nin vec4 pos; in vec3 nrm; in vec3 col; uniform mat4 mvp; uniform mat4 mv; out vec3 v;\n"
        "void main() { gl_Position = mvp * pos; vec3 n = normalize(mat3(mv) * nrm); vec4 p4 = mv * pos;\n"
        "  vec3 l = normalize(vec3(2.0, 2.0, 20.0) - p4.xyz / p4.w); v = col * max(0.0, dot(n, l)); }\n",
        "#version 300 es\nprecision mediump float; in vec3 v; out vec4 o; void main() { o = vec4(v, 1.0); }\n");
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
    float a = 0.6f, b = 0.8f;
    float rx[16] = {1,0,0,0, 0,cosf(a),sinf(a),0, 0,-sinf(a),cosf(a),0, 0,0,0,1};
    float ry[16] = {cosf(b),0,-sinf(b),0, 0,1,0,0, sinf(b),0,cosf(b),0, 0,0,0,1};
    float tr[16] = {1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,-6,1};
    float n = 4, fa = 10, t = 1.2f;
    float pr[16] = {n / t,0,0,0, 0,n / t,0,0, 0,0,-(fa + n) / (fa - n),-1, 0,0,-2 * fa * n / (fa - n),0};
    float mv[16], mvp[16];
    mat_mul(mv, ry, rx); mat_mul(mv, tr, mv); mat_mul(mvp, pr, mv);
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
    GLuint p = prog("#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\nvoid main(){gl_Position=pos; gl_PointSize=6.0; v=col;}\n", FS_COL);
    float pos[] = {-0.8f, -0.8f, 0, 1, -0.4f, -0.2f, 0, 1, 0.0f, 0.5f, 0, 1, 0.6f, -0.6f, 0, 1,
                   -0.9f, 0.8f, 0, 1, 0.9f, 0.6f, 0, 1, -0.5f, -0.9f, 0, 1, 0.7f, 0.9f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0.5f, 0, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_POINTS, 0, 4);
    glDrawArrays(GL_LINES, 4, 4);
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

static void s_instanced_indexed(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog("#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\n"
                    "void main(){ float i = float(gl_InstanceID); gl_Position = pos + vec4(-0.6 + 0.6 * i, 0.2 * i, 0, 0); v = col * (0.5 + 0.25 * i) + vec4(0.0, float(gl_VertexID) * 0.1, 0.0, 0.0); }\n", FS_COL);
    float pos[] = {-0.3f, -0.5f, 0, 1, 0.1f, -0.5f, 0, 1, -0.3f, 0.0f, 0, 1, 0.1f, 0.0f, 0, 1, -0.1f, 0.4f, 0, 1};
    float col[] = {1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glEnable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
    GLushort idx[] = {0, 1, 2, 3, 0xffff, 2, 3, 4};
    glDrawElementsInstanced(GL_TRIANGLE_STRIP, 8, GL_UNSIGNED_SHORT, idx, 3);
    glDisable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
}

static void s_discard_fragcoord(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_COL, "#version 300 es\nprecision highp float; in vec4 v; out vec4 o;\n"
                            "void main(){ if (mod(floor(gl_FragCoord.x / 8.0) + floor(gl_FragCoord.y / 8.0), 2.0) < 0.5) discard;\n"
                            "  o = vec4(gl_FragCoord.x / 128.0, gl_FragCoord.y / 128.0, gl_FrontFacing ? 1.0 : 0.0, 1.0); }\n");
    float pos[] = {-0.9f, -0.9f, 0, 1, 0.9f, -0.9f, 0, 1, -0.9f, 0.9f, 0, 1, 0.9f, 0.9f, 0, 1};
    float col[16] = {0};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static void s_derivatives_math(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; in vec2 t; out vec4 o;\n"
                            "void main(){ float a = sin(t.x * 12.0) * 0.5 + 0.5; float b = exp2(t.y * 2.0) / 4.0; float c = length(vec2(dFdx(t.x), dFdy(t.y))) * 64.0;\n"
                            "  o = vec4(a, b, clamp(c, 0.0, 1.0), 1.0) * inversesqrt(1.0 + t.x) + vec4(pow(t.y, 2.2) * 0.2, log2(1.0 + t.x) * 0.3, 0.0, 0.0); }\n");
    quad(p, 0.95f, 1.0f);
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
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; in vec2 t; uniform samplerCube s; out vec4 o;\n"
                            "void main(){ vec2 a = (t - 0.5) * 6.2831; vec3 d = vec3(cos(a.x) * cos(a.y * 0.5), sin(a.y * 0.5), sin(a.x) * cos(a.y * 0.5)); o = texture(s, d); }\n");
    quad(p, 0.95f, 1.0f);
}

static void s_mrt_float(void) {
    clear(0, 0, 0);
    /* render to an RGBA16F target, then draw it to the screen with a texture lookup */
    GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA16F, 64, 64, 0, GL_RGBA, GL_HALF_FLOAT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLuint fb; glGenFramebuffers(1, &fb); glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    glViewport(0, 0, 64, 64);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    GLuint p = prog(VS_COL, FS_COL);
    float pos[] = {-1, -1, 0, 1, 1, -1, 0, 1, 0, 1, 0, 1};
    float col[] = {2.0f, 0, 0, 1, 0, 1.5f, 0, 1, 0, 0, 0.5f, 1};
    attr(p, "pos", 4, pos); attr(p, "col", 4, col);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glBindFramebuffer(GL_FRAMEBUFFER, prev);
    glViewport(0, 0, W, H);
    GLuint q = prog(VS_TEX, "#version 300 es\nprecision highp float; in vec2 t; uniform sampler2D s; out vec4 o;\n"
                            "void main(){ o = texture(s, t) * 0.5; }\n");
    quad(q, 0.9f, 1.0f);
}

static int s_compute(void) {
    /* GLES 3.1 compute: an SSBO filled by a shader with a barrier and shared memory; the result
     * is visualised as a texture-less gradient so it shows up in the image comparison too */
    const char *cs = "#version 310 es\nlayout(local_size_x = 64) in;\n"
                     "layout(std430, binding = 0) buffer B { uint v[]; };\nshared uint sh[64];\nuniform uint k;\n"
                     "void main(){ uint i = gl_GlobalInvocationID.x; sh[gl_LocalInvocationIndex] = i * i + k; barrier();\n"
                     "  v[i] = sh[63u - gl_LocalInvocationIndex] + gl_WorkGroupID.x * 1000u; }\n";
    GLuint s = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(s, 1, &cs, NULL); glCompileShader(s);
    GLint ok; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "cs: %s\n", log); return -1; }
    GLuint p = glCreateProgram(); glAttachShader(p, s); glLinkProgram(p); glUseProgram(p);
    glUniform1ui(glGetUniformLocation(p, "k"), 7);
    GLuint buf; glGenBuffers(1, &buf); glBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
    glBufferData(GL_SHADER_STORAGE_BUFFER, 256 * 4, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buf);
    glDispatchCompute(4, 1, 1);
    glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
    unsigned *v = glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, 256 * 4, GL_MAP_READ_BIT);
    int bad = 0;
    for (unsigned i = 0; i < 256; i++) {
        unsigned g = i / 64, j = 63 - i % 64, src = g * 64 + j;
        if (v[i] != src * src + 7 + g * 1000) { if (bad < 4) fprintf(stderr, "compute: v[%u] = %u, want %u\n", i, v[i], src * src + 7 + g * 1000); bad++; }
    }
    glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    printf("compute: %d of 256 values wrong\n", bad);
    return bad;
}

struct scene { const char *name; void (*fn)(void); };
static const struct scene scenes[] = {
    {"clear", s_clear}, {"tri_color", s_tri_color}, {"flat", s_flat}, {"depth", s_depth}, {"blend", s_blend},
    {"tex_nearest", s_tex_nearest}, {"tex_linear_mip", s_tex_linear_mip}, {"cube", s_cube}, {"stencil", s_stencil},
    {"points_lines", s_points_lines}, {"scissor", s_scissor}, {"instanced_indexed", s_instanced_indexed},
    {"discard_fragcoord", s_discard_fragcoord}, {"derivatives_math", s_derivatives_math}, {"cubemap", s_cubemap},
    {"rgba16f_target", s_mrt_float},
};
#define NSCENES (sizeof scenes / sizeof scenes[0])

static int run(const char *dir, const char *only) {
    EGLDisplay d;
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    d = gpd(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    EGLint maj, min;
    if (!eglInitialize(d, &maj, &min)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cattr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE};
    EGLContext ctx = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (!ctx) { fprintf(stderr, "no GLES 3.1 context\n"); return 1; }
    eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
    printf("renderer: %s\n", glGetString(GL_RENDERER));
    GLuint fb, rb[2];
    glGenFramebuffers(1, &fb); glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glGenRenderbuffers(2, rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[0]); glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, W, H);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[1]); glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, rb[1]);
    glViewport(0, 0, W, H);
    static unsigned char px[W * H * 4];
    for (unsigned i = 0; i < NSCENES; i++) {
        if (only && strcmp(only, scenes[i].name)) continue;
        scenes[i].fn();
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
        char path[512]; snprintf(path, sizeof path, "%s/%s.rgba", dir, scenes[i].name);
        FILE *f = fopen(path, "wb"); fwrite(px, 1, sizeof px, f); fclose(f);
        GLenum e = glGetError();
        printf("scene %-20s done%s\n", scenes[i].name, e ? " (GL error)" : "");
        fflush(stdout);
    }
    if (!only || !strcmp(only, "compute")) s_compute();
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
        int ok = pct < 1.0;
        if (!ok) fails++;
        printf("%-20s %s  pixels off by >12: %6.2f%%  max diff %3d  mean diff %.3f\n", scenes[i].name, ok ? "PASS" : "FAIL", pct, maxd, sum / (W * H * 4));
        /* side by side (a | b | diff) as a PPM, top row first */
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
    fprintf(stderr, "usage: gltest run DIR [SCENE] | gltest cmp A B\n");
    return 2;
}
