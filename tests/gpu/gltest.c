/* Offscreen OpenGL ES test scenes for the emulated V3D, compared against Mesa's llvmpipe.
 *
 *   gltest run DIR [SCENE]  render every scene (or SCENE; NAME* for all starting with NAME)
 *                           into DIR/<scene>.rgba (128x128 RGBA8)
 *   gltest cmp A B       compare two runs: per-scene mismatch statistics, side-by-side PPMs
 *
 * Run once on the GPU and once with LIBGL_ALWAYS_SOFTWARE=1 (llvmpipe), then compare.
 * Built in the guest: gcc -O1 -o gltest gltest.c -lEGL -lGLESv2 -lm
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <GLES2/gl2ext.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

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

/* glCopyImageSubDataEXT, or NULL when the driver lacks GL_EXT_copy_image */
typedef void (*copy_image_fn)(GLuint, GLenum, GLint, GLint, GLint, GLint, GLuint, GLenum, GLint, GLint, GLint, GLint, GLsizei, GLsizei, GLsizei);
static copy_image_fn copy_image(void) {
    return strstr((const char *)glGetString(GL_EXTENSIONS), "GL_EXT_copy_image") ? (copy_image_fn)eglGetProcAddress("glCopyImageSubDataEXT") : NULL;
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

/* Text the way Firefox's WebRender draws it: a glyph mask texture and per-channel ("subpixel")
 * coverage, over a white and a coloured background. */
static GLuint glyph_tex(GLenum ifmt, GLenum fmt, int bgra) {
    static unsigned char img[32 * 32 * 4];
    for (int y = 0; y < 32; y++) for (int x = 0; x < 32; x++) {
        unsigned char *q = img + (y * 32 + x) * 4;
        int on = (x > 8 && x < 14) || (y > 20 && y < 25) || ((x - y) > -3 && (x - y) < 3 && x > 14);
        unsigned char r = on ? 255 : (x == 14 ? 128 : 0), g = on ? 230 : 0, b = on ? 200 : (x == 8 ? 90 : 0);
        if (ifmt == GL_R8) { img[y * 32 + x] = on ? 255 : 0; continue; }
        q[0] = bgra ? b : r; q[1] = g; q[2] = bgra ? r : b; q[3] = on ? 255 : 0;
    }
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, ifmt, 32, 32, 0, fmt, GL_UNSIGNED_BYTE, img);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    return t;
}

static void two_backgrounds(void) {
    clear(1, 1, 1);
    glEnable(GL_SCISSOR_TEST); glScissor(64, 0, 64, 128);
    glClearColor(0.2f, 0.5f, 0.9f, 1); glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
}

static void glyph_quads(GLuint p) {
    for (int k = 0; k < 4; k++) {
        float x0 = -0.95f + 0.48f * k, y0 = -0.6f, s = 0.4f;
        float pos[] = {x0, y0, 0, 1, x0 + s, y0, 0, 1, x0, y0 + 1.2f, 0, 1, x0 + s, y0 + 1.2f, 0, 1};
        float uv[] = {0, 0, 1, 0, 0, 1, 1, 1};
        attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
}

static void s_text_dual_source(void) {
    two_backgrounds();
    if (!strstr((const char *)glGetString(GL_EXTENSIONS), "GL_EXT_blend_func_extended")) { printf("no EXT_blend_func_extended\n"); return; }
    GLuint p = prog(VS_TEX,
        "#version 300 es\n#extension GL_EXT_blend_func_extended : require\nprecision mediump float; in vec2 t; uniform sampler2D s;\n"
        "layout(location = 0, index = 0) out vec4 o; layout(location = 0, index = 1) out vec4 m;\n"
        "void main() { vec4 c = vec4(0.1, 0.05, 0.3, 1.0); vec3 k = texture(s, t).rgb; o = vec4(c.rgb * k, max(k.r, max(k.g, k.b))); m = vec4(k, 1.0) * c.a; }\n");
    glyph_tex(GL_RGBA8, GL_RGBA, 0);
    glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC1_COLOR_EXT);
    glyph_quads(p);
    glDisable(GL_BLEND);
}

static void s_text_const_color(void) {
    two_backgrounds();
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision mediump float; in vec2 t; uniform sampler2D s; out vec4 o;\n"
                            "void main() { o = vec4(texture(s, t).rgb, 1.0); }\n");
    glyph_tex(GL_RGBA8, GL_RGBA, 0);
    glEnable(GL_BLEND); glBlendColor(0.1f, 0.05f, 0.3f, 1); glBlendFunc(GL_CONSTANT_COLOR, GL_ONE_MINUS_SRC_COLOR);
    glyph_quads(p);
    glDisable(GL_BLEND);
}

static void s_text_r8_alpha(void) {
    two_backgrounds();
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision mediump float; in vec2 t; uniform sampler2D s; out vec4 o;\n"
                            "void main() { float a = texture(s, t).r; o = vec4(0.6, 0.1, 0.1, 1.0) * a; }\n");
    glyph_tex(GL_R8, GL_RED, 0);
    glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
    glyph_quads(p);
    glDisable(GL_BLEND);
}

static void s_tex_bgra(void) {
    two_backgrounds();
    GLuint p = prog(VS_TEX, FS_TEX);
    if (!strstr((const char *)glGetString(GL_EXTENSIONS), "GL_EXT_texture_format_BGRA8888")) { printf("no BGRA8888\n"); return; }
    glyph_tex(GL_BGRA_EXT, GL_BGRA_EXT, 1);
    glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
    glyph_quads(p);
    /* the same texture data through RGBA8 and a B/R texture swizzle */
    glyph_tex(GL_RGBA8, GL_RGBA, 1);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R, GL_BLUE); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B, GL_RED);
    float pos[] = {-0.95f, -0.98f, 0, 1, 0.95f, -0.98f, 0, 1, -0.95f, -0.7f, 0, 1, 0.95f, -0.7f, 0, 1};
    float uv[] = {0, 0, 1, 0, 0, 1, 1, 1};
    attr(p, "pos", 4, pos); attr(p, "uv", 2, uv);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisable(GL_BLEND);
}

/* WebRender's other habits: a big glyph atlas filled through a pixel-unpack buffer with a row
 * stride, vertex data fetched from float/int textures, and 2D array textures. */
static void s_atlas_pbo(void) {
    two_backgrounds();
    GLuint t[2]; glGenTextures(2, t);
    static unsigned char src[40 * 36 * 4];
    for (int y = 0; y < 36; y++) for (int x = 0; x < 40; x++) {
        int on = ((x / 5 + y / 5) & 1) && x >= 4 && y >= 2;
        unsigned char *q = src + (y * 40 + x) * 4;
        q[0] = on ? 40 : 0; q[1] = on ? 220 : 0; q[2] = on ? 255 : 0; q[3] = on ? 255 : 0;
    }
    GLuint pbo; glGenBuffers(1, &pbo); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
    glBufferData(GL_PIXEL_UNPACK_BUFFER, sizeof src, src, GL_STREAM_DRAW);
    glBindTexture(GL_TEXTURE_2D, t[0]); glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 1024, 1024);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 40); glPixelStorei(GL_UNPACK_SKIP_PIXELS, 4); glPixelStorei(GL_UNPACK_SKIP_ROWS, 2);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 517, 301, 32, 32, GL_RGBA, GL_UNSIGNED_BYTE, 0);
    glBindTexture(GL_TEXTURE_2D, t[1]); glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, 2048, 2048);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 160); glPixelStorei(GL_UNPACK_SKIP_PIXELS, 16); glPixelStorei(GL_UNPACK_SKIP_ROWS, 2);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 1301, 777, 100, 32, GL_RED, GL_UNSIGNED_BYTE, 0);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 0); glPixelStorei(GL_UNPACK_SKIP_PIXELS, 0); glPixelStorei(GL_UNPACK_SKIP_ROWS, 0);
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
    /* each 3x3 screen block shows one texel: left half by texelFetch, right half by texture()
     * at the texel centre with LINEAR filtering (as glyph quads sample); top rows the RGBA8 atlas,
     * bottom rows the R8 one */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D s; uniform highp sampler2D r; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy) / 3; bool fetch = gl_FragCoord.x < 64.0; ivec2 k = ivec2(q.x % 21, q.y % 21);\n"
                            " if (gl_FragCoord.y >= 64.0) { ivec2 c = ivec2(517, 301) + k + ivec2(0, 5);\n"
                            "   o = fetch ? texelFetch(s, c, 0) : texture(s, (vec2(c) + 0.5) / 1024.0); }\n"
                            " else { ivec2 c = ivec2(1301, 777) + k + ivec2(9, 3);\n"
                            "   float b = fetch ? texelFetch(r, c, 0).r : texture(r, (vec2(c) + 0.5) / 2048.0).r; o = vec4(b, b * 0.5, 0.0, 1.0); } }\n");
    glUniform1i(glGetUniformLocation(p, "s"), 0); glUniform1i(glGetUniformLocation(p, "r"), 1);
    for (int i = 0; i < 2; i++) {
        glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, t[i]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    }
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f);
}

static void s_vs_fetch(void) {
    clear(0.05f, 0.05f, 0.05f);
    static float f[1024 * 8 * 4]; static int iv[512 * 4 * 4];
    for (int i = 0; i < 1024 * 8; i++) { f[i * 4] = -1; f[i * 4 + 1] = -1; f[i * 4 + 2] = 0; f[i * 4 + 3] = 1; }
    /* six vertices (two triangles) in row 3 from x = 600; colours in an int texture row 2 from 300 */
    float v[6][2] = {{-0.8f, -0.7f}, {0.7f, -0.8f}, {-0.6f, 0.8f}, {0.7f, -0.8f}, {0.8f, 0.7f}, {-0.6f, 0.8f}};
    for (int k = 0; k < 6; k++) { float *q = f + ((3 * 1024) + 600 + k) * 4; q[0] = v[k][0]; q[1] = v[k][1]; q[2] = 0.25f * k; q[3] = 1; }
    for (int k = 0; k < 6; k++) { int *q = iv + ((2 * 512) + 300 + k) * 4; q[0] = 40 * k; q[1] = 255 - 30 * k; q[2] = 100 + k; q[3] = -7; }
    GLuint t[2]; glGenTextures(2, t);
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t[0]);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA32F, 1024, 8); glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1024, 8, GL_RGBA, GL_FLOAT, f);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, t[1]);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA32I, 512, 4); glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 512, 4, GL_RGBA_INTEGER, GL_INT, iv);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glActiveTexture(GL_TEXTURE0);
    GLuint p = prog("#version 300 es\nuniform highp sampler2D d; uniform highp isampler2D c; out vec4 v;\n"
                    "void main() { vec4 a = texelFetch(d, ivec2(600 + gl_VertexID, 3), 0); ivec4 k = texelFetch(c, ivec2(300 + gl_VertexID, 2), 0);\n"
                    " gl_Position = vec4(a.xy, 0.0, 1.0); v = vec4(vec3(k.xyz) / 255.0, a.z + (k.w == -7 ? 0.0 : 1.0)); }\n",
                    "#version 300 es\nprecision mediump float; in vec4 v; out vec4 o; void main() { o = vec4(v.rgb, 1.0) + vec4(0.0, 0.0, v.a * 0.2, 0.0); }\n");
    glUniform1i(glGetUniformLocation(p, "d"), 0); glUniform1i(glGetUniformLocation(p, "c"), 1);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glBindVertexArray(0); glDeleteVertexArrays(1, &vao);
}

