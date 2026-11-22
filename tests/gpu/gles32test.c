/* OpenGL ES 3.2 test scenes: tessellation, geometry and compute shaders, transform feedback,
 * queries, multisampling, several render targets, layered rendering, buffer textures, indirect
 * draws. Each scene renders 128 x 128 pixels:
 *   gles32test run DIR [SCENE]     render the scenes to DIR/SCENE.rgba
 *   gles32test cmp A B             compare two runs (the GPU's and llvmpipe's)
 * Build: gcc -O1 -o gles32test gles32test.c -lEGL -lGLESv2 -lm */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl32.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <GLES2/gl2ext.h>

enum { W = 128, H = 128 };
static GLuint fb, rb[2];

static GLuint shader(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    char all[16384], ext[512] = "";
    /* (a first line of its own goes before the precisions: #extension) */
    if (*src == 10) { const char *e = strchr(src + 1, 10); snprintf(ext, sizeof ext, "%.*s\n", (int)(e - src - 1), src + 1); src = e + 1; }
    snprintf(all, sizeof all, "#version 320 es\n%sprecision highp float; precision highp int; precision highp sampler2DArray; precision highp sampler2DMS; precision highp usampler2D; precision highp isampler2D; precision highp samplerBuffer; precision highp image2D;\n%s", ext, src);
    const char *p = all;
    glShaderSource(s, 1, &p, NULL); glCompileShader(s);
    GLint ok; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n%s\n", log, src); exit(1); }
    return s;
}
static GLuint link(GLuint p) {
    glLinkProgram(p);
    GLint ok; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[2048]; glGetProgramInfoLog(p, sizeof log, NULL, log); fprintf(stderr, "link: %s\n", log); exit(1); }
    return p;
}
static GLuint prog5(const char *vs, const char *tcs, const char *tes, const char *gs, const char *fs) {
    GLuint p = glCreateProgram();
    glAttachShader(p, shader(GL_VERTEX_SHADER, vs));
    if (tcs) glAttachShader(p, shader(GL_TESS_CONTROL_SHADER, tcs));
    if (tes) glAttachShader(p, shader(GL_TESS_EVALUATION_SHADER, tes));
    if (gs) glAttachShader(p, shader(GL_GEOMETRY_SHADER, gs));
    if (fs) glAttachShader(p, shader(GL_FRAGMENT_SHADER, fs));
    glUseProgram(link(p));
    return p;
}
static GLuint prog(const char *vs, const char *fs) { return prog5(vs, NULL, NULL, NULL, fs); }
static GLuint cprog(const char *cs) { GLuint p = glCreateProgram(); glAttachShader(p, shader(GL_COMPUTE_SHADER, cs)); glUseProgram(link(p)); return p; }
static void clear(float r, float g, float b) { glClearColor(r, g, b, 1); glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT); }
/* a full-screen triangle made from gl_VertexID */
#define VS_FULL "out vec2 uv; void main() { uv = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0); }\n"
static void full(const char *fs) { prog(VS_FULL, fs); glDrawArrays(GL_TRIANGLES, 0, 3); }
#define VS_ID "void main() { gl_Position = vec4(float(gl_VertexID % 4) * 0.5 - 0.75, float(gl_VertexID / 4) * 0.5 - 0.75, 0.0, 1.0); }\n"
#define FS_V "in vec4 v; out vec4 o; void main() { o = v; }\n"

static void s_compute_ssbo(void) {
    cprog("layout(local_size_x = 8, local_size_y = 4) in; layout(std430, binding = 0) buffer B { uint d[]; };\n"
          "void main() { uvec2 g = gl_GlobalInvocationID.xy; d[g.y * 128u + g.x] = packUnorm4x8(vec4(float(gl_LocalInvocationIndex) / 31.0, float(gl_WorkGroupID.x) / 15.0, float(gl_WorkGroupID.y) / 31.0, 1.0)); }");
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_SHADER_STORAGE_BUFFER, b); glBufferData(GL_SHADER_STORAGE_BUFFER, W * H * 4, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, b);
    glDispatchCompute(16, 32, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    full("layout(std430, binding = 0) readonly buffer B { uint d[]; }; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = unpackUnorm4x8(d[q.y * 128 + q.x]); }");
}
static void s_compute_shared(void) {
    /* a prefix sum with shared memory and barriers, then atomics */
    cprog("layout(local_size_x = 64) in; layout(std430, binding = 0) buffer B { uint total; uint mx; uint d[]; }; shared uint s[64]; shared uint cnt;\n"
          "void main() { uint i = gl_LocalInvocationIndex; s[i] = i * 3u % 7u; if (i == 0u) cnt = 0u; memoryBarrierShared(); barrier();\n"
          " for (uint k = 1u; k < 64u; k <<= 1) { uint a = s[i] + (i >= k ? s[i - k] : 0u); barrier(); s[i] = a; memoryBarrierShared(); barrier(); }\n"
          " atomicAdd(cnt, s[i] & 1u); barrier();\n"
          " d[gl_WorkGroupID.x * 64u + i] = s[i] + cnt * 256u + gl_WorkGroupID.x; atomicAdd(total, s[i]); atomicMax(mx, s[i] + gl_WorkGroupID.x * 5u); }");
    static GLuint zero[2 + 64 * 32];
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_SHADER_STORAGE_BUFFER, b); glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof zero, zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, b);
    glDispatchCompute(32, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    full("layout(std430, binding = 0) readonly buffer B { uint total; uint mx; uint d[]; }; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy) / ivec2(2, 4); uint v = d[q.y * 64 + q.x];\n"
         " o = vec4(float(v & 255u) / 255.0, float(v >> 8) / 40.0, gl_FragCoord.x < 64.0 ? float(total & 255u) / 255.0 : float(mx & 255u) / 255.0, 1.0); }");
}
static void s_compute_image(void) {
    GLuint t[2]; glGenTextures(2, t);
    for (int k = 0; k < 2; k++) { glBindTexture(GL_TEXTURE_2D, t[k]); glTexStorage2D(GL_TEXTURE_2D, 1, k ? GL_R32F : GL_RGBA8, W, H); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST); }
    glBindImageTexture(0, t[0], 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_RGBA8); glBindImageTexture(1, t[1], 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32F);
    cprog("layout(local_size_x = 16, local_size_y = 16) in; layout(rgba8, binding = 0) writeonly uniform image2D a; layout(r32f, binding = 1) uniform image2D b;\n"
          "void main() { ivec2 p = ivec2(gl_GlobalInvocationID.xy); imageStore(b, p, vec4(float(p.x ^ p.y) / 127.0)); ivec2 s = imageSize(a); imageStore(a, s - 1 - p, vec4(vec2(p) / vec2(s), 0.5, 1.0)); }");
    glDispatchCompute(W / 16, H / 16, 1);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
    cprog("layout(local_size_x = 16, local_size_y = 16) in; layout(r32f, binding = 1) uniform image2D b;\n"
          "void main() { ivec2 p = ivec2(gl_GlobalInvocationID.xy); float v = imageLoad(b, p).x; imageStore(b, p, vec4(v * v)); }");
    glDispatchCompute(W / 16, H / 16, 1);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_2D, t[1]); glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t[0]);
    GLuint p = prog(VS_FULL, "uniform sampler2D a, b; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = texelFetch(a, q, 0); o.b = texelFetch(b, q, 0).r; }");
    glUniform1i(glGetUniformLocation(p, "b"), 1);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}