static void s_tex_array(void) {
    two_backgrounds();
    static unsigned char img[4][64 * 64 * 4];
    for (int l = 0; l < 4; l++) for (int y = 0; y < 64; y++) for (int x = 0; x < 64; x++) {
        unsigned char *q = img[l] + (y * 64 + x) * 4;
        q[0] = l * 60; q[1] = ((x / 8 + y / 8) & 1) ? 255 : 0; q[2] = x * 4; q[3] = 255;
    }
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D_ARRAY, t);
    glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, 64, 64, 4);
    glTexSubImage3D(GL_TEXTURE_2D_ARRAY, 0, 0, 0, 0, 64, 64, 4, GL_RGBA, GL_UNSIGNED_BYTE, img);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision mediump float; precision mediump sampler2DArray; in vec2 t; uniform sampler2DArray s; out vec4 o;\n"
                            "void main() { o = texture(s, vec3(t, floor(t.x * 4.0))); }\n");
    quad(p, 0.9f, 1.0f);
}

/* WebRender's opaque pass: front-to-back rectangles with depth write, z ids a hair apart
 * (1 - id / 2^22), LEQUAL; then an alpha pass tested against them without depth writes. */
static void s_depth_ids(void) {
    clear(1, 1, 1);
    glEnable(GL_DEPTH_TEST); glDepthMask(GL_TRUE);
    GLuint p = prog("#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\nvoid main() { gl_Position = pos; v = col; }\n", FS_COL);
    for (int pass = 0; pass < 2; pass++) {
        glDepthFunc(pass ? GL_LEQUAL : GL_LESS);
        for (int i = 0; i < 24; i++) {
            int id = 1000 + 24 - i + pass * 40;     /* front to back: higher id = nearer */
            float z = 1.0f - (float)id / (1 << 22), zc = z * 2 - 1;
            float x0 = -0.95f + 0.07f * i, y0 = pass ? 0.05f : -0.95f, x1 = x0 + 0.3f, y1 = y0 + 0.9f - 0.02f * i;
            float pos[] = {x0, y0, zc, 1, x1, y0, zc, 1, x0, y1, zc, 1, x1, y1, zc, 1};
            float c[16]; for (int k = 0; k < 4; k++) { c[k * 4] = (i * 37 % 255) / 255.0f; c[k * 4 + 1] = (i * 91 % 255) / 255.0f; c[k * 4 + 2] = pass ? 0.8f : 0.2f; c[k * 4 + 3] = 1; }
            attr(p, "pos", 4, pos); attr(p, "col", 4, c);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
    }
    /* alpha pass: a translucent band behind some rectangles and in front of others */
    glDepthMask(GL_FALSE); glDepthFunc(GL_LEQUAL); glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
    float z = 1.0f - 1012.0f / (1 << 22), zc = z * 2 - 1;
    float pos[] = {-1, -0.3f, zc, 1, 1, -0.3f, zc, 1, -1, 0.3f, zc, 1, 1, 0.3f, zc, 1};
    float c[] = {0.5f, 0.4f, 0, 0.5f, 0.5f, 0.4f, 0, 0.5f, 0.5f, 0.4f, 0, 0.5f, 0.5f, 0.4f, 0, 0.5f};
    attr(p, "pos", 4, pos); attr(p, "col", 4, c);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisable(GL_BLEND); glDepthMask(GL_TRUE); glDisable(GL_DEPTH_TEST);
}

/* Instanced rectangles from integer per-instance attributes (glVertexAttribIPointer + divisor),
 * a unit-quad vertex buffer, flat integer varyings. */
static void s_instanced_int(void) {
    clear(0.95f, 0.95f, 0.9f);
    GLuint p = prog("#version 300 es\nin vec2 corner; in ivec4 rect; in ivec4 colr; flat out ivec4 c; flat out int k;\n"
                    "void main() { vec2 xy = vec2(rect.xy) + corner * vec2(rect.zw); gl_Position = vec4(xy / 64.0 - 1.0, 0.0, 1.0); c = colr; k = gl_InstanceID; }\n",
                    "#version 300 es\nprecision mediump float; flat in ivec4 c; flat in int k; out vec4 o;\n"
                    "void main() { o = vec4(vec3(c.rgb) / 255.0, 1.0); if ((k & 3) == 3) o.b = 1.0; }\n");
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    float corner[] = {0, 0, 1, 0, 0, 1, 1, 1};
    static int inst[40][8];
    for (int i = 0; i < 40; i++) {
        inst[i][0] = 3 + (i % 8) * 15; inst[i][1] = 3 + (i / 8) * 25; inst[i][2] = 12 + i % 3; inst[i][3] = 20 - i % 5;
        inst[i][4] = i * 6; inst[i][5] = 255 - i * 6; inst[i][6] = (i * 50) % 256; inst[i][7] = 0;
    }
    GLuint b[2]; glGenBuffers(2, b);
    glBindBuffer(GL_ARRAY_BUFFER, b[0]); glBufferData(GL_ARRAY_BUFFER, sizeof corner, corner, GL_STATIC_DRAW);
    GLint lc = glGetAttribLocation(p, "corner"), lr = glGetAttribLocation(p, "rect"), lk = glGetAttribLocation(p, "colr");
    glEnableVertexAttribArray(lc); glVertexAttribPointer(lc, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glBindBuffer(GL_ARRAY_BUFFER, b[1]); glBufferData(GL_ARRAY_BUFFER, sizeof inst, inst, GL_STATIC_DRAW);
    glEnableVertexAttribArray(lr); glVertexAttribIPointer(lr, 4, GL_INT, 32, (void *)0); glVertexAttribDivisor(lr, 1);
    glEnableVertexAttribArray(lk); glVertexAttribIPointer(lk, 4, GL_INT, 32, (void *)16); glVertexAttribDivisor(lk, 1);
    glDrawArraysInstanced(GL_TRIANGLE_STRIP, 0, 4, 40);
    glBindVertexArray(0); glDeleteVertexArrays(1, &vao); glBindBuffer(GL_ARRAY_BUFFER, 0);
}

/* WebRender's GPU-cache updates: 1-pixel points scattered into RGBA32F and RGBA32I targets
 * (1024 wide), then read back by texelFetch and shown. */
static void s_scatter128(void) {
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLuint t[2], f[2]; glGenTextures(2, t); glGenFramebuffers(2, f);
    GLenum ifmt[2] = {GL_RGBA32F, GL_RGBA32I};
    for (int i = 0; i < 2; i++) {
        glBindTexture(GL_TEXTURE_2D, t[i]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[i], 1024, 64);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glBindFramebuffer(GL_FRAMEBUFFER, f[i]); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t[i], 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) printf("scatter128: target %d incomplete\n", i);
        glViewport(0, 0, 1024, 64);
        if (i == 0) { float z[4] = {0.25f, 0.25f, 0.25f, 1}; glClearBufferfv(GL_COLOR, 0, z); }
        else { int z[4] = {64, 64, 64, 255}; glClearBufferiv(GL_COLOR, 0, z); }
        GLuint p = prog(i == 0
            ? "#version 300 es\nin vec2 aPosition; in vec4 aValue; out vec4 v;\nvoid main() { v = aValue; gl_Position = vec4((aPosition + 0.5) / vec2(1024.0, 64.0) * 2.0 - 1.0, 0.0, 1.0); gl_PointSize = 1.0; }\n"
            : "#version 300 es\nin vec2 aPosition; in vec4 aValue; flat out ivec4 v;\nvoid main() { v = ivec4(aValue); gl_Position = vec4((aPosition + 0.5) / vec2(1024.0, 64.0) * 2.0 - 1.0, 0.0, 1.0); gl_PointSize = 1.0; }\n",
            i == 0 ? "#version 300 es\nprecision highp float; in vec4 v; out vec4 o; void main() { o = v; }\n"
                   : "#version 300 es\nprecision highp float; precision highp int; flat in ivec4 v; out ivec4 o; void main() { o = v; }\n");
        static float pos[2048 * 2], val[2048 * 4];
        int n = 0;
        for (int y = 0; y < 32; y++) for (int x = 0; x < 64; x++) {
            int tx = 700 + x * 3 % 300, ty = 7 + y;   /* scattered, as cache rows fill */
            pos[n * 2] = tx; pos[n * 2 + 1] = ty;
            val[n * 4] = i ? (x * 4) : x / 63.0f; val[n * 4 + 1] = i ? (y * 8) : y / 31.0f; val[n * 4 + 2] = i ? ((x + y) & 1) * 255 : 0.5f + 0.5f * ((x ^ y) & 1); val[n * 4 + 3] = i ? 255 : 1;
            n++;
        }
        attr(p, "aPosition", 2, pos); attr(p, "aValue", 4, val);
        glDrawArrays(GL_POINTS, 0, n);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a; uniform highp isampler2D b; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy) / 2; ivec2 c = ivec2(700 + q.x % 64 * 3 % 300, 7 + q.y % 32);\n"
                            " o = gl_FragCoord.y < 64.0 ? texelFetch(a, c, 0) : vec4(texelFetch(b, c, 0)) / 255.0; }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t[0]);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, t[1]); glActiveTexture(GL_TEXTURE0);
    glUniform1i(glGetUniformLocation(p, "a"), 0); glUniform1i(glGetUniformLocation(p, "b"), 1);
    quad(p, 1.0f, 1.0f);
}

/* WebRender's batched glyph uploads: glyphs land in a small staging texture first and are then
 * copied into the big atlas at unaligned offsets, by glCopyImageSubData, glBlitFramebuffer or
 * glCopyTexSubImage2D; R8 (alpha glyphs) and RGBA8 (colour/subpixel glyphs). */
static void staged_copy(GLenum ifmt, GLenum fmt, int cpp, int method, int dx, int dy, GLuint atlas, int aw) {
    static unsigned char img[64 * 64 * 4];
    for (int y = 0; y < 64; y++) for (int x = 0; x < 64; x++)
        for (int c = 0; c < cpp; c++) img[(y * 64 + x) * cpp + c] = ((x / 3 + y / 4 + c) & 1) ? 230 - 40 * c - method * 30 : 20 + x;
    GLuint st; glGenTextures(1, &st); glBindTexture(GL_TEXTURE_2D, st); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt, 64, 64);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 64, 64, fmt, GL_UNSIGNED_BYTE, img);
    GLuint f[2]; glGenFramebuffers(2, f);
    if (method == 0) {
        copy_image_fn ci = copy_image();
        if (!ci) { printf("no copy_image\n"); }
        else ci(st, GL_TEXTURE_2D, 0, 5, 3, 0, atlas, GL_TEXTURE_2D, 0, dx, dy, 0, 30, 21, 1);
    } else if (method == 1) {
        glBindFramebuffer(GL_READ_FRAMEBUFFER, f[0]); glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, st, 0);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, f[1]); glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, atlas, 0);
        glBlitFramebuffer(5, 3, 35, 24, dx, dy, dx + 30, dy + 21, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    } else {
        glBindFramebuffer(GL_READ_FRAMEBUFFER, f[0]); glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, st, 0);
        glBindTexture(GL_TEXTURE_2D, atlas);
        glCopyTexSubImage2D(GL_TEXTURE_2D, 0, dx, dy, 5, 3, 30, 21);
    }
    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
    glDeleteFramebuffers(2, f); glDeleteTextures(1, &st);
    (void)aw;
}