static void s_compute_indirect(void) {
    GLuint b[2]; glGenBuffers(2, b);
    GLuint args[3] = {4, 3, 2};
    glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, b[0]); glBufferData(GL_DISPATCH_INDIRECT_BUFFER, sizeof args, args, GL_STATIC_DRAW);
    static GLuint zero[256];
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, b[1]); glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof zero, zero, GL_DYNAMIC_COPY); glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, b[1]);
    cprog("layout(local_size_x = 2, local_size_y = 3, local_size_z = 4) in; layout(std430, binding = 0) buffer B { uint d[]; };\n"
          "void main() { uvec3 g = gl_WorkGroupID; atomicAdd(d[g.x + g.y * 4u + g.z * 12u], gl_LocalInvocationIndex + gl_NumWorkGroups.x + gl_NumWorkGroups.y * 10u + gl_NumWorkGroups.z * 100u); atomicAdd(d[24u + gl_LocalInvocationID.z], 1u); }");
    glDispatchComputeIndirect(0);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    full("layout(std430, binding = 0) readonly buffer B { uint d[]; }; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy) / 16; uint v = d[q.y * 8 + q.x]; o = vec4(float(v & 255u), float((v >> 4) & 255u), float((v >> 8) & 255u), 255.0) / 255.0; }");
}
static void s_geometry_quads(void) {
    clear(0.1f, 0.1f, 0.2f);
    prog5(VS_ID, NULL, NULL,
          "layout(points) in; layout(triangle_strip, max_vertices = 4) out; out vec4 v;\n"
          "void main() { for (int i = 0; i < 4; i++) { vec2 c = vec2(float(i & 1), float(i >> 1)); v = vec4(c, float(gl_PrimitiveIDIn) / 15.0, 1.0); gl_Position = gl_in[0].gl_Position + vec4(c * 0.4 - 0.2, 0.0, 0.0); EmitVertex(); } EndPrimitive(); }",
          FS_V);
    glDrawArrays(GL_POINTS, 0, 16);
}
static void s_geometry_lines(void) {
    clear(0, 0, 0);
    /* several strips, invocations, primitive ids and points out of triangles */
    prog5("out vec4 c; void main() { float a = float(gl_VertexID) * 0.7; c = vec4(0.5 + 0.5 * cos(a), 0.5 + 0.5 * sin(a), 1.0, 1.0); gl_Position = vec4(vec2(cos(a), sin(a)) * (0.2 + 0.05 * float(gl_VertexID)), 0.0, 1.0); }", NULL, NULL,
          "layout(triangles, invocations = 3) in; layout(line_strip, max_vertices = 8) out; in vec4 c[]; out vec4 v;\n"
          "void main() { float s = 1.0 - 0.25 * float(gl_InvocationID); for (int k = 0; k < 2; k++) { for (int i = 0; i < 4; i++) { v = c[i % 3] * s; gl_PrimitiveID = gl_PrimitiveIDIn; gl_Position = vec4(gl_in[i % 3].gl_Position.xy * s + float(k) * 0.1, 0.0, 1.0); EmitVertex(); } EndPrimitive(); } }",
          "in vec4 v; out vec4 o; void main() { o = vec4(v.rg, float(gl_PrimitiveID) / 4.0, 1.0); }");
    glDrawArrays(GL_TRIANGLES, 0, 12);
}
static void s_geometry_adjacency(void) {
    clear(0.2f, 0, 0);
    prog5(VS_ID, NULL, NULL,
          "layout(lines_adjacency) in; layout(triangle_strip, max_vertices = 4) out; out vec4 v;\n"
          "void main() { v = vec4(1, 0, 0, 1); gl_Position = gl_in[1].gl_Position; EmitVertex(); v = vec4(0, 1, 0, 1); gl_Position = gl_in[2].gl_Position; EmitVertex(); v = vec4(0, 0, 1, 1); gl_Position = gl_in[0].gl_Position + vec4(0, 0.5, 0, 0); EmitVertex(); v = vec4(1); gl_Position = gl_in[3].gl_Position + vec4(0, 0.5, 0, 0); EmitVertex(); }",
          FS_V);
    glDrawArrays(GL_LINES_ADJACENCY, 0, 16);
}
static void s_geometry_layers(void) {
    GLuint t, f; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D_ARRAY, t); glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, 64, 64, 4);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferTexture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, t, 0);
    glViewport(0, 0, 64, 64); clear(0.1f, 0.2f, 0.3f);
    prog5("out vec2 p; void main() { p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(p - 0.9, 0.0, 1.0); }", NULL, NULL,
          "layout(triangles, invocations = 4) in; layout(triangle_strip, max_vertices = 3) out; in vec2 p[]; out vec4 v;\n"
          "void main() { for (int i = 0; i < 3; i++) { gl_Layer = gl_InvocationID; v = vec4(p[i] * 0.5, float(gl_InvocationID) / 3.0, 1.0); gl_Position = gl_in[i].gl_Position + vec4(float(gl_InvocationID) * 0.2, 0.0, 0.0, 0.0); EmitVertex(); } }",
          FS_V);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0, 0, W, H);
    full("uniform sampler2DArray s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = texelFetch(s, ivec3(q % 64, (q.x >> 6) + (q.y >> 6) * 2), 0); }");
    glDeleteFramebuffers(1, &f);
}
#define VS_PATCH "out vec2 c; uniform float ofs; void main() { c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)); gl_Position = vec4(c * 1.7 - 0.85, 0.0, 1.0); }\n"
static void tess(const char *tes_layout, const char *tcs_levels, int wire, const char *pos) {
    clear(0.05f, 0.05f, 0.1f);
    char tcs[2048], tes[2048];
    snprintf(tcs, sizeof tcs, "layout(vertices = 4) out; in vec2 c[]; out vec2 d[]; patch out vec4 tint; void main() { gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position; d[gl_InvocationID] = c[gl_InvocationID]; if (gl_InvocationID == 0) { tint = vec4(0.2, 0.4, 0.6, 1.0); } %s }", tcs_levels);
    snprintf(tes, sizeof tes, "layout(%s) in; in vec2 d[]; patch in vec4 tint; out vec4 v; void main() { vec3 t = gl_TessCoord; vec4 p = %s; v = tint + vec4(t.xy * 0.6, d[3].x * 0.2, 0.0); gl_Position = p; }", tes_layout, pos);
    prog5(VS_PATCH, tcs, tes, wire ? "layout(triangles) in; layout(line_strip, max_vertices = 4) out; in vec4 v[]; out vec4 w; void main() { for (int i = 0; i < 4; i++) { w = v[i % 3]; gl_Position = gl_in[i % 3].gl_Position; EmitVertex(); } }" : NULL,
          wire ? "in vec4 w; out vec4 o; void main() { o = w; }" : FS_V);
    glPatchParameteri(GL_PATCH_VERTICES, 4);
    glDrawArrays(GL_PATCHES, 0, 4);
}
#define MIXQ "mix(mix(gl_in[0].gl_Position, gl_in[1].gl_Position, t.x), mix(gl_in[2].gl_Position, gl_in[3].gl_Position, t.x), t.y) + vec4(sin(t.y * 6.0) * 0.1, sin(t.x * 6.0) * 0.1, 0.0, 0.0)"
#define MIXT "gl_in[0].gl_Position * t.x + gl_in[1].gl_Position * t.y + gl_in[2].gl_Position * t.z + vec4(sin(t.x * 9.0) * 0.1, 0.0, 0.0, 0.0)"
#define LEVELS(o, i) "gl_TessLevelOuter[0] = " o "; gl_TessLevelOuter[1] = " o "; gl_TessLevelOuter[2] = " o "; gl_TessLevelOuter[3] = " o "; gl_TessLevelInner[0] = " i "; gl_TessLevelInner[1] = " i ";"
static void s_tess_quads(void) { tess("quads, equal_spacing, ccw", LEVELS("12.0", "12.0"), 0, MIXQ); }
static void s_tess_quads_wire(void) { tess("quads, equal_spacing, ccw", LEVELS("4.0", "4.0"), 1, MIXQ); }
static void s_tess_triangles(void) { tess("triangles, equal_spacing, cw", LEVELS("9.0", "9.0"), 0, MIXT); }
static void s_tess_triangles_wire(void) { tess("triangles, equal_spacing, ccw", LEVELS("3.0", "3.0"), 1, MIXT); }
static void s_tess_fraction(void) { tess("quads, fractional_even_spacing, ccw", LEVELS("16.0", "16.0"), 0, MIXQ); }
static void s_tess_uneven(void) { tess("quads, equal_spacing, ccw", "gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 5.0; gl_TessLevelOuter[2] = 3.0; gl_TessLevelOuter[3] = 8.0; gl_TessLevelInner[0] = 7.0; gl_TessLevelInner[1] = 2.0;", 0, "mix(mix(gl_in[0].gl_Position, gl_in[1].gl_Position, t.x), mix(gl_in[2].gl_Position, gl_in[3].gl_Position, t.x), t.y)"); }
static void s_tess_isolines(void) { tess("isolines, equal_spacing", "gl_TessLevelOuter[0] = 8.0; gl_TessLevelOuter[1] = 16.0;", 0, MIXQ); }
static void s_tess_points(void) { tess("quads, equal_spacing, point_mode", LEVELS("8.0", "8.0"), 0, "vec4(0, 0, 0, 0) + mix(mix(gl_in[0].gl_Position, gl_in[1].gl_Position, t.x), mix(gl_in[2].gl_Position, gl_in[3].gl_Position, t.x), t.y)"); }