static void s_glyph_copies(void) {
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLuint at[2]; glGenTextures(2, at);
    GLenum ifmt[2] = {GL_R8, GL_RGBA8}, fmt[2] = {GL_RED, GL_RGBA};
    int size[2] = {2048, 1024};
    for (int k = 0; k < 2; k++) {
        glBindTexture(GL_TEXTURE_2D, at[k]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], size[k], size[k]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint f; glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, at[k], 0);
        glClearColor(0, 0, 0, 0); glClear(GL_COLOR_BUFFER_BIT);
        glBindFramebuffer(GL_FRAMEBUFFER, prev); glDeleteFramebuffers(1, &f);
        for (int m = 0; m < 3; m++) staged_copy(ifmt[k], fmt[k], k ? 4 : 1, m, 301 + m * 37, 517 + k * 3, at[k], size[k]);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(0.1f, 0.1f, 0.1f);
    /* 6 bands (2 formats x 3 methods), each a 2x-zoomed 32x21 window around its copy */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a; uniform highp sampler2D b; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy) / 2; int band = q.y / 21; int m = band % 3; int k = band / 3;\n"
                            " if (band > 5 || q.x > 33) { o = vec4(0.1); return; }\n"
                            " ivec2 c = ivec2(301 + m * 37 - 1 + q.x, 517 + k * 3 - 1 + q.y % 21);\n"
                            " o = k == 0 ? vec4(texelFetch(a, c, 0).r, 0.3, 0.0, 1.0) : texelFetch(b, c, 0); }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, at[0]);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, at[1]); glActiveTexture(GL_TEXTURE0);
    glUniform1i(glGetUniformLocation(p, "a"), 0); glUniform1i(glGetUniformLocation(p, "b"), 1);
    quad(p, 1.0f, 1.0f);
}

/* Large textures sampled across their whole area (glyph atlases grow to 2048x2048 and more):
 * every tiling and XOR/padding case of the address layout, not just one window. Four
 * quadrants: R8 2048², RGBA8 2048², R8 1024x4096, RG8 2048x512. */
static unsigned char pat(int x, int y, int c) { unsigned h = (unsigned)(x * 73856093) ^ (unsigned)(y * 19349663) ^ (unsigned)(c * 83492791); h ^= h >> 13; h *= 0x5bd1e995; return (unsigned char)(h >> 24); }

static void s_big_tex_layout(void) {
    struct { GLenum ifmt, fmt; int cpp, w, h; } t[4] = {{GL_R8, GL_RED, 1, 2048, 2048}, {GL_RGBA8, GL_RGBA, 4, 2048, 2048}, {GL_R8, GL_RED, 1, 1024, 4096}, {GL_RG8, GL_RG, 2, 2048, 512}};
    GLuint tex[4]; glGenTextures(4, tex);
    unsigned char *img = malloc(2048 * 2048 * 4);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    for (int k = 0; k < 4; k++) {
        for (int y = 0; y < t[k].h; y++) for (int x = 0; x < t[k].w; x++) for (int c = 0; c < t[k].cpp; c++) img[((size_t)y * t[k].w + x) * t[k].cpp + c] = pat(x, y, c + k);
        glActiveTexture(GL_TEXTURE0 + k); glBindTexture(GL_TEXTURE_2D, tex[k]);
        glTexStorage2D(GL_TEXTURE_2D, 1, t[k].ifmt, t[k].w, t[k].h);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, t[k].w, t[k].h, t[k].fmt, GL_UNSIGNED_BYTE, img);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }
    free(img);
    glActiveTexture(GL_TEXTURE0);
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D s0, s1, s2, s3; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); int k = (q.x >= 64 ? 1 : 0) + (q.y >= 64 ? 2 : 0); ivec2 r = q % 64;\n"
                            " if (k == 0) o = vec4(texelFetch(s0, r * 32 + ivec2(r.y % 32, (r.x * 7) % 32), 0).r, 0.0, 0.0, 1.0);\n"
                            " else if (k == 1) o = texelFetch(s1, r * 32 + ivec2((r.y * 5) % 32, r.x % 32), 0);\n"
                            " else if (k == 2) o = vec4(0.0, texelFetch(s2, r * ivec2(16, 64) + ivec2(r.y % 16, (r.x * 3) % 64), 0).r, 0.0, 1.0);\n"
                            " else o = vec4(texelFetch(s3, r * ivec2(32, 8) + ivec2((r.y * 11) % 32, r.x % 8), 0).rg, 1.0, 1.0); }\n");
    for (int k = 0; k < 4; k++) { char n[4] = {'s', (char)('0' + k), 0}; glUniform1i(glGetUniformLocation(p, n), k); }
    quad(p, 1.0f, 1.0f);
    for (int k = 0; k < 4; k++) { glActiveTexture(GL_TEXTURE0 + k); glBindTexture(GL_TEXTURE_2D, 0); }
    glActiveTexture(GL_TEXTURE0); glDeleteTextures(4, tex);
}

/* Rendering into big XOR-tiled textures (glyph atlases are drawn into): small rectangles spread
 * over the whole of an R8 2048², an RGBA8 2048² and an R8 1024x4096 target, read back by
 * texelFetch. Each output pixel shows the centre of one rectangle. */
static void s_render_big_targets(void) {
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    struct { GLenum ifmt; int w, h; } t[3] = {{GL_R8, 2048, 2048}, {GL_RGBA8, 2048, 2048}, {GL_R8, 1024, 4096}};
    GLuint tex[3]; glGenTextures(3, tex);
    GLuint pc = prog("#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\nvoid main() { gl_Position = pos; v = col; }\n", FS_COL);
    for (int k = 0; k < 3; k++) {
        glBindTexture(GL_TEXTURE_2D, tex[k]); glTexStorage2D(GL_TEXTURE_2D, 1, t[k].ifmt, t[k].w, t[k].h);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint f; glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[k], 0);
        glViewport(0, 0, t[k].w, t[k].h); glClearColor(0, 0, 0, 0); glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(pc);
        static float pos[64 * 64 * 6 * 4], col[64 * 64 * 6 * 4];
        int n = 0;
        for (int j = 0; j < 64; j++) for (int i = 0; i < 64; i++) {
            /* a 6x6 rect near the centre of each (w/64 x h/64) cell, offset so it straddles tile edges */
            float cx = (i + 0.5f) * t[k].w / 64 + (i % 5), cy = (j + 0.5f) * t[k].h / 64 + (j % 7);
            float x0 = (cx - 3) / t[k].w * 2 - 1, x1 = (cx + 3) / t[k].w * 2 - 1, y0 = (cy - 3) / t[k].h * 2 - 1, y1 = (cy + 3) / t[k].h * 2 - 1;
            float v[6][2] = {{x0, y0}, {x1, y0}, {x0, y1}, {x1, y0}, {x1, y1}, {x0, y1}};
            for (int q = 0; q < 6; q++) {
                pos[n * 4] = v[q][0]; pos[n * 4 + 1] = v[q][1]; pos[n * 4 + 2] = 0; pos[n * 4 + 3] = 1;
                col[n * 4] = pat(i, j, k) / 255.0f; col[n * 4 + 1] = pat(i, j, k + 1) / 255.0f; col[n * 4 + 2] = pat(i, j, k + 2) / 255.0f; col[n * 4 + 3] = 1;
                n++;
            }
        }
        attr(pc, "pos", 4, pos); attr(pc, "col", 4, col);
        glDrawArrays(GL_TRIANGLES, 0, n);
        glBindFramebuffer(GL_FRAMEBUFFER, prev); glDeleteFramebuffers(1, &f);
    }
    glViewport(0, 0, W, H);
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D s0, s1, s2; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 r = q % 64; int k = (q.x >= 64 ? 1 : 0) + (q.y >= 64 ? 2 : 0);\n"
                            " if (k == 0) o = vec4(texelFetch(s0, (r * 2 + 1) * 16 + ivec2(r.x % 5, r.y % 7), 0).r, 0.0, 0.0, 1.0);\n"
                            " else if (k == 1) o = texelFetch(s1, (r * 2 + 1) * 16 + ivec2(r.x % 5, r.y % 7), 0);\n"
                            " else if (k == 2) o = vec4(0.0, texelFetch(s2, (r * 2 + 1) * ivec2(8, 32) + ivec2(r.x % 5, r.y % 7), 0).r, 0.0, 1.0);\n"
                            " else o = vec4(0.2, 0.2, 0.2, 1.0); }\n");
    for (int k = 0; k < 3; k++) { glActiveTexture(GL_TEXTURE0 + k); glBindTexture(GL_TEXTURE_2D, tex[k]); char nm[4] = {'s', (char)('0' + k), 0}; glUniform1i(glGetUniformLocation(p, nm), k); }
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f);
    for (int k = 0; k < 3; k++) { glActiveTexture(GL_TEXTURE0 + k); glBindTexture(GL_TEXTURE_2D, 0); }
    glActiveTexture(GL_TEXTURE0); glDeleteTextures(3, tex);
}

/* WebRender's GPU-cache updates by pixel buffer: many single-row (and a few two-row) uploads of
 * RGBA32F and RGBA32I data into different rows of 1024-wide textures, from one PBO at different
 * offsets, all before one draw reads them back. */
static void s_row_uploads(void) {
    GLuint t[2]; glGenTextures(2, t);
    GLenum ifmt[2] = {GL_RGBA32F, GL_RGBA32I}, fmt[2] = {GL_RGBA, GL_RGBA_INTEGER}, ty[2] = {GL_FLOAT, GL_INT};
    static float fd[64 * 56 * 4]; static int id[64 * 56 * 4];
    for (int i = 0; i < 64 * 56; i++) for (int c = 0; c < 4; c++) { fd[i * 4 + c] = ((i * 7 + c * 13) % 97) / 96.0f; id[i * 4 + c] = (i * 5 + c * 29) % 251; }
    for (int k = 0; k < 2; k++) {
        glBindTexture(GL_TEXTURE_2D, t[k]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 1024, 64);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint pbo; glGenBuffers(1, &pbo); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
        glBufferData(GL_PIXEL_UNPACK_BUFFER, sizeof fd, k ? (void *)id : (void *)fd, GL_STREAM_DRAW);
        /* rows 1..30: one row each, 56 texels at x = 3 * row, data from row-sized slices of the PBO */
        for (int r = 1; r <= 30; r++)
            glTexSubImage2D(GL_TEXTURE_2D, 0, 3 * r, r, 56, 1, fmt[k], ty[k], (void *)(intptr_t)(r * 56 * 16));
        /* rows 40..41 and 50..52: multi-row uploads */
        glPixelStorei(GL_UNPACK_ROW_LENGTH, 56);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 100, 40, 56, 2, fmt[k], ty[k], (void *)(intptr_t)(31 * 56 * 16));
        glTexSubImage2D(GL_TEXTURE_2D, 0, 700, 50, 40, 3, fmt[k], ty[k], (void *)(intptr_t)(35 * 56 * 16));
        glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0); glDeleteBuffers(1, &pbo);
    }
    clear(0, 0, 0);
    /* each output pixel (x, y): texel (x * 8 + y % 8 * 3 % 56 + ..., y / 2) of either texture */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a; uniform highp isampler2D b; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); int row = q.y % 64; ivec2 c = ivec2(q.x * 8 + (row % 7), row);\n"
                            " o = q.y < 64 ? texelFetch(a, c, 0) : vec4(texelFetch(b, c, 0)) / 255.0; }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t[0]);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, t[1]); glActiveTexture(GL_TEXTURE0);
    glUniform1i(glGetUniformLocation(p, "a"), 0); glUniform1i(glGetUniformLocation(p, "b"), 1);
    quad(p, 1.0f, 1.0f);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, 0); glActiveTexture(GL_TEXTURE0); glDeleteTextures(2, t);
}

/* WebRender's text batches: one glDrawElementsInstanced of a 6-index quad with ~1000 instances,
 * each fetching its rectangle from an RGBA32F data texture by gl_InstanceID and carrying an
 * integer instance attribute. */
static void s_many_instances(void) {
    clear(1, 1, 1);
    enum { N = 1024 };
    static float data[N * 4]; static int attr_i[N * 4];
    for (int i = 0; i < N; i++) {
        int gx = i % 32, gy = i / 32;
        data[i * 4] = gx * 4; data[i * 4 + 1] = gy * 4; data[i * 4 + 2] = 3; data[i * 4 + 3] = 3;
        attr_i[i * 4] = (i * 37) % 256; attr_i[i * 4 + 1] = (i * 91) % 256; attr_i[i * 4 + 2] = (i * 13) % 256; attr_i[i * 4 + 3] = i;
    }
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA32F, 1024, 1); glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1024, 1, GL_RGBA, GL_FLOAT, data);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint p = prog("#version 300 es\nuniform highp sampler2D d; in vec2 corner; in ivec4 inst; flat out ivec4 c;\n"
                    "void main() { vec4 r = texelFetch(d, ivec2(gl_InstanceID % 1024, 0), 0); vec2 xy = r.xy + corner * r.zw;\n"
                    " gl_Position = vec4(xy / 64.0 - 1.0, 0.0, 1.0); c = inst; }\n",
                    "#version 300 es\nprecision mediump float; flat in ivec4 c; out vec4 o; void main() { o = vec4(vec3(c.rgb) / 255.0, 1.0); if (c.w > 1023) o = vec4(1, 0, 1, 1); }\n");
    glUniform1i(glGetUniformLocation(p, "d"), 0);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    float corner[] = {0, 0, 1, 0, 1, 1, 0, 1}; unsigned short idx[] = {0, 1, 2, 0, 2, 3};
    GLuint b[3]; glGenBuffers(3, b);
    glBindBuffer(GL_ARRAY_BUFFER, b[0]); glBufferData(GL_ARRAY_BUFFER, sizeof corner, corner, GL_STATIC_DRAW);
    GLint lc = glGetAttribLocation(p, "corner"), li = glGetAttribLocation(p, "inst");
    glEnableVertexAttribArray(lc); glVertexAttribPointer(lc, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glBindBuffer(GL_ARRAY_BUFFER, b[1]); glBufferData(GL_ARRAY_BUFFER, sizeof attr_i, attr_i, GL_STATIC_DRAW);
    glEnableVertexAttribArray(li); glVertexAttribIPointer(li, 4, GL_INT, 16, 0); glVertexAttribDivisor(li, 1);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, b[2]); glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof idx, idx, GL_STATIC_DRAW);
    glDrawElementsInstanced(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, 0, N);
    glBindVertexArray(0); glDeleteVertexArrays(1, &vao); glBindBuffer(GL_ARRAY_BUFFER, 0); glDeleteTextures(1, &t);
}

/* WebRender's partial tile updates: a frame drawn with depth, then a dirty rectangle redrawn
 * with the scissor on: scissored colour and depth clears (Mesa draws those, the depth one with a
 * shader writing gl_FragDepth), then opaque rectangles writing depth and translucent "text"
 * tested against them (LEQUAL, no depth writes). */
static void dirty_pass(GLuint p, float zo, float za, float r, float g) {
    /* opaque background rectangle (depth write), then a translucent band in front of it */
    glDepthMask(GL_TRUE); glDepthFunc(GL_LEQUAL); glDisable(GL_BLEND);
    float zc = zo * 2 - 1;
    float pos[] = {-0.9f, -0.9f, zc, 1, 0.9f, -0.9f, zc, 1, -0.9f, 0.9f, zc, 1, 0.9f, 0.9f, zc, 1};
    float c[16]; for (int k = 0; k < 4; k++) { c[k * 4] = r; c[k * 4 + 1] = g; c[k * 4 + 2] = 0.9f; c[k * 4 + 3] = 1; }
    attr(p, "pos", 4, pos); attr(p, "col", 4, c); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDepthMask(GL_FALSE); glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
    zc = za * 2 - 1;
    for (int i = 0; i < 6; i++) {
        float y0 = -0.8f + i * 0.27f, y1 = y0 + 0.12f;
        float tp[] = {-0.7f, y0, zc, 1, 0.7f, y0, zc, 1, -0.7f, y1, zc, 1, 0.7f, y1, zc, 1};
        float tc[16]; for (int k = 0; k < 4; k++) { tc[k * 4] = 0.05f; tc[k * 4 + 1] = 0.05f; tc[k * 4 + 2] = 0.1f; tc[k * 4 + 3] = 0.9f; }
        attr(p, "pos", 4, tp); attr(p, "col", 4, tc); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    }
    glDisable(GL_BLEND); glDepthMask(GL_TRUE);
}

static void s_partial_update(void) {
    GLuint p = prog("#version 300 es\nin vec4 pos; in vec4 col; out vec4 v;\nvoid main() { gl_Position = pos; v = col; }\n", FS_COL);
    glEnable(GL_DEPTH_TEST);
    /* frame 1: whole target */
    glClearColor(1, 1, 1, 1); glClearDepthf(1); glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    dirty_pass(p, 1.0f - 100.0f / (1 << 22), 1.0f - 140.0f / (1 << 22), 0.9f, 0.9f);
    glFinish();
    /* frame 2: a dirty rectangle only, with higher z ids (nearer), like a new frame's items */
    glEnable(GL_SCISSOR_TEST); glScissor(20, 30, 70, 50);
    glClearColor(1, 1, 0.8f, 1); glClearDepthf(1); glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    dirty_pass(p, 1.0f - 300.0f / (1 << 22), 1.0f - 340.0f / (1 << 22), 0.7f, 1.0f);
    glDisable(GL_SCISSOR_TEST); glDisable(GL_DEPTH_TEST);
}

/* WebRender's GPU cache while still small: 1024x2 RGBA32F and 1024x1 RGBA32I targets (LT-tiled),
 * written by 1-pixel points, then read by a vertex shader that places quads from .xyw (as the
 * text shader reads glyph rectangles), so a wrong texel moves or drops a quad. */