/* which way triangles go: their facing, for both domains and windings, whole and with the back culled */
static void s_tess_winding(void) {
    clear(0.05f, 0.05f, 0.1f);
    static const char *layouts[4] = {"triangles, ccw", "triangles, cw", "quads, ccw", "quads, cw"};
    for (int k = 0; k < 8; k++) {
        char tes[1024];
        snprintf(tes, sizeof tes, "layout(%s) in; void main() { vec3 t = gl_TessCoord; gl_Position = %s; }", layouts[k & 3],
                 k & 2 ? "mix(mix(gl_in[0].gl_Position, gl_in[1].gl_Position, t.x), mix(gl_in[2].gl_Position, gl_in[3].gl_Position, t.x), t.y)" : "gl_in[0].gl_Position * t.x + gl_in[1].gl_Position * t.y + gl_in[2].gl_Position * t.z");
        prog5("void main() { vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)); gl_Position = vec4(c * 1.7 - 0.85, 0.0, 1.0); }",
              "layout(vertices = 4) out; void main() { gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position; " LEVELS("3.0", "3.0") " }", tes, NULL,
              "out vec4 o; void main() { o = gl_FrontFacing ? vec4(0.2, 0.9, 0.2, 1) : vec4(0.9, 0.2, 0.2, 1); }");
        glPatchParameteri(GL_PATCH_VERTICES, 4);
        glViewport((k & 3) * 32, (k >> 2) * 64, 32, 64);
        if (k > 3) glEnable(GL_CULL_FACE);
        glDrawArrays(GL_PATCHES, 0, 4);
        glDisable(GL_CULL_FACE);
    }
    glViewport(0, 0, W, H);
}