static void s_small_cache(void) {
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLuint t[2], f[2]; glGenTextures(2, t); glGenFramebuffers(2, f);
    int hgt[2] = {2, 1};
    GLenum ifmt[2] = {GL_RGBA32F, GL_RGBA32I};
    for (int k = 0; k < 2; k++) {
        glBindTexture(GL_TEXTURE_2D, t[k]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 1024, hgt[k]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glBindFramebuffer(GL_FRAMEBUFFER, f[k]); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t[k], 0);
        glViewport(0, 0, 1024, hgt[k]);
        if (k == 0) { float z[4] = {0, 0, 0, 0}; glClearBufferfv(GL_COLOR, 0, z); } else { int z[4] = {0, 0, 0, 0}; glClearBufferiv(GL_COLOR, 0, z); }
        GLuint p = prog(k == 0
            ? "#version 300 es\nin vec2 aPosition; in vec4 aValue; out vec4 v;\nvoid main() { v = aValue; gl_Position = vec4((aPosition + 0.5) / vec2(1024.0, 2.0) * 2.0 - 1.0, 0.0, 1.0); gl_PointSize = 1.0; }\n"
            : "#version 300 es\nin vec2 aPosition; in vec4 aValue; flat out ivec4 v;\nvoid main() { v = ivec4(aValue); gl_Position = vec4((aPosition + 0.5) / vec2(1024.0, 1.0) * 2.0 - 1.0, 0.0, 1.0); gl_PointSize = 1.0; }\n",
            k == 0 ? "#version 300 es\nprecision highp float; in vec4 v; out vec4 o; void main() { o = v; }\n"
                   : "#version 300 es\nprecision highp float; precision highp int; flat in ivec4 v; out ivec4 o; void main() { o = v; }\n");
        static float pos[2048 * 2], val[2048 * 4];
        int n = 0;
        for (int i = 0; i < 256; i++) {
            /* float: quad i's rectangle at texel (3 + 2i, i & 1); int: its colour at texel 5 + 3i */
            int tx = k == 0 ? 3 + 2 * i : 5 + 3 * i, ty = k == 0 ? (i & 1) : 0;
            pos[n * 2] = tx; pos[n * 2 + 1] = ty;
            if (k == 0) { val[n * 4] = (i % 16) * 8; val[n * 4 + 1] = (i / 16) * 8; val[n * 4 + 2] = 99; val[n * 4 + 3] = 6; }
            else { val[n * 4] = (i * 37) % 256; val[n * 4 + 1] = (i * 91) % 256; val[n * 4 + 2] = 200; val[n * 4 + 3] = i; }
            n++;
        }
        attr(p, "aPosition", 2, pos); attr(p, "aValue", 4, val);
        glDrawArrays(GL_POINTS, 0, n);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(1, 1, 1);
    GLuint p = prog("#version 300 es\nuniform highp sampler2D a; uniform highp isampler2D b; in vec2 corner; flat out ivec4 c;\n"
                    "void main() { int i = gl_InstanceID; vec4 r = texelFetch(a, ivec2(3 + 2 * i, i & 1), 0); c = texelFetch(b, ivec2(5 + 3 * i, 0), 0);\n"
                    " vec2 xy = vec2(r.x, r.y) + corner * r.w; gl_Position = vec4(xy / 64.0 - 1.0, 0.0, 1.0); }\n",
                    "#version 300 es\nprecision mediump float; flat in ivec4 c; out vec4 o; void main() { o = vec4(vec3(c.rgb) / 255.0, 1.0); }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t[0]);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, t[1]); glActiveTexture(GL_TEXTURE0);
    glUniform1i(glGetUniformLocation(p, "a"), 0); glUniform1i(glGetUniformLocation(p, "b"), 1);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    float corner[] = {0, 0, 1, 0, 0, 1, 1, 1};
    GLuint vb; glGenBuffers(1, &vb); glBindBuffer(GL_ARRAY_BUFFER, vb); glBufferData(GL_ARRAY_BUFFER, sizeof corner, corner, GL_STATIC_DRAW);
    GLint lc = glGetAttribLocation(p, "corner"); glEnableVertexAttribArray(lc); glVertexAttribPointer(lc, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glDrawArraysInstanced(GL_TRIANGLE_STRIP, 0, 4, 256);
    glBindVertexArray(0); glDeleteVertexArrays(1, &vao); glBindBuffer(GL_ARRAY_BUFFER, 0);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, 0); glActiveTexture(GL_TEXTURE0); glDeleteTextures(2, t);
}

/* WebRender's GPU cache growing: data rows in a 1024x4 RGBA32F texture (and RGBA32I), copied
 * whole into a 1024x16 one (copy-image, else framebuffer blit), as the cache does when it needs
 * more rows; then read back. Each GL call's error is reported. */
#define GLCHK(what) do { GLenum e_ = glGetError(); if (e_) printf("cache_grow: %s -> GL error %#x\n", what, e_); } while (0)
static void s_cache_grow(void) {
    copy_image_fn ci = copy_image();
    printf("cache_grow: copy_image %s\n", ci ? "yes" : "no");
    GLenum ifmt[2] = {GL_RGBA32F, GL_RGBA32I}, fmt[2] = {GL_RGBA, GL_RGBA_INTEGER}, ty[2] = {GL_FLOAT, GL_INT};
    static float fd[1024 * 4 * 4]; static int id[1024 * 4 * 4];
    for (int i = 0; i < 4096; i++) for (int c = 0; c < 4; c++) { fd[i * 4 + c] = ((i * 11 + c * 7) % 101) / 100.0f; id[i * 4 + c] = (i * 3 + c * 41) % 253; }
    GLuint big[2][2] = {{0, 0}, {0, 0}};
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    for (int k = 0; k < 2; k++) {
        GLuint small; glGenTextures(1, &small); glBindTexture(GL_TEXTURE_2D, small);
        glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 1024, 4); GLCHK("storage small");
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1024, 4, fmt[k], ty[k], k ? (void *)id : (void *)fd); GLCHK("upload small");
        for (int m = 0; m < 2; m++) {
            glGenTextures(1, &big[k][m]); glBindTexture(GL_TEXTURE_2D, big[k][m]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 1024, 16); GLCHK("storage big");
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            if (m == 0) {
                if (ci) { ci(small, GL_TEXTURE_2D, 0, 0, 0, 0, big[k][m], GL_TEXTURE_2D, 0, 0, 0, 0, 1024, 4, 1); GLCHK("copy_image"); }
            } else {
                GLuint f[2]; glGenFramebuffers(2, f);
                glBindFramebuffer(GL_READ_FRAMEBUFFER, f[0]); glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, small, 0);
                glBindFramebuffer(GL_DRAW_FRAMEBUFFER, f[1]); glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, big[k][m], 0);
                glBlitFramebuffer(0, 0, 1024, 4, 0, 0, 1024, 4, GL_COLOR_BUFFER_BIT, GL_NEAREST); GLCHK("blit");
                glBindFramebuffer(GL_READ_FRAMEBUFFER, 0); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0); glDeleteFramebuffers(2, f);
            }
        }
        glDeleteTextures(1, &small);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev);
    clear(0, 0, 0);
    /* quadrants: float copy-image, float blit, int copy-image, int blit; texel (x * 16 + y % 16, y % 4) */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a0, a1; uniform highp isampler2D b0, b1; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 r = q % 64; ivec2 c = ivec2(r.x * 16 + r.y % 16, r.y % 4);\n"
                            " if (q.y < 64) o = q.x < 64 ? texelFetch(a0, c, 0) : texelFetch(a1, c, 0);\n"
                            " else o = vec4(q.x < 64 ? texelFetch(b0, c, 0) : texelFetch(b1, c, 0)) / 255.0; }\n");
    GLuint all[4] = {big[0][0], big[0][1], big[1][0], big[1][1]}; const char *nm[4] = {"a0", "a1", "b0", "b1"};
    for (int i = 0; i < 4; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, all[i]); glUniform1i(glGetUniformLocation(p, nm[i]), i); }
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f); GLCHK("draw");
    for (int i = 0; i < 4; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, 0); }
    glActiveTexture(GL_TEXTURE0); glDeleteTextures(4, all);
}

/* WebRender's batched texture uploads: a 512x512 staging texture filled through a pixel buffer,
 * then many small regions copied into a 2048x2048 atlas at other offsets (by drawing textured
 * quads into the atlas, and by copy-image), the staging texture refilled with new data, and
 * copied again; finally the atlas is sampled. R8 (glyph masks) and RGBA8. */
static void batch_round(GLuint staging, GLenum fmt, int cpp, int round, GLuint atlas_fbo, GLuint copy_prog, GLuint atlas, int use_ci) {
    static unsigned char data[512 * 512 * 4];
    for (int y = 0; y < 512; y++) for (int x = 0; x < 512; x++) for (int c = 0; c < cpp; c++)
        data[((size_t)y * 512 + x) * cpp + c] = (unsigned char)(((x / 7 + y / 5 + c + round * 3) & 1) ? 40 + round * 60 + c * 20 : 0);
    GLuint pbo; glGenBuffers(1, &pbo); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
    glBufferData(GL_PIXEL_UNPACK_BUFFER, 512 * 512 * cpp, NULL, GL_STREAM_DRAW);
    void *p = glMapBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, 512 * 512 * cpp, GL_MAP_WRITE_BIT | GL_MAP_INVALIDATE_BUFFER_BIT);
    memcpy(p, data, 512 * 512 * cpp); glUnmapBuffer(GL_PIXEL_UNPACK_BUFFER);
    glBindTexture(GL_TEXTURE_2D, staging);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 512, 512, fmt, GL_UNSIGNED_BYTE, 0);
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0); glDeleteBuffers(1, &pbo);
    copy_image_fn ci = copy_image();
    /* 24 regions of 20x24 from the staging texture into atlas cells */
    for (int k = 0; k < 24; k++) {
        int sx = (k % 8) * 60 + 3, sy = (k / 8) * 70 + 5;
        int dx = ((k + round * 24) % 32) * 64 + 2, dy = ((k + round * 24) / 32) * 32 + 1;
        if (use_ci && ci) ci(staging, GL_TEXTURE_2D, 0, sx, sy, 0, atlas, GL_TEXTURE_2D, 0, dx, dy, 0, 20, 24, 1);
        else {
            glBindFramebuffer(GL_FRAMEBUFFER, atlas_fbo); glViewport(0, 0, 2048, 2048);
            glUseProgram(copy_prog);
            glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, staging);
            float x0 = dx / 1024.0f - 1, x1 = (dx + 20) / 1024.0f - 1, y0 = dy / 1024.0f - 1, y1 = (dy + 24) / 1024.0f - 1;
            float u0 = sx / 512.0f, u1 = (sx + 20) / 512.0f, v0 = sy / 512.0f, v1 = (sy + 24) / 512.0f;
            float pos[] = {x0, y0, 0, 1, x1, y0, 0, 1, x0, y1, 0, 1, x1, y1, 0, 1};
            float uv[] = {u0, v0, u1, v0, u0, v1, u1, v1};
            attr(copy_prog, "pos", 4, pos); attr(copy_prog, "uv", 2, uv);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
    }
}

static void s_batched_uploads(void) {
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLenum ifmt[2] = {GL_R8, GL_RGBA8}, fmt[2] = {GL_RED, GL_RGBA};
    GLuint atlas[2][2];
    GLuint copy_prog = prog(VS_TEX, FS_TEX);
    for (int k = 0; k < 2; k++) for (int m = 0; m < 2; m++) {
        glGenTextures(1, &atlas[k][m]); glBindTexture(GL_TEXTURE_2D, atlas[k][m]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 2048, 2048);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint f; glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, atlas[k][m], 0);
        glViewport(0, 0, 2048, 2048); glClearColor(0, 0, 0, 0); glClear(GL_COLOR_BUFFER_BIT);
        GLuint st; glGenTextures(1, &st); glBindTexture(GL_TEXTURE_2D, st); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 512, 512);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        for (int round = 0; round < 3; round++) batch_round(st, fmt[k], k ? 4 : 1, round, f, copy_prog, atlas[k][m], m);
        glBindFramebuffer(GL_FRAMEBUFFER, prev); glDeleteFramebuffers(1, &f); glDeleteTextures(1, &st);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(0, 0, 0);
    /* quadrants: R8 by draw, R8 by copy-image, RGBA8 by draw, RGBA8 by copy-image; each output
     * pixel shows atlas texel (2 + 64 * (x/8) + x%8 * 3, 1 + 32 * (y/16) + y%16) */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a0, a1, b0, b1; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 r = q % 64; ivec2 c = ivec2(2 + 64 * (r.x / 2) + (r.x % 2) * 9, 1 + 32 * (r.y / 16) + (r.y % 16));\n"
                            " if (q.y < 64) o = q.x < 64 ? vec4(texelFetch(a0, c, 0).r, 0.2, 0.0, 1.0) : vec4(texelFetch(a1, c, 0).r, 0.0, 0.2, 1.0);\n"
                            " else o = q.x < 64 ? texelFetch(b0, c, 0) : texelFetch(b1, c, 0); }\n");
    GLuint all[4] = {atlas[0][0], atlas[0][1], atlas[1][0], atlas[1][1]}; const char *nm[4] = {"a0", "a1", "b0", "b1"};
    for (int i = 0; i < 4; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, all[i]); glUniform1i(glGetUniformLocation(p, nm[i]), i); }
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f);
    for (int i = 0; i < 4; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, 0); }
    glActiveTexture(GL_TEXTURE0); glDeleteTextures(4, all);
}

/* WebRender's staging uploads as it really does them: a persistently mapped pixel buffer
 * (glBufferStorage + MAP_PERSISTENT | MAP_FLUSH_EXPLICIT), staging data written at an offset into
 * it and flushed, then a whole 512x512 staging texture uploaded from that offset (R8, BGRA8 and
 * RGBA8), and regions copied from staging into a 2048x2048 atlas. Three rounds reuse the mapping. */