static void s_feedback(void) {
    clear(0, 0, 0);
    GLuint p = glCreateProgram();
    glAttachShader(p, shader(GL_VERTEX_SHADER, "out vec2 a; out vec4 b; void main() { float t = float(gl_VertexID) * 0.4; a = vec2(cos(t), sin(t)) * (0.1 + 0.02 * float(gl_VertexID)); b = vec4(a * 0.5 + 0.5, 1.0, 1.0); gl_Position = vec4(a, 0.0, 1.0); }"));
    glAttachShader(p, shader(GL_FRAGMENT_SHADER, "out vec4 o; void main() { o = vec4(1); }"));
    const char *names[2] = {"a", "b"};
    glTransformFeedbackVaryings(p, 2, names, GL_INTERLEAVED_ATTRIBS);
    glUseProgram(link(p));
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, b); glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 6 * 4 * 40, NULL, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, b);
    GLuint q[2]; glGenQueries(2, q);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, q[0]); glBeginQuery(GL_PRIMITIVES_GENERATED, q[1]);
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, 20);
    glPauseTransformFeedback(); glDrawArrays(GL_POINTS, 0, 7); glResumeTransformFeedback();
    glDrawArrays(GL_POINTS, 20, 40); /* more than fits */
    glEndTransformFeedback();
    glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN); glEndQuery(GL_PRIMITIVES_GENERATED);
    glDisable(GL_RASTERIZER_DISCARD);
    GLuint n[2] = {0, 0}; glGetQueryObjectuiv(q[0], GL_QUERY_RESULT, &n[0]); glGetQueryObjectuiv(q[1], GL_QUERY_RESULT, &n[1]);
    printf("  feedback: %u written, %u generated\n", n[0], n[1]);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, 0);
    prog("layout(location = 0) in vec2 a; layout(location = 1) in vec4 b; out vec4 v; void main() { v = b; gl_Position = vec4(a, 0.0, 1.0); gl_PointSize = 5.0; }", FS_V);
    glBindBuffer(GL_ARRAY_BUFFER, b);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 24, (void *)0); glVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 24, (void *)8);
    glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);
    glDrawArrays(GL_POINTS, 0, 40);
    glDisableVertexAttribArray(0); glDisableVertexAttribArray(1); glBindBuffer(GL_ARRAY_BUFFER, 0);
    /* the counts, in the picture */
    glEnable(GL_SCISSOR_TEST);
    for (int k = 0; k < 2; k++) { glScissor(0, k * 4, (int)n[k], 3); clear(1, (float)k, 0); }
    glDisable(GL_SCISSOR_TEST);
}
static void s_occlusion(void) {
    clear(0, 0, 0);
    GLuint q[4]; glGenQueries(4, q);
    glEnable(GL_DEPTH_TEST);
    GLuint p = prog("uniform vec4 r; void main() { vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)); gl_Position = vec4(mix(r.xy, r.zw, c), 0.0, 1.0); }", "uniform vec4 c; out vec4 o; void main() { o = c; }");
    GLint r = glGetUniformLocation(p, "r"), c = glGetUniformLocation(p, "c");
    glUniform4f(r, -0.5f, -0.5f, 0.5f, 0.5f); glUniform4f(c, 0, 0, 1, 1); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    float rects[4][4] = {{-1, -1, -0.6f, -0.6f}, {-0.4f, -0.4f, 0.4f, 0.4f}, {0.3f, 0.3f, 0.8f, 0.8f}, {2, 2, 3, 3}};
    glDepthFunc(GL_LESS);
    for (int k = 0; k < 4; k++) {
        glBeginQuery(GL_ANY_SAMPLES_PASSED, q[k]); glUniform4fv(r, 1, rects[k]); glUniform4f(c, 0, 0.25f + 0.25f * k, 0, 1); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4); glEndQuery(GL_ANY_SAMPLES_PASSED);
    }
    glDisable(GL_DEPTH_TEST);
    glEnable(GL_SCISSOR_TEST);
    for (int k = 0; k < 4; k++) { GLuint any = 0; glGetQueryObjectuiv(q[k], GL_QUERY_RESULT, &any); glScissor(k * 32, 0, 30, 8); clear(any ? 1 : 0, any ? 0 : 1, 0); }
    glDisable(GL_SCISSOR_TEST);
}
static void s_msaa(void) {
    GLuint f, r; glGenFramebuffers(1, &f); glGenRenderbuffers(1, &r);
    glBindRenderbuffer(GL_RENDERBUFFER, r); glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4, GL_RGBA8, W, H);
    glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, r);
    clear(0, 0, 0.3f);
    prog("out vec4 v; void main() { float a = float(gl_VertexID) * 2.1 + 0.3; v = vec4(float(gl_VertexID % 3 == 0), float(gl_VertexID % 3 == 1), 0.0, 1.0); gl_Position = vec4(cos(a) * 0.9, sin(a) * 0.9, 0.0, 1.0); }", FS_V);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, f); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fb);
    glBlitFramebuffer(0, 0, W, H, 0, 0, W, H, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glDeleteFramebuffers(1, &f);
}
static void s_msaa_texture(void) {
    GLuint f, t; glGenFramebuffers(1, &f); glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, t); glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 4, GL_RGBA8, 32, 32, GL_TRUE);
    glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D_MULTISAMPLE, t, 0);
    glViewport(0, 0, 32, 32); clear(0, 0, 0);
    prog(VS_FULL, "out vec4 o; void main() { o = vec4(float(gl_SampleID) / 3.0, gl_SamplePosition, 1.0); }");
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0, 0, W, H);
    /* (where samples are is the GPU's business: their numbers only) */
    full("uniform sampler2DMS s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); int k = (q.x >> 6) + (q.y >> 6) * 2; o = vec4(texelFetch(s, (q % 64) / 2, k).r, 0.0, 0.0, 1.0); }");
    glDeleteFramebuffers(1, &f);
}
static void s_mrt(void) {
    GLuint f, t[4]; glGenFramebuffers(1, &f); glGenTextures(4, t); glBindFramebuffer(GL_FRAMEBUFFER, f);
    GLenum fmt[4] = {GL_RGBA8, GL_RGBA8UI, GL_RGBA16F, GL_RG16I}, at[4];
    for (int k = 0; k < 4; k++) {
        glActiveTexture(GL_TEXTURE0 + k); glBindTexture(GL_TEXTURE_2D, t[k]); glTexStorage2D(GL_TEXTURE_2D, 1, fmt[k], W, H);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0 + k, GL_TEXTURE_2D, t[k], 0); at[k] = GL_COLOR_ATTACHMENT0 + k;
    }
    glDrawBuffers(4, at);
    float black[4] = {0, 0, 0, 0}; GLuint u[4] = {7, 8, 9, 10}; GLint i[4] = {-5, 6, 0, 0};
    glClearBufferfv(GL_COLOR, 0, black); glClearBufferuiv(GL_COLOR, 1, u); glClearBufferfv(GL_COLOR, 2, black); glClearBufferiv(GL_COLOR, 3, i);
    glColorMaski(2, GL_TRUE, GL_FALSE, GL_TRUE, GL_TRUE);
    full("in vec2 uv; layout(location = 0) out vec4 a; layout(location = 1) out uvec4 b; layout(location = 2) out vec4 c; layout(location = 3) out ivec2 d;\n"
         "void main() { if (gl_FragCoord.x < 40.0) discard; a = vec4(uv, 0.0, 1.0); b = uvec4(gl_FragCoord.xyxy); c = vec4(uv * 8.0 - 4.0, uv.x * uv.y, 1.0); d = ivec2(gl_FragCoord.yx) - 64; }");
    glColorMaski(2, GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    GLuint p = prog(VS_FULL, "uniform sampler2D a; uniform usampler2D b; uniform sampler2D c; uniform isampler2D d; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); int k = (q.x >> 6) + (q.y >> 6) * 2; q = (q % 64) * 2;\n"
                  " if (k == 0) o = texelFetch(a, q, 0); else if (k == 1) o = vec4(texelFetch(b, q, 0)) / 128.0; else if (k == 2) o = texelFetch(c, q, 0) * 0.2 + 0.5; else o = vec4(vec2(texelFetch(d, q, 0).xy) / 128.0 + 0.5, 0.0, 1.0); }");
    const char *n[4] = {"a", "b", "c", "d"}; for (int k = 0; k < 4; k++) glUniform1i(glGetUniformLocation(p, n[k]), k);
    glDrawArrays(GL_TRIANGLES, 0, 3); glActiveTexture(GL_TEXTURE0); glDeleteFramebuffers(1, &f);
}
static void s_buffer_texture(void) {
    static float d[256 * 4]; for (int k = 0; k < 256 * 4; k++) d[k] = (float)((k * 37) % 101) / 100.0f;
    GLuint b, t; glGenBuffers(1, &b); glBindBuffer(GL_TEXTURE_BUFFER, b); glBufferData(GL_TEXTURE_BUFFER, sizeof d, d, GL_STATIC_DRAW);
    glGenTextures(1, &t); glBindTexture(GL_TEXTURE_BUFFER, t); glTexBuffer(GL_TEXTURE_BUFFER, GL_RGBA32F, b);
    full("uniform samplerBuffer s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy) / 8; o = texelFetch(s, q.y * 16 + q.x); o.a = float(textureSize(s)) / 256.0; }");
}
static void s_indirect_draw(void) {
    clear(0, 0.1f, 0);
    struct { GLuint count, n, first, base; GLuint inst; } a = {6, 5, 0, 1, 0};
    GLushort idx[6] = {0, 1, 2, 2, 1, 3};
    GLuint b[2]; glGenBuffers(2, b);
    glBindBuffer(GL_DRAW_INDIRECT_BUFFER, b[0]); glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof a, &a, GL_STATIC_DRAW);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, b[1]); glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof idx, idx, GL_STATIC_DRAW);
    prog("out vec4 v; void main() { int i = gl_VertexID - 1; vec2 c = vec2(float(i & 1), float(i >> 1)); v = vec4(c, float(gl_InstanceID) / 4.0, 1.0); gl_Position = vec4(c * 0.3 - 0.95 + float(gl_InstanceID) * 0.35, 0.0, 1.0); }", FS_V);
    glDrawElementsIndirect(GL_TRIANGLES, GL_UNSIGNED_SHORT, 0);
    glBindBuffer(GL_DRAW_INDIRECT_BUFFER, 0); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
}
static void s_restart(void) {
    clear(0.1f, 0, 0.1f);
    GLushort idx[] = {0, 1, 4, 5, 0xffff, 2, 3, 6, 7, 0xffff, 8, 12, 9, 13, 10, 14};
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, b); glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof idx, idx, GL_STATIC_DRAW);
    glEnable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
    prog("out vec4 v; void main() { v = vec4(float(gl_VertexID % 4) / 3.0, float(gl_VertexID / 4) / 3.0, 0.5, 1.0); gl_Position = vec4(v.xy * 1.6 - 0.8, 0.0, 1.0); }", FS_V);
    glDrawElements(GL_TRIANGLE_STRIP, 16, GL_UNSIGNED_SHORT, 0);
    glDisable(GL_PRIMITIVE_RESTART_FIXED_INDEX); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
}
static void s_srgb(void) {
    GLuint f, t; glGenFramebuffers(1, &f); glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); glTexStorage2D(GL_TEXTURE_2D, 1, GL_SRGB8_ALPHA8, W, H);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t, 0);
    clear(0.5f, 0.2f, 0);
    glEnable(GL_BLEND); glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
    full("in vec2 uv; out vec4 o; void main() { o = vec4(uv.x, uv.x * uv.x, 0.25, uv.y); }");
    glDisable(GL_BLEND);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    full("uniform sampler2D s; out vec4 o; void main() { o = texelFetch(s, ivec2(gl_FragCoord.xy), 0); }"); glDeleteFramebuffers(1, &f);
}
static void s_gather(void) {
    static unsigned char px[16 * 16 * 4]; for (int k = 0; k < 16 * 16 * 4; k++) px[k] = (unsigned char)(k * 73 % 251);
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 16, 16, 0, GL_RGBA, GL_UNSIGNED_BYTE, px);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    full("in vec2 uv; uniform sampler2D s; out vec4 o; void main() { vec2 p = (floor(uv * 32.0) + 0.5) / 32.0; if (uv.x < 0.5) o = textureGather(s, p, 1); else o = textureGatherOffset(s, p, ivec2(1, -2), 2); o.a = 1.0; }");
}
static void s_grad_lod(void) {
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); glTexStorage2D(GL_TEXTURE_2D, 5, GL_RGBA8, 16, 16);
    for (int l = 0; l < 5; l++) { static unsigned char px[16 * 16 * 4]; for (int k = 0; k < 16 * 16; k++) { px[4 * k] = (unsigned char)(l * 60); px[4 * k + 1] = (unsigned char)(255 - l * 60); px[4 * k + 2] = (unsigned char)(k & 1 ? 255 : 0); px[4 * k + 3] = 255; } glTexSubImage2D(GL_TEXTURE_2D, l, 0, 0, 16 >> l, 16 >> l, GL_RGBA, GL_UNSIGNED_BYTE, px); }
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST_MIPMAP_NEAREST);
    full("in vec2 uv; uniform sampler2D s; out vec4 o; void main() { vec2 q = floor(uv * 4.0); float l = q.x + 0.25; if (q.y < 1.0) o = textureLod(s, uv, l); else if (q.y < 2.0) o = textureGrad(s, uv, vec2(exp2(l) / 16.0, 0.0), vec2(0.0)); else if (q.y < 3.0) o = texelFetch(s, ivec2(0), int(l)); else o = vec4(textureSize(s, int(l)), 0, 16) / 16.0; }");
}
static void s_ubo_bits(void) {
    struct { float a[4]; int i[4]; unsigned u[4]; } d = {{0.1f, 0.5f, 0.9f, 1}, {-7, 100, 12345, -1}, {0xdeadbeefu, 3, 0xffffu, 1u << 31}};
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_UNIFORM_BUFFER, b); glBufferData(GL_UNIFORM_BUFFER, sizeof d, &d, GL_STATIC_DRAW);
    GLuint p = prog(VS_FULL, "layout(std140, binding = 2) uniform B { vec4 a; ivec4 i; uvec4 u; }; out vec4 o; void main() { uvec2 q = uvec2(gl_FragCoord.xy); uint k = (q.x >> 4) + (q.y >> 4) * 8u; uint v = 0u;\n"
                  " switch (k % 14u) { case 0u: v = bitfieldReverse(u.x); break; case 1u: v = uint(bitCount(u.x)); break; case 2u: v = uint(findMSB(u.z)); break; case 3u: v = uint(findLSB(u.w)); break; case 4u: v = bitfieldExtract(u.x, int(k & 7u), 9); break; case 5u: v = uint(bitfieldExtract(i.z, 3, 8)); break; case 6u: v = bitfieldInsert(u.x, k, 8, 8); break;\n"
                  " case 7u: { uint h, l; umulExtended(u.x, k * 77777u + 3u, h, l); v = h ^ l; break; } case 8u: { uint c; v = uaddCarry(u.x, u.w + k, c) + c; break; } case 9u: v = uint(i.x / (int(k) - 30)); break; case 10u: v = u.x % (k + 1u); break; case 11u: v = uint(i.x >> int(k & 3u)); break; case 12u: v = packHalf2x16(a.xy * float(k)); break; default: v = floatBitsToUint(ldexp(a.z, int(k) - 40)); }\n"
                  " o = unpackUnorm4x8(v); o.a = 1.0; }");
    (void)p; glBindBufferBase(GL_UNIFORM_BUFFER, 2, b); glDrawArrays(GL_TRIANGLES, 0, 3);
}
static void s_advanced_interp(void) {
    clear(0, 0, 0);
    prog("flat out int f; out vec3 p; centroid out vec2 c; void main() { float a = float(gl_VertexID) * 2.1; float w = 1.0 + float(gl_VertexID); f = gl_VertexID; p = vec3(cos(a), sin(a), 1.0) * 0.5 + 0.5; c = p.xy; gl_Position = vec4(cos(a) * w, sin(a) * w, 0.0, w); }",
         "flat in int f; in vec3 p; centroid in vec2 c; out vec4 o; void main() { o = vec4(p.x, interpolateAtOffset(p, vec2(0.25, -0.25)).y, float(f) / 3.0, 1.0); o.x += abs(dFdx(p.x)) * 4.0 + fwidth(c.y); }");
    glDrawArrays(GL_TRIANGLES, 0, 3);
}
static void s_atomic_fs(void) {
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, b); GLuint z[2] = {0, 5}; glBufferData(GL_ATOMIC_COUNTER_BUFFER, sizeof z, z, GL_DYNAMIC_COPY); glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, b);
    full("layout(binding = 0, offset = 0) uniform atomic_uint n; layout(binding = 0, offset = 4) uniform atomic_uint m; out vec4 o; void main() { if (gl_FragCoord.x < 50.0) atomicCounterIncrement(n); if (gl_FragCoord.y < 3.0) atomicCounterIncrement(m); o = vec4(1); }");
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    full("layout(binding = 0, offset = 0) uniform atomic_uint n; layout(binding = 0, offset = 4) uniform atomic_uint m; out vec4 o; void main() { uint a = atomicCounter(n), b = atomicCounter(m); o = vec4(float(a & 255u), float(a >> 8), float(b & 255u), 255.0) / 255.0; }");
}
static void s_cube_array(void) {
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_CUBE_MAP_ARRAY, t); glTexStorage3D(GL_TEXTURE_CUBE_MAP_ARRAY, 1, GL_RGBA8, 4, 4, 12);
    for (int f = 0; f < 12; f++) { unsigned char px[64]; for (int k = 0; k < 16; k++) { px[4 * k] = (unsigned char)(f * 20); px[4 * k + 1] = (unsigned char)(k * 16); px[4 * k + 2] = (unsigned char)(f & 1 ? 255 : 0); px[4 * k + 3] = 255; } glTexSubImage3D(GL_TEXTURE_CUBE_MAP_ARRAY, 0, 0, 0, f, 4, 4, 1, GL_RGBA, GL_UNSIGNED_BYTE, px); }
    glTexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    full("precision highp samplerCubeArray; in vec2 uv; uniform samplerCubeArray s; out vec4 o; void main() { vec2 a = uv * vec2(6.283, 3.1416); vec3 d = vec3(cos(a.x) * sin(a.y), cos(a.y), sin(a.x) * sin(a.y)); o = texture(s, vec4(d, fract(uv.y * 4.0) < 0.5 ? 0.0 : 1.0)); }");
}
static void s_shadow(void) {
    GLuint f, t; glGenFramebuffers(1, &f); glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); glTexStorage2D(GL_TEXTURE_2D, 1, GL_DEPTH_COMPONENT24, 32, 32);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE, GL_COMPARE_REF_TO_TEXTURE); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LESS);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D, t, 0);
    glViewport(0, 0, 32, 32); glClearDepthf(0.9f); glClear(GL_DEPTH_BUFFER_BIT); glClearDepthf(1); glEnable(GL_DEPTH_TEST);
    prog("void main() { vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)); gl_Position = vec4(c - 0.5, c.x - 0.5, 1.0); }", "void main() {}"); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisable(GL_DEPTH_TEST); glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0, 0, W, H);
    full("precision highp sampler2DShadow; in vec2 uv; uniform sampler2DShadow s; out vec4 o; void main() { o = vec4(texture(s, vec3(uv, uv.y)), texture(s, vec3(uv, 0.5)), dot(textureGather(s, uv, 0.4), vec4(0.25)), 1.0); }");
    glDeleteFramebuffers(1, &f);
}
static void s_clip_border(void) {
    GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); unsigned char px[16] = {255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255};
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA, GL_UNSIGNED_BYTE, px); float bc[4] = {0.5f, 0.25f, 0.75f, 1};
    glTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, bc); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_MIRRORED_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    full("in vec2 uv; uniform sampler2D s; out vec4 o; void main() { o = texture(s, uv * 4.0 - 1.5); }");
}