#define PSCHK(what) do { GLenum e_ = glGetError(); if (e_) printf("persistent_staging: %s k=%d -> GL error %#x\n", what, k, e_); } while (0)
static void s_persistent_staging(void) {
    typedef void (*buffer_storage_fn)(GLenum, GLsizeiptr, const void *, GLbitfield);
    buffer_storage_fn bs = strstr((const char *)glGetString(GL_EXTENSIONS), "GL_EXT_buffer_storage") ? (buffer_storage_fn)eglGetProcAddress("glBufferStorageEXT") : NULL;
    copy_image_fn ci = copy_image();
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    printf("persistent_staging: buffer_storage %s copy_image %s\n", bs ? "yes" : "no", ci ? "yes" : "no");
    if (!bs || !ci) { clear(0, 0, 0); return; }  /* a defined image, not the previous scene */
    GLenum ifmt[3] = {GL_R8, GL_BGRA8_EXT, GL_RGBA8}, fmt[3] = {GL_RED, GL_BGRA_EXT, GL_RGBA};
    int cpp[3] = {1, 4, 4};
    GLuint atlas[3];
    for (int k = 0; k < 3; k++) {
        glGenTextures(1, &atlas[k]); glBindTexture(GL_TEXTURE_2D, atlas[k]);
        if (k == 1) glTexImage2D(GL_TEXTURE_2D, 0, GL_BGRA_EXT, 2048, 2048, 0, GL_BGRA_EXT, GL_UNSIGNED_BYTE, NULL);
        else glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 2048, 2048);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        static unsigned char zero[2048 * 2048 * 4];
        glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 2048, 2048, fmt[k], GL_UNSIGNED_BYTE, zero); PSCHK("zero-fill");
        GLuint st; glGenTextures(1, &st); glBindTexture(GL_TEXTURE_2D, st);
        if (k == 1) glTexImage2D(GL_TEXTURE_2D, 0, GL_BGRA_EXT, 512, 512, 0, GL_BGRA_EXT, GL_UNSIGNED_BYTE, NULL);
        else glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 512, 512);
        /* without these the BGRA (glTexImage2D) staging texture is incomplete and copy-image may refuse it */
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint stf; glGenFramebuffers(1, &stf); glBindFramebuffer(GL_FRAMEBUFFER, stf);  /* staging is a render target in WebRender */
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, st, 0);
        glBindFramebuffer(GL_FRAMEBUFFER, prev);
        PSCHK("staging fbo"); size_t slice = 512 * 512 * cpp[k], size = slice * 3 + 4096;
        GLuint pbo; glGenBuffers(1, &pbo); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
        bs(GL_PIXEL_UNPACK_BUFFER, size, NULL, GL_MAP_WRITE_BIT | 0x0040 /* PERSISTENT */);
        unsigned char *map = glMapBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, size, GL_MAP_WRITE_BIT | 0x0040 | GL_MAP_FLUSH_EXPLICIT_BIT); PSCHK("storage+map");
        for (int round = 0; round < 3; round++) {
            size_t off = 4096 + round * slice;
            for (int y = 0; y < 512; y++) for (int x = 0; x < 512; x++) for (int c = 0; c < cpp[k]; c++)
                map[off + ((size_t)y * 512 + x) * cpp[k] + c] = (unsigned char)(((x / 6 + y / 5 + c + round) & 1) ? 50 + round * 60 + c * 25 : 0);
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
            glFlushMappedBufferRange(GL_PIXEL_UNPACK_BUFFER, off, slice); PSCHK("flush");
            glBindTexture(GL_TEXTURE_2D, st);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 512, 512, fmt[k], GL_UNSIGNED_BYTE, (void *)(intptr_t)off); PSCHK("pbo upload");
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
            for (int q = 0; q < 24; q++) {
                int sx = (q % 8) * 60 + 3, sy = (q / 8) * 70 + 5, dx = ((q + round * 24) % 32) * 64 + 2, dy = ((q + round * 24) / 32) * 32 + 1;
                ci(st, GL_TEXTURE_2D, 0, sx, sy, 0, atlas[k], GL_TEXTURE_2D, 0, dx, dy, 0, 20, 24, 1); PSCHK("copy image");
            }
            GLsync f = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0); glClientWaitSync(f, GL_SYNC_FLUSH_COMMANDS_BIT, 2000000000ull); glDeleteSync(f);
        }
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo); glUnmapBuffer(GL_PIXEL_UNPACK_BUFFER); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
        glDeleteBuffers(1, &pbo); glDeleteFramebuffers(1, &stf); glDeleteTextures(1, &st);
    }
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a, b, c; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 r = q % 64; ivec2 t = ivec2(2 + 64 * (r.x / 2) + (r.x % 2) * 9, 1 + 32 * (r.y / 16) + (r.y % 16));\n"
                            " if (q.y < 64) o = q.x < 64 ? vec4(texelFetch(a, t, 0).r, 0.2, 0.0, 1.0) : texelFetch(b, t, 0); else o = texelFetch(c, t, 0); }\n");
    const char *nm[3] = {"a", "b", "c"};
    for (int i = 0; i < 3; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, atlas[i]); glUniform1i(glGetUniformLocation(p, nm[i]), i); }
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f);
    for (int i = 0; i < 3; i++) { glActiveTexture(GL_TEXTURE0 + i); glBindTexture(GL_TEXTURE_2D, 0); }
    glActiveTexture(GL_TEXTURE0); glDeleteTextures(3, atlas);
}

/* Firefox's batched uploads, closer still: ONE persistently mapped pixel buffer reused every
 * round at offset 0 once the previous round's fence has signalled (polled with timeout 0, as
 * WebRender does), a 512x512 staging render target rotated from a pool of three, and regions
 * copied into the atlas with glBlitFramebuffer (what WebRender uses without GL_EXT_copy_image). */
static void s_wr_staging(void) {
    typedef void (*buffer_storage_fn)(GLenum, GLsizeiptr, const void *, GLbitfield);
    const char *ext = (const char *)glGetString(GL_EXTENSIONS);
    buffer_storage_fn bs = strstr(ext, "GL_EXT_buffer_storage") ? (buffer_storage_fn)eglGetProcAddress("glBufferStorageEXT") : NULL;
    printf("wr_staging: buffer_storage %s, EXT_copy_image %s, OES_copy_image %s\n", bs ? "yes" : "no",
           strstr(ext, "GL_EXT_copy_image") ? "yes" : "no", strstr(ext, "GL_OES_copy_image") ? "yes" : "no");
    if (!bs) { clear(0, 0, 0); return; }
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLenum ifmt[3] = {GL_R8, GL_RGBA8, GL_BGRA8_EXT}, fmt[3] = {GL_RED, GL_RGBA, GL_BGRA_EXT};
    int cpp[3] = {1, 4, 4};
    copy_image_fn ci = copy_image();
    GLuint atlas[3];
    for (int k = 0; k < 3; k++) {
        glGenTextures(1, &atlas[k]); glBindTexture(GL_TEXTURE_2D, atlas[k]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 2048, 2048);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLuint af; glGenFramebuffers(1, &af); glBindFramebuffer(GL_FRAMEBUFFER, af);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, atlas[k], 0);
        glViewport(0, 0, 2048, 2048); glClearColor(0, 0, 0, 0); glClear(GL_COLOR_BUFFER_BIT);
        GLuint st[3], sf[3];
        glGenTextures(3, st); glGenFramebuffers(3, sf);
        for (int i = 0; i < 3; i++) {
            glBindTexture(GL_TEXTURE_2D, st[i]); glTexStorage2D(GL_TEXTURE_2D, 1, ifmt[k], 512, 512);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glBindFramebuffer(GL_FRAMEBUFFER, sf[i]); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, st[i], 0);
        }
        size_t slice = 512 * 512 * cpp[k];
        GLuint pbo; glGenBuffers(1, &pbo); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
        bs(GL_PIXEL_UNPACK_BUFFER, slice, NULL, GL_MAP_WRITE_BIT | 0x0040);
        unsigned char *map = glMapBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, slice, GL_MAP_WRITE_BIT | 0x0040 | GL_MAP_FLUSH_EXPLICIT_BIT);
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
        GLsync fence = 0;
        int polls = 0;
        for (int round = 0; round < 5; round++) {
            if (fence) {
                /* about 10 s at most, so a job the GPU never finishes fails the scene instead of hanging the suite */
                for (int w = 0; glClientWaitSync(fence, 0, 0) == GL_TIMEOUT_EXPIRED; w++) {
                    if (w == 50000) { printf("wr_staging: fence not signalled after %d polls\n", w); break; }
                    polls++; usleep(200);
                }
                glDeleteSync(fence);
            }
            /* only the 24 source regions are written; everything else in the slice is zero, so
             * data from the wrong round shows up as missing or wrong cells */
            memset(map, 0, slice);
            for (int q = 0; q < 24; q++) {
                int sx = (q % 8) * 60 + 3, sy = (q / 8) * 70 + 5 + round * 2;
                for (int y = 0; y < 24; y++) for (int x = 0; x < 20; x++) for (int c = 0; c < cpp[k]; c++)
                    map[((size_t)(sy + y) * 512 + sx + x) * cpp[k] + c] = (unsigned char)(((x / 3 + y / 4 + c) & 1) ? 60 + round * 40 + c * 10 : 20);
            }
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
            glFlushMappedBufferRange(GL_PIXEL_UNPACK_BUFFER, 0, slice);
            glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
            glBindTexture(GL_TEXTURE_2D, st[round % 3]);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 512, 512, fmt[k], GL_UNSIGNED_BYTE, 0);
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
            glBindFramebuffer(GL_READ_FRAMEBUFFER, sf[round % 3]); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, af);
            for (int q = 0; q < 24; q++) {
                int sx = (q % 8) * 60 + 3, sy = (q / 8) * 70 + 5 + round * 2, dx = ((q + round * 24) % 32) * 64 + 2, dy = ((q + round * 24) / 32) * 32 + 1;
                if (k == 2 && ci) ci(st[round % 3], GL_TEXTURE_2D, 0, sx, sy, 0, atlas[k], GL_TEXTURE_2D, 0, dx, dy, 0, 20, 24, 1);
                else glBlitFramebuffer(sx, sy, sx + 20, sy + 24, dx, dy, dx + 20, dy + 24, GL_COLOR_BUFFER_BIT, GL_NEAREST);
            }
            { GLenum e = glGetError(); if (e) printf("wr_staging: k=%d round %d GL error %#x\n", k, round, e); }
            fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
            glFlush();
        }
        printf("wr_staging: k=%d fence polls %d\n", k, polls);
        if (fence) { glClientWaitSync(fence, GL_SYNC_FLUSH_COMMANDS_BIT, 2000000000ull); glDeleteSync(fence); }
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo); glUnmapBuffer(GL_PIXEL_UNPACK_BUFFER); glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
        glDeleteBuffers(1, &pbo);
        glBindFramebuffer(GL_FRAMEBUFFER, prev);
        glDeleteFramebuffers(3, sf); glDeleteTextures(3, st); glDeleteFramebuffers(1, &af);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a, b, c; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 r = q % 64; ivec2 t = ivec2(2 + 64 * (r.x / 2) + (r.x % 2) * 9, 1 + 32 * (r.y / 16) + (r.y % 16));\n"
                            " o = q.y >= 64 ? texelFetch(c, t, 0) : q.x < 64 ? vec4(texelFetch(a, t, 0).r, 0.2, 0.0, 1.0) : texelFetch(b, t, 0); }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, atlas[0]); glUniform1i(glGetUniformLocation(p, "a"), 0);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, atlas[1]); glUniform1i(glGetUniformLocation(p, "b"), 1);
    glActiveTexture(GL_TEXTURE2); glBindTexture(GL_TEXTURE_2D, atlas[2]); glUniform1i(glGetUniformLocation(p, "c"), 2);
    glActiveTexture(GL_TEXTURE0);
    quad(p, 1.0f, 1.0f);
    glActiveTexture(GL_TEXTURE2); glBindTexture(GL_TEXTURE_2D, 0); glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, 0); glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, 0);
    glDeleteTextures(3, atlas);
}