/* Found by review: each was wrong once. */
#define HASH "float h(int n) { uint x = uint(n) * 0x9E3779B9u; x ^= x >> 15; x *= 0x85EBCA6Bu; x ^= x >> 13; x *= 0xC2B2AE35u; x ^= x >> 16; return float(x & 0xffffu) / 65536.0; }\n"
static void reset(void) {
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0, 0, W, H);
    glDisable(GL_BLEND); glDisable(GL_DEPTH_TEST); glDisable(GL_STENCIL_TEST); glDisable(GL_SCISSOR_TEST); glDisable(GL_CULL_FACE); glDisable(GL_POLYGON_OFFSET_FILL); glDisable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
    glDisable(GL_SAMPLE_MASK); glDisable(GL_SAMPLE_ALPHA_TO_COVERAGE); glDisable(GL_RASTERIZER_DISCARD);
    glBlendEquation(GL_FUNC_ADD); glBlendFunc(GL_ONE, GL_ZERO); glBlendColor(0,0,0,0); glDepthFunc(GL_LESS); glDepthMask(1); glStencilMask(255); glStencilFunc(GL_ALWAYS, 0, 255); glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    glColorMask(1,1,1,1); glFrontFace(GL_CCW); glCullFace(GL_BACK); glDepthRangef(0,1); glLineWidth(1); glClearDepthf(1); glClearStencil(0);
    for (int k = 0; k < 8; k++) { glDisableVertexAttribArray(k); glVertexAttribDivisor(k, 0); }
    glActiveTexture(GL_TEXTURE0); glBindBuffer(GL_ARRAY_BUFFER, 0); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
    glFrontFace(GL_CCW);
    while (glGetError()) ;
}
#define VS_RECT "layout(location = 0) in vec4 unused; uniform vec4 r; uniform float z; out vec2 uv; void main() { vec2 c = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)); uv = c; gl_Position = vec4(mix(r.xy, r.zw, c), z, 1.0); }\n"
#define FS_C "uniform vec4 c; in vec2 uv; out vec4 o; void main() { o = c; }\n"
static GLint R_, Z_, C_;
static void pattern(void) { full("in vec2 uv; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = vec4(float(q.x) / 127.0, float(q.y) / 127.0, float((q.x / 8 ^ q.y / 8) & 1), 1); }"); }
static GLuint rectp(const char *fs) { GLuint p = prog(VS_RECT, fs); R_ = glGetUniformLocation(p, "r"); Z_ = glGetUniformLocation(p, "z"); C_ = glGetUniformLocation(p, "c"); return p; }
static void rect(float x0, float y0, float x1, float y1, float z, float r, float g, float b, float a) { glUniform4f(R_, x0, y0, x1, y1); if (Z_ >= 0) glUniform1f(Z_, z); if (C_ >= 0) glUniform4f(C_, r, g, b, a); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4); }
static GLuint tex2d(GLenum fmt, int w, int h) { GLuint t; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D, t); glTexStorage2D(GL_TEXTURE_2D, 1, fmt, w, h); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST); return t; }
static void fbtex(GLuint t) { GLuint f; glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t, 0); }
static void show(GLuint t) { glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0,0,W,H); glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, t); full("in vec2 uv; uniform sampler2D s; out vec4 o; void main() { o = texture(s, uv); }"); }
static void count(const char *pre, float z) {
    GLuint b; glGenBuffers(1, &b); glBindBuffer(GL_SHADER_STORAGE_BUFFER, b); GLuint zero[4] = {0}; glBufferData(GL_SHADER_STORAGE_BUFFER, 16, zero, GL_DYNAMIC_COPY); glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, b);
    glEnable(GL_DEPTH_TEST);
    rectp(FS_C); rect(-1,-1,1,1,0,0,0,1,1);
    char fs[512]; snprintf(fs, sizeof fs, "%s layout(std430, binding=0) buffer B { uint n; }; out vec4 o; void main() { atomicAdd(n, 1u); o = vec4(1); }", pre);
    rectp(fs); rect(-1,-1,1,0,z,0,0,0,0);
    glMemoryBarrier(GL_ALL_BARRIER_BITS); glDisable(GL_DEPTH_TEST);
    full("layout(std430, binding=0) buffer B { uint n; }; out vec4 o; void main() { o = vec4(float(n & 255u), float(n >> 8), 0.0, 255.0) / 255.0; }");
}
static void blitzs(GLbitfield m) {
    GLuint f, r; glGenFramebuffers(1, &f); glGenRenderbuffers(1, &r); glBindRenderbuffer(GL_RENDERBUFFER, r); glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, W, H);
    glBindFramebuffer(GL_FRAMEBUFFER, f); glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, r);
    glClearDepthf(0.25f); glClearStencil(7); glClear(GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glClearDepthf(0.75f); glClearStencil(3); glClear(GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, f); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fb);
    glBlitFramebuffer(0,0,64,128,0,0,64,128,m,GL_NEAREST);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    /* red where stencil 3, green where 7; blue where z 0.5 passes */
    glEnable(GL_STENCIL_TEST); rectp(FS_C);
    glStencilFunc(GL_EQUAL, 3, 255); rect(-1,-1,1,1,0, 1,0,0,1); glStencilFunc(GL_EQUAL, 7, 255); rect(-1,-1,1,1,0, 0,1,0,1);
    glDisable(GL_STENCIL_TEST); glEnable(GL_DEPTH_TEST); glDepthFunc(GL_LESS); glDepthMask(0); rect(-1,-1,1,0,0, 0,0,1,1);
}
static void layers(int single) {
    GLuint t, f; glGenTextures(1, &t); glBindTexture(GL_TEXTURE_2D_ARRAY, t); glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA8, 64, 64, 4); glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &f); glBindFramebuffer(GL_FRAMEBUFFER, f); glViewport(0,0,64,64);
    glFramebufferTexture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, t, 0); clear(0.1f,0.2f,0.3f);
    if (single) glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, t, 0, 1);
    prog5(VS_FULL, NULL, NULL, "layout(triangles) in; layout(triangle_strip, max_vertices = 3) out; out vec4 v; void main() { for (int i = 0; i < 3; i++) { gl_Layer = 1; v = vec4(1,0,0,1); gl_Position = gl_in[i].gl_Position; EmitVertex(); } }", FS_V);
    glDrawArrays(GL_TRIANGLES,0,3);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0,0,W,H); glBindTexture(GL_TEXTURE_2D_ARRAY, t);
    full("uniform sampler2DArray s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = texelFetch(s, ivec3(q % 64, (q.x >> 6) + (q.y >> 6) * 2), 0); }");
}
static void s_blend_clamp(void) {
    clear(0.25f, 0.5f, 0.75f); glEnable(GL_BLEND);
    rectp(FS_C);
    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA); rect(-1,-1,-0.5f,1,0, 2,2,0,0.5f);
    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA); rect(-0.5f,-1,0,1,0, 0.5f,0.5f,0.5f,2);
    glBlendFunc(GL_ONE, GL_ONE); glBlendEquation(GL_FUNC_REVERSE_SUBTRACT); rect(0,-1,0.5f,1,0, -1,-1,-1,0);
    glBlendEquation(GL_FUNC_ADD); glBlendColor(2,2,2,2); glBlendFunc(GL_CONSTANT_COLOR, GL_ZERO); rect(0.5f,-1,1,1,0, 0.3f,0.3f,0.3f,1);
}
static void s_dual_src(void) {
    if (!strstr((const char*)glGetString(GL_EXTENSIONS), "GL_EXT_blend_func_extended")) return;
    clear(0.3f,0.6f,0.9f); glEnable(GL_BLEND); glBlendFunc(0x88F9 /*SRC1_COLOR*/, 0x88FA /*ONE_MINUS_SRC1_COLOR*/);
    rectp("\n#extension GL_EXT_blend_func_extended : require\nin vec2 uv; layout(location = 0, index = 0) out vec4 a; layout(location = 0, index = 1) out vec4 b; void main() { a = vec4(uv, 1, 1); b = vec4(uv.yx * 1.5, 0.3, 0.7); }"); rect(-0.9f,-0.9f,0.9f,0,0,0,0,0,0);
    glBlendFunc(GL_ONE, 0x8589 /*SRC1_ALPHA*/); rect(-0.9f,0.1f,0.9f,0.9f,0,0,0,0,0);
}
static void s_blit_flip(void) {
    GLuint t = tex2d(GL_RGBA8, W, H); fbtex(t); pattern(); GLint f; glGetIntegerv(GL_FRAMEBUFFER_BINDING, &f);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, f); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fb);
    glBlitFramebuffer(0,0,50,50, 0,50,50,0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBlitFramebuffer(0,0,50,50, 110,0,60,50, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBlitFramebuffer(3,5,53,55, 50,120,0,70, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBlitFramebuffer(53,5,3,55, 60,70,110,120, GL_COLOR_BUFFER_BIT, GL_NEAREST);
}
static void s_blit_depth(void) { blitzs(GL_DEPTH_BUFFER_BIT); }
static void s_blit_stencil(void) { blitzs(GL_STENCIL_BUFFER_BIT); }
static void s_late_side(void) { count("", 0.5f); }
static void s_early_side(void) { count("layout(early_fragment_tests) in;", 0.5f); }
static void s_layer_single(void) { layers(1); }
static void s_layer_vs(void) { /* out-of-range layer of a layered target */
    GLuint t[2], f; glGenTextures(2,t); for (int k = 0; k < 2; k++) { glBindTexture(GL_TEXTURE_2D_ARRAY,t[k]); glTexStorage3D(GL_TEXTURE_2D_ARRAY,1,GL_RGBA8,64,64,2); glTexParameteri(GL_TEXTURE_2D_ARRAY,GL_TEXTURE_MIN_FILTER,GL_NEAREST); } glGenFramebuffers(1,&f); glBindFramebuffer(GL_FRAMEBUFFER,f); for (int k = 0; k < 2; k++) { glFramebufferTexture(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,t[k],0); glViewport(0,0,64,64); clear(0,0,0.4f); }
    glFramebufferTexture(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,t[0],0); prog5(VS_FULL,NULL,NULL,"layout(triangles, invocations = 8) in; layout(triangle_strip, max_vertices = 3) out; out vec4 v; void main() { for (int i = 0; i < 3; i++) { gl_Layer = 2 + gl_InvocationID * 4; v = vec4(1,0,0,1); gl_Position = gl_in[i].gl_Position; EmitVertex(); } }",FS_V); glDrawArrays(GL_TRIANGLES,0,3); reset(); glBindTexture(GL_TEXTURE_2D_ARRAY,t[1]); full("uniform sampler2DArray s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); o = texelFetch(s, ivec3(q % 64, q.x / 64), 0); }"); }
static void s_nan_lines(void) {
    prog("layout(location=0) in vec2 p; void main() { gl_Position = vec4(p, 0, 1); }", "out vec4 o; void main() { o = vec4(1); }");
    union { unsigned u; float f; } nan = {0x7fc00000};
    float v[] = {-0.9f,0.1f, -0.5f,0.8f, -0.2f,nan.f, 0.1f,0.5f, 0.6f,0.9f, 0.9f,0.2f};
    GLuint b; glGenBuffers(1,&b); glBindBuffer(GL_ARRAY_BUFFER,b); glBufferData(GL_ARRAY_BUFFER,sizeof v,v,GL_STATIC_DRAW);
    glVertexAttribPointer(0,2,GL_FLOAT,0,0,0); glEnableVertexAttribArray(0); glDrawArrays(GL_LINE_STRIP,0,6);
}
static void s_restart_primid(void) {
    prog("layout(location=0) in vec2 p; void main() { gl_Position = vec4(p, 0, 1); }", "out vec4 o; void main() { int k = gl_PrimitiveID; o = vec4(float(k & 1), float((k >> 1) & 1), float((k >> 2) & 1), 1) * 0.7 + 0.2; }");
    float v[32]; for (int k = 0; k < 16; k++) { v[2*k] = (k & 1) * 0.4f - 0.95f + (k / 4) * 0.5f; v[2*k+1] = ((k >> 1) & 1) * 1.6f - 0.8f; }
    GLushort e[20]; int n = 0; for (int k = 0; k < 16; k++) { e[n++] = k; if (k % 4 == 3) e[n++] = 0xffff; }
    GLuint b[2]; glGenBuffers(2, b); glBindBuffer(GL_ARRAY_BUFFER, b[0]); glBufferData(GL_ARRAY_BUFFER, sizeof v, v, GL_STATIC_DRAW); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, b[1]); glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof e, e, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, 0, 0, 0); glEnableVertexAttribArray(0); glEnable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
    glDrawElements(GL_TRIANGLE_STRIP, n, GL_UNSIGNED_SHORT, 0);
}
static void s_tex3d_layers(void) {
    GLuint t, f; glGenTextures(1,&t); glBindTexture(GL_TEXTURE_3D,t); glTexStorage3D(GL_TEXTURE_3D, 2, GL_RGBA8, 40, 40, 16); glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST_MIPMAP_NEAREST); glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1,&f); glBindFramebuffer(GL_FRAMEBUFFER,f);
    for (int l = 0; l < 2; l++) for (int z = 0; z < 16 >> l; z++) { glFramebufferTextureLayer(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,t,l,z); glViewport(0,0,40>>l,40>>l); clear(z/16.0f, l, 0.2f); rectp(FS_C); rect(-0.5f,-0.5f,0.5f,0.5f,0, 0.5f, z/16.0f, 1, 1); }
    glFramebufferTexture(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,t,0); glViewport(0,0,40,40);
    prog5(VS_FULL, NULL, NULL, "layout(triangles, invocations = 16) in; layout(triangle_strip, max_vertices = 3) out; out vec4 v; void main() { for (int i = 0; i < 3; i++) { gl_Layer = gl_InvocationID; v = vec4(1,float(gl_InvocationID) / 15.0,0,1); gl_Position = vec4(gl_in[i].gl_Position.xy * 0.5 - vec2(0.5) , 0, 1); EmitVertex(); } }", FS_V);
    glEnable(GL_SCISSOR_TEST); glScissor(0,0,9,9); glDrawArrays(GL_TRIANGLES,0,3); glDisable(GL_SCISSOR_TEST);
    glBindFramebuffer(GL_FRAMEBUFFER, fb); glViewport(0,0,W,H);
    full("uniform highp sampler3D s; out vec4 o; void main() { ivec2 q = ivec2(gl_FragCoord.xy); int z = q.x / 16 + (q.y / 32) * 8; o = textureLod(s, vec3(vec2(q % ivec2(16, 32)) / vec2(16.0, 32.0), (float(z & 15) + 0.5) / 16.0), float(z >> 4)); }");
}
static void s_lut3d(void) { /* a colour lookup table drawn slice by slice */ GLuint t, f; glGenTextures(1,&t); glBindTexture(GL_TEXTURE_3D,t); glTexStorage3D(GL_TEXTURE_3D,1,GL_RGBA8,16,16,16); glTexParameteri(GL_TEXTURE_3D,GL_TEXTURE_MIN_FILTER,GL_LINEAR); glGenFramebuffers(1,&f); glBindFramebuffer(GL_FRAMEBUFFER,f); glViewport(0,0,16,16); rectp("uniform vec4 c; in vec2 uv; out vec4 o; void main() { o = vec4(1.0 - uv, 1.0 - c.x, 1); }"); for (int z = 0; z < 16; z++) { glFramebufferTextureLayer(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,t,0,z); rect(-1,-1,1,1,0,z/15.0f,0,0,0); } reset(); glBindTexture(GL_TEXTURE_3D,t); full("uniform highp sampler3D s; in vec2 uv; out vec4 o; void main() { o = texture(s, vec3(uv, uv.x * uv.y)); }"); }
static void s_points_far(void) {
    prog(HASH "void main() { float e = h(gl_VertexID * 3) * 14.0, s = h(gl_VertexID * 3 + 1) * 14.0; float a = h(gl_VertexID * 3 + 2) * 6.28; gl_Position = vec4(cos(a) * pow(10.0, e), sin(a) * pow(10.0, e), 0, 1); gl_PointSize = pow(10.0, s); }", "out vec4 o; void main() { o = vec4(1,0,0,1); }");
    glDrawArrays(GL_POINTS,0,20000); clear(0,1,0);
}
static void s_empty_indirect(void) {
    GLuint args[10] = {0,1,0,0,0, 3,1,0,0,0}; GLuint b[2]; glGenBuffers(2,b); glBindBuffer(GL_DRAW_INDIRECT_BUFFER,b[0]); glBufferData(GL_DRAW_INDIRECT_BUFFER,sizeof args,args,GL_STATIC_DRAW); GLushort e[] = {0,1,2}; glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,b[1]); glBufferData(GL_ELEMENT_ARRAY_BUFFER,6,e,GL_STATIC_DRAW);
    prog(VS_FULL, "out vec4 o; void main() { o = vec4(0,1,0,1); }"); glDrawElementsIndirect(GL_TRIANGLES,GL_UNSIGNED_SHORT,0); glDrawElementsIndirect(GL_TRIANGLES,GL_UNSIGNED_SHORT,(void*)20);
}

struct scene { const char *name; void (*fn)(void); };
static const struct scene scenes[] = {
    {"compute_ssbo", s_compute_ssbo}, {"compute_shared", s_compute_shared}, {"compute_image", s_compute_image}, {"compute_indirect", s_compute_indirect},
    {"geometry_quads", s_geometry_quads}, {"geometry_lines", s_geometry_lines}, {"geometry_adjacency", s_geometry_adjacency}, {"geometry_layers", s_geometry_layers},
    {"tess_quads", s_tess_quads}, {"tess_quads_wire", s_tess_quads_wire}, {"tess_triangles", s_tess_triangles}, {"tess_triangles_wire", s_tess_triangles_wire},
    {"tess_fraction", s_tess_fraction}, {"tess_uneven", s_tess_uneven}, {"tess_isolines", s_tess_isolines}, {"tess_points", s_tess_points}, {"tess_winding", s_tess_winding},
    {"feedback", s_feedback}, {"occlusion", s_occlusion}, {"msaa", s_msaa}, {"msaa_texture", s_msaa_texture}, {"mrt", s_mrt}, {"buffer_texture", s_buffer_texture},
    {"indirect_draw", s_indirect_draw}, {"restart", s_restart}, {"srgb", s_srgb}, {"gather", s_gather}, {"grad_lod", s_grad_lod}, {"ubo_bits", s_ubo_bits},
    {"advanced_interp", s_advanced_interp}, {"atomic_fs", s_atomic_fs}, {"cube_array", s_cube_array}, {"shadow", s_shadow}, {"clip_border", s_clip_border},
    {"blend_clamp", s_blend_clamp}, {"dual_src", s_dual_src}, {"blit_flip", s_blit_flip}, {"blit_depth", s_blit_depth}, {"blit_stencil", s_blit_stencil}, {"late_side", s_late_side}, {"early_side", s_early_side}, {"layer_single", s_layer_single}, {"layer_vs", s_layer_vs}, {"nan_lines", s_nan_lines}, {"restart_primid", s_restart_primid}, {"tex3d_layers", s_tex3d_layers}, {"lut3d", s_lut3d}, {"points_far", s_points_far}, {"empty_indirect", s_empty_indirect},
};
#define NSCENES (sizeof scenes / sizeof scenes[0])