/* The glyph atlas written from both sides, as Firefox does with batched + PBO uploads: larger
 * glyphs go GPU-side (glTexSubImage2D from a pixel buffer, a render job into the atlas), small
 * ones are copied in from a staging texture (glCopyImageSubData, a CPU map of the atlas in
 * Mesa's V3D driver), and the atlas is sampled to the screen in between. */
static void s_mixed_atlas(void) {
    copy_image_fn ci = copy_image();
    if (!ci) { printf("mixed_atlas: no copy_image\n"); clear(0, 0, 0); return; }
    GLint prev; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prev);
    GLuint atlas, st;
    glGenTextures(1, &atlas); glBindTexture(GL_TEXTURE_2D, atlas); glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, 2048, 2048);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint af; glGenFramebuffers(1, &af); glBindFramebuffer(GL_FRAMEBUFFER, af);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, atlas, 0);
    glViewport(0, 0, 2048, 2048); glClearColor(0, 0, 0, 0); glClear(GL_COLOR_BUFFER_BIT);
    glBindFramebuffer(GL_FRAMEBUFFER, prev);
    glGenTextures(1, &st); glBindTexture(GL_TEXTURE_2D, st); glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, 512, 512);
    static unsigned char stage[512 * 512], big[64 * 64];
    GLuint pbo; glGenBuffers(1, &pbo);
    GLuint sp = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a; in vec2 t; out vec4 o;\n"
                             "void main() { o = vec4(texture(a, t).r, 0.0, 0.0, 1.0); }\n");
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    for (int i = 0; i < 48; i++) {
        /* a 64x64 "large glyph" through a pixel buffer: cells along row 0..2 of 64x64 slots */
        for (int y = 0; y < 64; y++) for (int x = 0; x < 64; x++) big[y * 64 + x] = (unsigned char)(((x / 8 + y / 8 + i) & 1) ? 100 + (i % 5) * 30 : 30);
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo); glBufferData(GL_PIXEL_UNPACK_BUFFER, sizeof big, big, GL_STREAM_DRAW);
        glBindTexture(GL_TEXTURE_2D, atlas);
        glTexSubImage2D(GL_TEXTURE_2D, 0, (i % 16) * 128, (i / 16) * 64, 64, 64, GL_RED, GL_UNSIGNED_BYTE, 0);
        glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
        /* four small glyphs through the staging texture (client memory), copied next to it */
        memset(stage, 0, sizeof stage);
        for (int q = 0; q < 4; q++) for (int y = 0; y < 16; y++) for (int x = 0; x < 12; x++)
            stage[(q * 20 + y) * 512 + x] = (unsigned char)(((x / 3 + y / 4 + q + i) & 1) ? 120 + q * 30 : 40);
        glBindTexture(GL_TEXTURE_2D, st); glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 512, 512, GL_RED, GL_UNSIGNED_BYTE, stage);
        for (int q = 0; q < 4; q++) ci(st, GL_TEXTURE_2D, 0, 0, q * 20, 0, atlas, GL_TEXTURE_2D, 0, (i % 16) * 128 + 64 + (q % 2) * 16, (i / 16) * 64 + (q / 2) * 20, 0, 12, 16, 1);
        /* sample the atlas (a "text draw") into the scene's framebuffer every few rounds */
        if (i % 3 == 2) {
            glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
            glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, atlas); glUniform1i(glGetUniformLocation(sp, "a"), 0);
            quad(sp, 0.1f, 0.1f);
            glFlush();
        }
    }
    glDeleteBuffers(1, &pbo); glDeleteTextures(1, &st); glDeleteFramebuffers(1, &af);
    glBindFramebuffer(GL_FRAMEBUFFER, prev); glViewport(0, 0, W, H);
    clear(0, 0, 0);
    /* show the used part of the atlas, (0,0)-(2048,192), scaled to the whole scene: 2048/W x 192/H */
    GLuint p = prog(VS_TEX, "#version 300 es\nprecision highp float; uniform highp sampler2D a; out vec4 o;\n"
                            "void main() { ivec2 q = ivec2(gl_FragCoord.xy); ivec2 t = ivec2(q.x * 16, q.y * 3 / 2);\n"
                            " o = vec4(texelFetch(a, t, 0).r, texelFetch(a, t + ivec2(2, 0), 0).r, texelFetch(a, t + ivec2(0, 1), 0).r, 1.0); }\n");
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, atlas); glUniform1i(glGetUniformLocation(p, "a"), 0);
    quad(p, 1.0f, 1.0f);
    glBindTexture(GL_TEXTURE_2D, 0); glDeleteTextures(1, &atlas);
}

struct scene { const char *name; void (*fn)(void); };
/* ---- shader control flow: loops reading uniform arrays / uniform blocks at computed indices and
 * sampling textures inside loops (Skia-style gradients in Chromium). On V3D each loop iteration
 * rewinds the uniform stream with a branch uniform, and non-constant uniform indices become TMU
 * general loads with a config uniform. */
static void ubo_stops(GLuint p, int n) {
    float b[16 * 4 * 2 + 4] = {0};
    for (int i = 0; i < 16; i++) {
        b[i * 4] = i < n ? (float)i / (n - 1) : 2.0f;
        b[64 + i * 4] = (i * 53 % 17) / 16.0f; b[64 + i * 4 + 1] = (i * 29 % 13) / 12.0f; b[64 + i * 4 + 2] = (i * 7 % 5) / 4.0f; b[64 + i * 4 + 3] = 1;
    }
    memcpy(&b[128], &n, 4);
    GLuint ub; glGenBuffers(1, &ub); glBindBuffer(GL_UNIFORM_BUFFER, ub);
    glBufferData(GL_UNIFORM_BUFFER, sizeof b, b, GL_STATIC_DRAW);
    glUniformBlockBinding(p, glGetUniformBlockIndex(p, "Stops"), 1);
    glBindBufferBase(GL_UNIFORM_BUFFER, 1, ub);
}
#define STOPS_BLOCK "layout(std140) uniform Stops { vec4 sp[16]; vec4 sc[16]; int sn; };\n"

static void s_sh_ubo_gradient(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX,
        "#version 300 es\nprecision highp float; in vec2 t; out vec4 o;\n" STOPS_BLOCK
        "void main() { float x = t.x; vec4 c = sc[0];\n"
        "  for (int i = 1; i < sn; i++) { if (x <= sp[i].x) { c = mix(sc[i - 1], sc[i], (x - sp[i - 1].x) / (sp[i].x - sp[i - 1].x)); break; } c = sc[i]; }\n"
        "  o = vec4(c.rgb * (0.4 + 0.6 * t.y), 1.0); }\n");
    ubo_stops(p, 7);
    quad(p, 1, 1);
}

static void s_sh_uarray_loop(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX,
        "#version 300 es\nprecision highp float; in vec2 t; out vec4 o; uniform vec4 k[24]; uniform int n; uniform float g;\n"
        "void main() { int j = int(t.x * 23.0) % n; vec4 a = vec4(0.0);\n"
        "  for (int i = 0; i <= j; i++) { if (i == 3 && t.y > 0.5) continue; a += k[i] * g; if (a.r > 2.5) break; }\n"
        "  int m = int(t.y * 7.0); for (int i = 0; i < m; i++) a.b += k[(i + j) % n].g * 0.1;\n"
        "  o = vec4(fract(a.rgb), 1.0); }\n");
    float k[24 * 4]; for (int i = 0; i < 24 * 4; i++) k[i] = ((i * 37) % 23) / 40.0f;
    glUniform4fv(glGetUniformLocation(p, "k"), 24, k);
    glUniform1i(glGetUniformLocation(p, "n"), 19);
    glUniform1f(glGetUniformLocation(p, "g"), 0.7f);
    quad(p, 1, 1);
}

static GLuint loop_tex(void) {
    unsigned char img[16 * 16 * 4];
    for (int i = 0; i < 256; i++) { img[i * 4] = i * 7; img[i * 4 + 1] = (i / 16) * 16; img[i * 4 + 2] = (i % 16) * 16; img[i * 4 + 3] = 255; }
    GLuint tx; glGenTextures(1, &tx); glBindTexture(GL_TEXTURE_2D, tx);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 16, 16, 0, GL_RGBA, GL_UNSIGNED_BYTE, img);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    return tx;
}

static void s_sh_tex_loop(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX,
        "#version 300 es\nprecision highp float; in vec2 t; out vec4 o; uniform sampler2D s; uniform int n; uniform vec2 d[8];\n" STOPS_BLOCK
        "void main() { vec4 a = vec4(0.0); int m = 1 + int(t.y * float(n));\n"
        "  for (int i = 0; i < m; i++) { vec2 q = t + d[i % 8] * sp[i % sn].x; if (q.x > 1.0) break; a += texture(s, q) * sc[i % sn]; }\n"
        "  o = vec4(a.rgb / float(m), 1.0); }\n");
    GLuint tx = loop_tex();
    glUniform1i(glGetUniformLocation(p, "s"), 0);
    glUniform1i(glGetUniformLocation(p, "n"), 6);
    float d[16]; for (int i = 0; i < 8; i++) { d[i * 2] = 0.03f * i; d[i * 2 + 1] = 0.05f * (i % 3); }
    glUniform2fv(glGetUniformLocation(p, "d"), 8, d);
    ubo_stops(p, 5);
    quad(p, 1, 1);
    glDeleteTextures(1, &tx);
}