static int run(const char *dir, const char *only) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay d = gpd(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    EGLint maj, min;
    if (!eglInitialize(d, &maj, &min)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cattr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (!ctx) { fprintf(stderr, "no GLES 3.2 context\n"); return 1; }
    eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
    printf("renderer: %s / %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));
    glGenFramebuffers(1, &fb); glBindFramebuffer(GL_FRAMEBUFFER, fb); glGenRenderbuffers(2, rb);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[0]); glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, W, H);
    glBindRenderbuffer(GL_RENDERBUFFER, rb[1]); glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, rb[1]);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glViewport(0, 0, W, H);
    static unsigned char px[W * H * 4];
    for (unsigned i = 0; i < NSCENES; i++) {
        if (only && strcmp(only, scenes[i].name)) continue;
        reset();
        clear(0, 0, 0);
        scenes[i].fn();
        glBindFramebuffer(GL_FRAMEBUFFER, fb);
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
        char path[512]; snprintf(path, sizeof path, "%s/%s.rgba", dir, scenes[i].name);
        FILE *f = fopen(path, "wb"); fwrite(px, 1, sizeof px, f); fclose(f);
        GLenum e = glGetError();
        printf("scene %-20s done%s\n", scenes[i].name, e ? " (GL error)" : ""); fflush(stdout);
    }
    return 0;
}

static int cmp(const char *a, const char *b) {
    static unsigned char pa[W * H * 4], pb[W * H * 4];
    int fails = 0;
    for (unsigned i = 0; i < NSCENES; i++) {
        const char *dir[2] = {a, b}; unsigned char *p[2] = {pa, pb}; int ok = 1;
        for (int k = 0; k < 2; k++) {
            char path[512]; snprintf(path, sizeof path, "%s/%s.rgba", dir[k], scenes[i].name);
            FILE *f = fopen(path, "rb"); if (!f || fread(p[k], 1, sizeof pa, f) != sizeof pa) { printf("%-20s missing in %s\n", scenes[i].name, dir[k]); ok = 0; } if (f) fclose(f);
        }
        if (!ok) { fails++; continue; }
        int off = 0, maxd = 0; double sum = 0;
        for (int q = 0; q < W * H; q++) { int m = 0; for (int c = 0; c < 4; c++) { int d = abs(pa[4 * q + c] - pb[4 * q + c]); if (d > m) m = d; sum += d; } if (m > 12) off++; if (m > maxd) maxd = m; }
        double pct = 100.0 * off / (W * H); ok = pct < 2.0; fails += !ok;
        printf("%-20s %s  pixels off by >12: %6.2f%%  max diff %3d  mean diff %.3f\n", scenes[i].name, ok ? "PASS" : "FAIL", pct, maxd, sum / (W * H * 4));
    }
    printf("%d of %u scenes differ\n", fails, (unsigned)NSCENES);
    return fails != 0;
}

int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "run")) return run(argv[2], argc > 3 ? argv[3] : NULL);
    if (argc >= 4 && !strcmp(argv[1], "cmp")) return cmp(argv[2], argv[3]);
    fprintf(stderr, "usage: gles32test run DIR [SCENE] | gles32test cmp A B\n");
    return 1;
}