/* the same in a vertex shader: 64 small quads placed by loops over a uniform array and a block */
static void vs_loop_grid(GLuint p) {
    float pos[64 * 6 * 2], id[64 * 6];
    static const float cx[6] = {0, 1, 0, 0, 1, 1}, cy[6] = {0, 0, 1, 1, 0, 1};
    for (int q = 0; q < 64; q++) for (int v = 0; v < 6; v++) { pos[(q * 6 + v) * 2] = cx[v]; pos[(q * 6 + v) * 2 + 1] = cy[v]; id[q * 6 + v] = q; }
    attr(p, "c", 2, pos); attr(p, "id", 1, id);
    glDrawArrays(GL_TRIANGLES, 0, 64 * 6);
}

static void s_sh_vs_loops(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(
        "#version 300 es\nprecision highp float; in vec2 c; in float id; out vec4 v; uniform vec4 k[16]; uniform int n;\n" STOPS_BLOCK
        "void main() { int q = int(id); vec2 o = vec2(float(q % 8), float(q / 8)) * 0.25 - 1.0; vec4 col = vec4(0.0);\n"
        "  for (int i = 0; i < n; i++) { if (i > q % 11) break; col += k[(q + i) % 16] * 0.3; }\n"
        "  for (int i = 1; i < sn; i++) { if (float(q) / 64.0 <= sp[i].x) { col.b += sc[i].b * 0.5; break; } }\n"
        "  gl_Position = vec4(o + c * 0.22, 0.0, 1.0); v = vec4(fract(col.rgb), 1.0); }\n",
        "#version 300 es\nprecision mediump float; in vec4 v; out vec4 o; void main() { o = v; }\n");
    float k[64]; for (int i = 0; i < 64; i++) k[i] = ((i * 41) % 29) / 28.0f;
    glUniform4fv(glGetUniformLocation(p, "k"), 16, k);
    glUniform1i(glGetUniformLocation(p, "n"), 9);
    ubo_stops(p, 6);
    vs_loop_grid(p);
}

static void s_sh_vs_tex_loop(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(
        "#version 300 es\nprecision highp float; in vec2 c; in float id; out vec4 v; uniform sampler2D s; uniform int n;\n" STOPS_BLOCK
        "void main() { int q = int(id); vec2 o = vec2(float(q % 8), float(q / 8)) * 0.25 - 1.0; vec4 a = vec4(0.0);\n"
        "  for (int i = 0; i <= q % n; i++) a += textureLod(s, vec2(float(q % 16) + 0.5, float(i) + 0.5) / 16.0, 0.0) * sc[(q + i) % sn];\n"
        "  gl_Position = vec4(o + c * 0.22, 0.0, 1.0); v = vec4(fract(a.rgb), 1.0); }\n",
        "#version 300 es\nprecision mediump float; in vec4 v; out vec4 o; void main() { o = v; }\n");
    GLuint tx = loop_tex();
    glUniform1i(glGetUniformLocation(p, "s"), 0);
    glUniform1i(glGetUniformLocation(p, "n"), 7);
    ubo_stops(p, 9);
    vs_loop_grid(p);
    glDeleteTextures(1, &tx);
}

/* ---- register spills and scratch memory: a chain of values that all stay live until the end
 * (more than the register file holds, so the compiler spills them through the TMU), and a local
 * array indexed at run time (lowered to scratch loads and stores). Large ANGLE/Skia shaders do both. */
#define SPILL_N 56
/* loop: 0 = straight line; 1 = the values are also used inside a loop whose trip count differs
 * per pixel/vertex (non-uniform control flow, so the spills there are conditional) */
static char *spill_src(int vertex, int loop) {
    static char b[32768]; int o = 0;
    o += sprintf(b + o, vertex
        ? "#version 300 es\nprecision highp float; in vec2 c; in float id; out vec4 v; uniform vec4 k[8]; uniform int n;\nvoid main() { int q = int(id);\n"
          "  vec2 t = vec2(float(q %% 8), float(q / 8)) / 8.0 + c * 0.1;\n"
        : "#version 300 es\nprecision highp float; in vec2 t; out vec4 o; uniform vec4 k[8]; uniform int n;\nvoid main() {\n");
    o += sprintf(b + o, "  vec4 a0 = sin(t.xyxy * k[0] + 0.3);\n");
    for (int i = 1; i < SPILL_N; i++) o += sprintf(b + o, "  vec4 a%d = sin(a%d * k[%d] + t.yxyx * %.2f);\n", i, i - 1, i % 8, 0.37 * i);
    o += sprintf(b + o, "  vec4 r = vec4(0.0);\n");
    if (loop) {
        o += sprintf(b + o, "  int m = n + int(t.x * 4.0 + t.y * 3.0);\n  for (int it = 0; it < m; it++) {\n    if (r.x > 3.0 + t.y) break;\n");
        for (int i = 0; i < SPILL_N / 2; i++) o += sprintf(b + o, "    r += a%d * a%d * 0.2;\n", i, SPILL_N - 1 - i);
        o += sprintf(b + o, "    if (t.x > 0.5) { r = r.yzwx; continue; }\n    r.w += a%d.x;\n  }\n", SPILL_N / 3);
    }
    for (int i = 0; i < SPILL_N / 2; i++) o += sprintf(b + o, "  r += a%d * a%d;\n", i, SPILL_N - 1 - i);
    o += sprintf(b + o, vertex
        ? "  gl_Position = vec4(vec2(float(q %% 8), float(q / 8)) * 0.25 - 1.0 + c * 0.22, 0.0, 1.0); v = vec4(0.5 + 0.5 * sin(r.rgb * 3.0), 1.0); }\n"
        : "  o = vec4(0.5 + 0.5 * sin(r.rgb * 3.0), 1.0); }\n");
    return b;
}
static const char *FS_V = "#version 300 es\nprecision mediump float; in vec4 v; out vec4 o; void main() { o = v; }\n";

static void spill_uniforms(GLuint p) {
    float k[32]; for (int i = 0; i < 32; i++) k[i] = 0.5f + ((i * 13) % 11) / 7.0f;
    glUniform4fv(glGetUniformLocation(p, "k"), 8, k);
    glUniform1i(glGetUniformLocation(p, "n"), 3);
}

static void s_sh_spill_fs(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, spill_src(0, 0));
    spill_uniforms(p);
    quad(p, 1, 1);
}

static void s_sh_spill_vs(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(spill_src(1, 0), FS_V);
    spill_uniforms(p);
    vs_loop_grid(p);
}

static void s_sh_spill_loop_fs(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX, spill_src(0, 1));
    spill_uniforms(p);
    quad(p, 1, 1);
}

static void s_sh_spill_loop_vs(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(spill_src(1, 1), FS_V);
    spill_uniforms(p);
    vs_loop_grid(p);
}

#define SCRATCH_BODY \
    "  vec4 arr[96]; for (int i = 0; i < 96; i++) arr[i] = k[i % 8] * (float(i) * 0.01 + t.x);\n" \
    "  int j = int(t.y * 95.0); for (int i = 0; i < n + int(t.x * 5.0); i++) { arr[(j + i) % 96] += arr[(j * 7 + i) % 96] * 0.25; }\n" \
    "  vec4 r = arr[j] + arr[(j * 5 + 3) % 96];\n"
static void s_sh_scratch_fs(void) {
    clear(0, 0, 0);
    GLuint p = prog(VS_TEX,
        "#version 300 es\nprecision highp float; in vec2 t; out vec4 o; uniform vec4 k[8]; uniform int n;\nvoid main() {\n"
        SCRATCH_BODY "  o = vec4(0.5 + 0.5 * sin(r.rgb * 3.0), 1.0); }\n");
    spill_uniforms(p);
    glUniform1i(glGetUniformLocation(p, "n"), 5);
    quad(p, 1, 1);
}

static void s_sh_scratch_vs(void) {
    clear(0.1f, 0.1f, 0.1f);
    GLuint p = prog(
        "#version 300 es\nprecision highp float; in vec2 c; in float id; out vec4 v; uniform vec4 k[8]; uniform int n;\nvoid main() { int q = int(id);\n"
        "  vec2 t = vec2(float(q % 8), float(q / 8)) / 8.0;\n" SCRATCH_BODY
        "  gl_Position = vec4(vec2(float(q % 8), float(q / 8)) * 0.25 - 1.0 + c * 0.22, 0.0, 1.0); v = vec4(0.5 + 0.5 * sin(r.rgb * 3.0), 1.0); }\n", FS_V);
    spill_uniforms(p);
    glUniform1i(glGetUniformLocation(p, "n"), 5);
    vs_loop_grid(p);
}

static const struct scene scenes[] = {
    {"clear", s_clear}, {"tri_color", s_tri_color}, {"flat", s_flat}, {"depth", s_depth}, {"blend", s_blend},
    {"tex_nearest", s_tex_nearest}, {"tex_linear_mip", s_tex_linear_mip}, {"cube", s_cube}, {"stencil", s_stencil},
    {"points_lines", s_points_lines}, {"scissor", s_scissor}, {"instanced_indexed", s_instanced_indexed},
    {"discard_fragcoord", s_discard_fragcoord}, {"derivatives_math", s_derivatives_math}, {"cubemap", s_cubemap},
    {"rgba16f_target", s_mrt_float}, {"text_dual_source", s_text_dual_source}, {"text_const_color", s_text_const_color},
    {"text_r8_alpha", s_text_r8_alpha}, {"tex_bgra", s_tex_bgra}, {"atlas_pbo", s_atlas_pbo}, {"vs_fetch", s_vs_fetch},
    {"tex_array", s_tex_array}, {"depth_ids", s_depth_ids}, {"instanced_int", s_instanced_int},
    {"scatter128", s_scatter128}, {"glyph_copies", s_glyph_copies},
    {"big_tex_layout", s_big_tex_layout}, {"render_big_targets", s_render_big_targets},
    {"row_uploads", s_row_uploads}, {"many_instances", s_many_instances}, {"partial_update", s_partial_update},
    {"small_cache", s_small_cache}, {"cache_grow", s_cache_grow},
    {"batched_uploads", s_batched_uploads}, {"persistent_staging", s_persistent_staging}, {"wr_staging", s_wr_staging}, {"mixed_atlas", s_mixed_atlas},
    {"sh_ubo_gradient", s_sh_ubo_gradient}, {"sh_uarray_loop", s_sh_uarray_loop}, {"sh_tex_loop", s_sh_tex_loop},
    {"sh_vs_loops", s_sh_vs_loops}, {"sh_vs_tex_loop", s_sh_vs_tex_loop},
    {"sh_spill_fs", s_sh_spill_fs}, {"sh_spill_vs", s_sh_spill_vs}, {"sh_scratch_fs", s_sh_scratch_fs}, {"sh_scratch_vs", s_sh_scratch_vs},
    {"sh_spill_loop_fs", s_sh_spill_loop_fs}, {"sh_spill_loop_vs", s_sh_spill_loop_vs},
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
    printf("renderer: %s / %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));
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
        size_t n = only ? strlen(only) : 0;  /* SCENE picks one; NAME* every scene starting with NAME */
        if (only && (n && only[n - 1] == '*' ? strncmp(only, scenes[i].name, n - 1) : strcmp(only, scenes[i].name))) continue;
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
