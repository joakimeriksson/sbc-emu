//! The demos. Nothing is loaded: meshes and textures come out of the shaders.
use crate::gl::*;
use crate::sys::*;

pub trait Scene {
    /// Begin anew, where it depends on what was.
    fn start(&mut self) {}
    /// Draw frame `t` (seconds), `dt` since the last.
    fn draw(&mut self, t: f32, dt: f32, to: &Target);
}

/// What to draw into: a framebuffer, its pixels, and whether up is up (1) or down (-1).
pub struct Target {
    pub fb: u32,
    pub w: i32,
    pub h: i32,
    pub up: f32,
}

impl Target {
    fn perspective(&self, fov: f32) -> M {
        let mut m = M::perspective(fov, self.w as f32 / self.h as f32, 0.1, 60.0);
        m.0[5] *= self.up;
        m
    }

    /// From a distance.
    fn view(&self, distance: f32, fov: f32) -> M {
        self.perspective(fov) * M::at(0.0, 0.0, -distance)
    }

    fn begin(&self, c: [f32; 3]) {
        begin(self.fb, self.w, self.h, c);
        // SAFETY: plain GL state
        unsafe { glFrontFace(if self.up < 0.0 { 0x900 } else { 0x901 }) }
    }
}

pub const NAMES: [&str; 6] = ["cubes", "planet", "swarm", "fur", "tunnel", "shadow"];

pub fn make(name: &str) -> Option<Box<dyn Scene>> {
    Some(match name {
        "cubes" => Box::new(Cubes::new()),
        "planet" => Box::new(Planet::new()),
        "swarm" => Box::new(Swarm::new()),
        "fur" => Box::new(Fur::new()),
        "tunnel" => Box::new(Tunnel::new()),
        "shadow" => Box::new(Shadow::new()),
        _ => return None,
    })
}

fn begin(fb: u32, w: i32, h: i32, c: [f32; 3]) {
    // SAFETY: plain GL state
    unsafe {
        glBindVertexArray(0);
        glBindFramebuffer(FRAMEBUFFER, fb);
        glViewport(0, 0, w, h);
        glDisable(BLEND);
        glDisable(CULL_FACE);
        glEnable(DEPTH_TEST);
        glDepthMask(1);
        glClearColor(c[0], c[1], c[2], 1.0);
        glClear(COLOR_BUFFER_BIT | DEPTH_BUFFER_BIT);
    }
}

/// A cube's corner from a vertex number, with its normal.
const CUBE: &str = "
const int C[36] = int[36](0,1,2, 2,1,3, 4,6,5, 5,6,7, 0,2,4, 4,2,6, 1,5,3, 3,5,7, 0,4,1, 1,4,5, 2,3,6, 6,3,7);
const vec3 N[6] = vec3[6](vec3(0,0,-1), vec3(0,0,1), vec3(-1,0,0), vec3(1,0,0), vec3(0,-1,0), vec3(0,1,0));
vec3 corner(int k) { int c = C[k + 2 - k % 3 * 2]; return vec3(float(c & 1), float(c >> 1 & 1), float(c >> 2)) * 2.0 - 1.0; }
vec3 normal(int k) { return N[k / 6]; }
";

/// 125 cubes, one draw.
struct Cubes(u32);

impl Cubes {
    fn new() -> Self {
        let vs = format!(
            "{CUBE}
            uniform mat4 m; uniform mat4 r; uniform float t; out vec3 n; out vec3 c; out vec3 q;
            void main() {{
                int i = gl_InstanceID;
                vec3 g = vec3(float(i % 5), float(i / 5 % 5), float(i / 25)) - 2.0;
                float w = sin(length(g) * 1.3 - t * 2.5) * 0.5 + 0.5;
                float a = t + g.x * 0.3 + g.y * 0.2, s = sin(a), k = cos(a);
                mat3 spin = mat3(k, 0, -s, 0, 1, 0, s, 0, k);
                q = corner(gl_VertexID);
                n = mat3(r) * spin * normal(gl_VertexID);
                c = 0.5 + 0.5 * cos(vec3(0, 2, 4) + g * 0.6 + t * 0.5);
                gl_Position = m * vec4(spin * q * (0.16 + 0.2 * w) + g * 0.95, 1);
            }}"
        );
        Self(program(&[
            (VERTEX, &vs),
            (
                FRAGMENT,
                "in vec3 n; in vec3 c; in vec3 q; out vec4 o;
                void main() {
                    vec3 l = normalize(vec3(0.4, 0.7, 0.6)), m = normalize(n);
                    float d = max(dot(m, l), 0.0), s = pow(max(reflect(-l, m).z, 0.0), 24.0);
                    vec3 e = step(0.86, abs(q));
                    float edge = e.x + e.y + e.z > 1.5 ? 0.35 : 1.0;
                    o = vec4(c * (0.2 + 0.8 * d) * edge + s * 0.5, 1);
                }",
            ),
        ]))
    }
}

impl Scene for Cubes {
    fn draw(&mut self, t: f32, _: f32, to: &Target) {
        to.begin([0.03, 0.04, 0.07]);
        let r = M::turn(t * 0.3, 0.45);
        // SAFETY: plain GL calls
        unsafe {
            glUseProgram(self.0);
            set(self.0, "t", t);
            set_matrix(self.0, "r", &r);
            set_matrix(self.0, "m", &(to.view(8.0, 0.9) * r));
            glEnable(CULL_FACE);
            glDrawArraysInstanced(TRIANGLES, 0, 36, 125);
        }
    }
}

const NOISE: &str = "
float hash(vec3 p) { uvec3 q = uvec3(ivec3(p) + 512) * uvec3(1597334673u, 3812015801u, 2798796415u); uint n = (q.x ^ q.y ^ q.z) * 1597334673u; return float(n >> 8) / 16777216.0; }
float noise(vec3 p) { vec3 i = floor(p), f = p - i; f = f * f * (3.0 - 2.0 * f);
    return mix(mix(mix(hash(i), hash(i + vec3(1,0,0)), f.x), mix(hash(i + vec3(0,1,0)), hash(i + vec3(1,1,0)), f.x), f.y),
               mix(mix(hash(i + vec3(0,0,1)), hash(i + vec3(1,0,1)), f.x), mix(hash(i + vec3(0,1,1)), hash(i + vec3(1,1,1)), f.x), f.y), f.z); }
float land(vec3 p) { return noise(p * 2.0) * 0.55 + noise(p * 4.3) * 0.28 + noise(p * 9.1) * 0.17; }
";

/// An octahedron, tessellated into a planet with mountains.
struct Planet(u32);

impl Planet {
    fn new() -> Self {
        let te = format!(
            "layout(triangles, fractional_odd_spacing, ccw) in; {NOISE}
            uniform mat4 m; out vec3 p; out float height;
            void main() {{
                p = normalize(gl_in[0].gl_Position.xyz * gl_TessCoord.x + gl_in[1].gl_Position.xyz * gl_TessCoord.y + gl_in[2].gl_Position.xyz * gl_TessCoord.z);
                height = land(p + 3.0);
                gl_Position = m * vec4(p * (1.0 + 0.32 * max(height - 0.5, 0.0)), 1);
            }}"
        );
        // the light and the fragments' normals are the planet's
        let fs = format!(
            "{NOISE} in vec3 p; in float height; uniform vec2 sun; uniform float t; out vec4 o;
            void main() {{
                vec3 n = normalize(p), l = normalize(vec3(sun.x, 0.25, sun.y));
                bool sea = height < 0.5;
                vec3 c = sea ? mix(vec3(0.01, 0.06, 0.2), vec3(0.05, 0.3, 0.45), smoothstep(0.3, 0.5, height))
                    : mix(mix(vec3(0.75, 0.7, 0.45), vec3(0.12, 0.4, 0.1), smoothstep(0.5, 0.56, height)), mix(vec3(0.35, 0.28, 0.2), vec3(1), smoothstep(0.68, 0.74, height)), smoothstep(0.58, 0.68, height));
                vec3 g = vec3(dFdx(height), dFdy(height), 0.0) * 30.0;
                float d = max(dot(n, l) + (sea ? 0.0 : clamp(g.x + g.y, -0.15, 0.15)), 0.0);
                float cloud = smoothstep(0.52, 0.72, noise(n * 5.0 + vec3(t * 0.1)) * 0.7 + noise(n * 12.0 - vec3(t * 0.15)) * 0.3);
                o = vec4(mix(c, vec3(1), cloud * 0.8) * (0.06 + d), 1);
            }}"
        );
        Self(program(&[
            (
                VERTEX,
                "const vec3 P[6] = vec3[6](vec3(1,0,0), vec3(-1,0,0), vec3(0,1,0), vec3(0,-1,0), vec3(0,0,1), vec3(0,0,-1));
                const int F[24] = int[24](0,2,4, 4,2,1, 1,2,5, 5,2,0, 0,4,3, 4,1,3, 1,5,3, 5,0,3);
                void main() { gl_Position = vec4(P[F[gl_VertexID]], 1); }",
            ),
            (
                TESS_CONTROL,
                "layout(vertices = 3) out; uniform float level;
                void main() {
                    gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;
                    gl_TessLevelOuter[0] = level; gl_TessLevelOuter[1] = level; gl_TessLevelOuter[2] = level; gl_TessLevelInner[0] = level;
                }",
            ),
            (TESS_EVALUATION, &te),
            (FRAGMENT, &fs),
        ]))
    }
}

impl Scene for Planet {
    fn draw(&mut self, t: f32, _: f32, to: &Target) {
        to.begin([0.0, 0.0, 0.015]);
        // SAFETY: plain GL calls
        unsafe {
            glUseProgram(self.0);
            let p = self.0;
            set(p, "level", 20.0 + 11.0 * (t * 0.5).sin());
            set(p, "t", t);
            let a = t * 0.25;
            // (the sun stands still while the planet turns)
            glUniform2f(location(p, "sun"), (a - 0.9).sin(), (a - 0.9).cos());
            set_matrix(p, "m", &(to.view(3.0, 0.8) * M::turn(-a, 0.35)));
            glEnable(CULL_FACE);
            glPatchParameteri(0x8e72, 3);
            glDrawArrays(PATCHES, 0, 24);
        }
    }
}

const SWARM: i32 = 8192;

/// Particles around three attractors: a vertex shader moves them and transform feedback keeps them.
struct Swarm {
    step: u32,
    show: u32,
    /// what was, and what will be, with their vertex arrays
    buffers: [u32; 2],
    arrays: [u32; 2],
}

impl Swarm {
    fn new() -> Self {
        let step = captured(
            "layout(location = 0) in vec4 p; layout(location = 1) in vec4 v; uniform float t; uniform float dt; uniform int first; out vec4 pos; out vec4 vel;
            float h(uint n) { n = (n ^ 61u) ^ (n >> 16); n *= 9u; n ^= n >> 4; n *= 0x27d4eb2du; n ^= n >> 15; return float(n >> 8) / 16777216.0; }
            void main() {
                if (first != 0) {
                    uint i = uint(gl_VertexID);
                    float x = h(i) * 6.2832, z = h(i + 99991u) * 2.0 - 1.0, r = sqrt(1.0 - z * z);
                    pos = vec4(vec3(r * cos(x), z, r * sin(x)) * (1.2 + h(i + 777u)), 1);
                    vel = vec4(cross(pos.xyz, vec3(0.1, 0.5, 0.05)), 0);
                    return;
                }
                vec3 f = -p.xyz * 0.15;
                for (int k = 0; k < 3; k++) {
                    float s = t * 0.7 + float(k) * 2.0944;
                    vec3 d = vec3(cos(s) * 1.4, sin(s * 1.7) * 0.9, sin(s) * 1.4) - p.xyz;
                    float l = dot(d, d);
                    f += d / (l * sqrt(l) + 0.15) * 0.9;
                }
                vel = vec4((v.xyz + f * dt) * (1.0 - 0.12 * dt), 0);
                pos = vec4(p.xyz + vel.xyz * dt, 1);
            }",
            &["pos", "vel"],
        );
        let show = program(&[
            (
                VERTEX,
                "layout(location = 0) in vec4 p; layout(location = 1) in vec4 v; uniform mat4 m; out vec3 c;
                void main() {
                    c = mix(vec3(0.1, 0.3, 1.0), vec3(1.0, 0.45, 0.1), smoothstep(0.4, 2.2, length(v.xyz))) * 0.6 + 0.08;
                    gl_Position = m * p;
                    gl_PointSize = 14.0 / gl_Position.w + 1.5;
                }",
            ),
            (FRAGMENT, "in vec3 c; out vec4 o; void main() { vec2 d = gl_PointCoord * 2.0 - 1.0; o = vec4(c * max(1.0 - dot(d, d), 0.0), 1); }"),
        ]);
        let (mut buffers, mut arrays) = ([0; 2], [0; 2]);
        // SAFETY: buffers of their size, two vectors a particle
        unsafe {
            glGenBuffers(2, buffers.as_mut_ptr());
            glGenVertexArrays(2, arrays.as_mut_ptr());
            for k in 0..2 {
                glBindVertexArray(arrays[k]);
                glBindBuffer(ARRAY_BUFFER, buffers[k]);
                glBufferData(ARRAY_BUFFER, SWARM as isize * 32, std::ptr::null(), DYNAMIC_COPY);
                for a in 0..2 {
                    glVertexAttribPointer(a, 4, 0x1406, 0, 32, std::ptr::without_provenance(a as usize * 16));
                    glEnableVertexAttribArray(a);
                }
            }
        }
        Self { step, show, buffers, arrays }
    }

    fn run(&mut self, t: f32, dt: f32, first: bool) {
        // SAFETY: plain GL calls; the captured buffer is not the one that is read
        unsafe {
            glUseProgram(self.step);
            glBindVertexArray(self.arrays[0]);
            set(self.step, "t", t);
            set(self.step, "dt", dt);
            glUniform1i(location(self.step, "first"), first as i32);
            glEnable(RASTERIZER_DISCARD);
            glBindBufferBase(TRANSFORM_FEEDBACK_BUFFER, 0, self.buffers[1]);
            glBeginTransformFeedback(POINTS);
            glDrawArrays(POINTS, 0, SWARM);
            glEndTransformFeedback();
            glBindBufferBase(TRANSFORM_FEEDBACK_BUFFER, 0, 0);
            glDisable(RASTERIZER_DISCARD);
        }
        self.buffers.swap(0, 1);
        self.arrays.swap(0, 1);
    }
}

impl Scene for Swarm {
    fn start(&mut self) {
        self.run(0.0, 0.0, true);
    }

    fn draw(&mut self, t: f32, dt: f32, to: &Target) {
        // (steps small enough to be steady, however long a frame took)
        let n = (dt / 0.02).ceil().clamp(1.0, 4.0);
        for k in 0..n as i32 {
            self.run(t - dt + dt * (k as f32 + 1.0) / n, (dt / n).min(0.02), false);
        }
        to.begin([0.0, 0.0, 0.0]);
        // SAFETY: plain GL calls
        unsafe {
            glUseProgram(self.show);
            glBindVertexArray(self.arrays[0]);
            set_matrix(self.show, "m", &(to.view(6.5, 0.9) * M::turn(t * 0.15, 0.5)));
            glEnable(BLEND);
            glBlendFunc(ONE, ONE);
            glDisable(DEPTH_TEST);
            glDrawArrays(POINTS, 0, SWARM);
            glBindVertexArray(0);
        }
    }
}

/// A ring whose geometry shader grows hair on it.
struct Fur {
    skin: u32,
    hair: u32,
}

const RING: &str = "
const int U = 48, V = 20;
vec3 ring(float u, float v, out vec3 n) { float a = u * 6.28319, b = v * 6.28319; n = vec3(cos(a) * cos(b), sin(b), sin(a) * cos(b)); return vec3(cos(a), 0, sin(a)) * 1.25 + n * 0.5; }
const ivec2 Q[6] = ivec2[6](ivec2(0,0), ivec2(0,1), ivec2(1,0), ivec2(1,0), ivec2(0,1), ivec2(1,1));
void place(out vec3 p, out vec3 n, out vec2 uv) { int c = gl_VertexID / 6; ivec2 q = ivec2(c % U, c / U) + Q[gl_VertexID % 6]; uv = vec2(q) / vec2(U, V); p = ring(uv.x, uv.y, n); }
vec3 color(vec2 uv) { float s = smoothstep(-0.2, 0.2, sin(uv.x * 37.7 + sin(uv.y * 12.6) * 2.0)); return mix(vec3(0.85, 0.45, 0.1), vec3(0.06, 0.03, 0.02), s); }
";

impl Fur {
    fn new() -> Self {
        let vs = format!("{RING} uniform mat4 m; uniform mat4 r; out vec3 n; out vec3 c; void main() {{ vec3 p; vec2 uv; place(p, n, uv); c = color(uv) * 0.5; n = mat3(r) * n; gl_Position = m * vec4(p, 1); }}");
        let skin = program(&[(VERTEX, &vs), (FRAGMENT, "in vec3 n; in vec3 c; out vec4 o; void main() { o = vec4(c * (0.25 + 0.75 * max(normalize(n).y, 0.0)), 1); }")]);
        let vs = format!("{RING} out vec3 gp; out vec3 gn; out vec3 gc; void main() {{ vec2 uv; place(gp, gn, uv); gc = color(uv); gl_Position = vec4(gp, 1); }}");
        let hair = program(&[
            (VERTEX, &vs),
            (
                GEOMETRY,
                "layout(triangles) in; layout(triangle_strip, max_vertices = 21) out;
                in vec3 gp[]; in vec3 gn[]; in vec3 gc[]; uniform mat4 m; uniform mat4 r; uniform float t; out vec3 c;
                float h(int k) { return fract(sin(float(gl_PrimitiveIDIn * 3 + k) * 12.9898) * 43758.5453); }
                void main() {
                    for (int k = 0; k < 3; k++) {
                        vec3 w = vec3(h(k), h(k + 7), h(k + 13)) + 0.05; w /= w.x + w.y + w.z;
                        vec3 p = gp[0] * w.x + gp[1] * w.y + gp[2] * w.z, n = normalize(gn[0] * w.x + gn[1] * w.y + gn[2] * w.z), col = gc[0] * w.x + gc[1] * w.y + gc[2] * w.z;
                        vec3 wind = vec3(sin(t * 2.0 + p.x * 2.0), -0.9, cos(t * 1.7 + p.z * 2.0)) * 0.1;
                        vec3 side = normalize(cross(n, vec3(0.3, 1.0, 0.2))) * 0.012;
                        float len = 0.22 + 0.12 * h(k + 23);
                        for (int s = 0; s < 4; s++) {
                            float f = float(s) / 3.0;
                            vec3 q = p + n * len * f + wind * f * f;
                            float light = 0.3 + 0.7 * max((mat3(r) * n).y, 0.0);
                            c = col * light * (0.35 + 0.65 * f);
                            gl_Position = m * vec4(q - side * (1.0 - f), 1); EmitVertex();
                            if (s < 3) { gl_Position = m * vec4(q + side * (1.0 - f), 1); EmitVertex(); }
                        }
                        EndPrimitive();
                    }
                }",
            ),
            (FRAGMENT, "in vec3 c; out vec4 o; void main() { o = vec4(c, 1); }"),
        ]);
        Self { skin, hair }
    }
}

impl Scene for Fur {
    fn draw(&mut self, t: f32, _: f32, to: &Target) {
        to.begin([0.16, 0.2, 0.26]);
        let r = M::turn(t * 0.4, 0.6 + 0.3 * (t * 0.5).sin());
        let m = to.view(4.6, 0.8) * r;
        for p in [self.skin, self.hair] {
            // SAFETY: plain GL calls
            unsafe {
                glUseProgram(p);
                set(p, "t", t);
                set_matrix(p, "m", &m);
                set_matrix(p, "r", &r);
                glDrawArrays(TRIANGLES, 0, 48 * 20 * 6);
            }
        }
    }
}

/// A flight between twisted pillars, found ray by ray, on a small picture that is made larger.
struct Tunnel {
    p: u32,
    fb: u32,
}

const SMALL: (i32, i32) = (160, 90);

impl Tunnel {
    fn new() -> Self {
        let p = program(&[
            (VERTEX, "out vec2 uv; void main() { uv = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(uv * 2.0 - 1.0, 0, 1); uv = uv * 2.0 - 1.0; }"),
            (
                FRAGMENT,
                "in vec2 uv; uniform float t; uniform float aspect; out vec4 o;
                float map(vec3 p) {
                    float a = p.z * 0.35, s = sin(a), c = cos(a);
                    p.xy = mat2(c, -s, s, c) * p.xy;
                    vec2 q = abs(mod(p.xy, 3.0) - 1.5);
                    float pillar = length(q - 0.55 - 0.15 * sin(p.z * 1.7)) - 0.22;
                    vec2 b = vec2(length(p.xy) - 2.6, mod(p.z, 2.0) - 1.0);
                    return min(pillar, length(b) - 0.16) * 0.8;
                }
                void main() {
                    float k = t * 0.4, s = sin(k) * 0.25, c = sqrt(1.0 - s * s);
                    vec3 d = normalize(vec3(uv.x * aspect, uv.y, 1.4)), e = vec3(0, 0, t * 2.2);
                    d.xz = mat2(c, -s, s, c) * d.xz;
                    float z = 0.0, i = 0.0;
                    for (; i < 28.0; i += 1.0) { float m = map(e + d * z); if (m < 0.01 + z * 0.002 || z > 22.0) break; z += m; }
                    vec3 p = e + d * z, n = vec3(map(p + vec3(0.02, 0, 0)), map(p + vec3(0, 0.02, 0)), map(p + vec3(0, 0, 0.02))) - map(p);
                    n /= max(length(n), 1e-6);
                    vec3 col = 0.5 + 0.5 * cos(vec3(0, 2, 4) + p.z * 0.35 + length(p.xy));
                    col *= 0.25 + 0.75 * max(dot(n, normalize(vec3(0.3, 0.6, -0.7))), 0.0);
                    o = vec4(mix(col, vec3(0.02, 0.03, 0.08), smoothstep(6.0, 20.0, z)) + vec3(0.5, 0.35, 0.2) * i * 0.006, 1);
                }",
            ),
        ]);
        let (mut tex, mut fb) = (0, 0);
        // SAFETY: plain GL calls
        unsafe {
            glGenTextures(1, &mut tex);
            glBindTexture(TEXTURE_2D, tex);
            glTexStorage2D(TEXTURE_2D, 1, RGBA8, SMALL.0, SMALL.1);
            glGenFramebuffers(1, &mut fb);
            glBindFramebuffer(FRAMEBUFFER, fb);
            glFramebufferTexture2D(FRAMEBUFFER, COLOR_ATTACHMENT0, TEXTURE_2D, tex, 0);
        }
        Self { p, fb }
    }
}

impl Scene for Tunnel {
    fn draw(&mut self, t: f32, _: f32, to: &Target) {
        // SAFETY: plain GL calls
        unsafe {
            glBindFramebuffer(FRAMEBUFFER, self.fb);
            glViewport(0, 0, SMALL.0, SMALL.1);
            glDisable(DEPTH_TEST);
            glDisable(BLEND);
            glDisable(CULL_FACE);
            glUseProgram(self.p);
            set(self.p, "t", t);
            set(self.p, "aspect", to.w as f32 / to.h as f32);
            glDrawArrays(TRIANGLES, 0, 3);
            glBindFramebuffer(READ_FRAMEBUFFER, self.fb);
            glBindFramebuffer(DRAW_FRAMEBUFFER, to.fb);
            let (y0, y1) = if to.up < 0.0 { (to.h, 0) } else { (0, to.h) };
            glBlitFramebuffer(0, 0, SMALL.0, SMALL.1, 0, y0, to.w, y1, COLOR_BUFFER_BIT, LINEAR as u32);
        }
    }
}

/// A knot over a floor, with its shadow.
struct Shadow {
    depth: u32,
    show: u32,
    map: u32,
    fb: u32,
}

const MAP: i32 = 384;
const KNOT: i32 = 96 * 8 * 6;

const SOLID: &str = "
const int U = 96, V = 8;
const ivec2 Q[6] = ivec2[6](ivec2(0,0), ivec2(1,0), ivec2(0,1), ivec2(0,1), ivec2(1,0), ivec2(1,1));
vec3 path(float a) { return vec3((2.0 + cos(3.0 * a)) * cos(2.0 * a), sin(3.0 * a), (2.0 + cos(3.0 * a)) * sin(2.0 * a)) * 0.42; }
void knot(int id, out vec3 p, out vec3 n) {
    int c = id / 6; ivec2 q = ivec2(c % U, c / U) + Q[id % 6];
    float a = float(q.x) / float(U) * 6.28319, b = float(q.y) / float(V) * 6.28319;
    vec3 o = path(a), f = normalize(path(a + 0.01) - o), s = normalize(cross(f, vec3(0, 1, 0.1))), u = cross(s, f);
    n = s * cos(b) + u * sin(b); p = o + n * 0.2;
}
void solid(out vec3 p, out vec3 n, mat4 r) {
    if (gl_VertexID < U * V * 6) { knot(gl_VertexID, p, n); p = mat3(r) * p; n = mat3(r) * n; }
    else { ivec2 q = Q[gl_VertexID - U * V * 6]; p = vec3(float(q.y) * 8.0 - 4.0, -1.6, float(q.x) * 8.0 - 4.0); n = vec3(0, 1, 0); }
}
";

impl Shadow {
    fn new() -> Self {
        let vs = format!("{SOLID} uniform mat4 l; uniform mat4 r; void main() {{ vec3 p, n; solid(p, n, r); gl_Position = l * vec4(p, 1); }}");
        let depth = program(&[(VERTEX, &vs), (FRAGMENT, "void main() {}")]);
        let vs = format!("{SOLID} uniform mat4 l; uniform mat4 m; uniform mat4 r; out vec3 n; out vec3 w; out vec4 s; void main() {{ vec3 p; solid(p, n, r); w = p; s = l * vec4(p, 1); gl_Position = m * vec4(p, 1); }}");
        let show = program(&[
            (VERTEX, &vs),
            (
                FRAGMENT,
                "in vec3 n; in vec3 w; in vec4 s; uniform sampler2DShadow map; out vec4 o;
                void main() {
                    vec3 q = s.xyz / s.w * 0.5 + 0.5; q.z -= 0.004;
                    float lit = texture(map, q);
                    vec3 l = normalize(vec3(0.5, 1.0, 0.35)), m = normalize(n);
                    bool ground = w.y < -1.55;
                    vec2 f = floor(w.xz * 1.5);
                    vec3 c = ground ? mix(vec3(0.25, 0.27, 0.3), vec3(0.7, 0.68, 0.62), mod(f.x + f.y, 2.0)) : vec3(0.15, 0.55, 0.85);
                    float d = max(dot(m, l), 0.0) * lit;
                    vec3 v = normalize(vec3(0, 2.0, 5.0) - w);
                    float h = ground ? 0.0 : pow(max(dot(m, normalize(l + v)), 0.0), 40.0) * lit;
                    float fog = smoothstep(3.0, 5.0, length(w.xz));
                    o = vec4(mix(c * (0.22 + 0.85 * d) + h, vec3(0.5, 0.58, 0.68), ground ? fog : 0.0), 1);
                }",
            ),
        ]);
        let (mut map, mut fb) = (0, 0);
        // SAFETY: plain GL calls
        unsafe {
            glGenTextures(1, &mut map);
            glBindTexture(TEXTURE_2D, map);
            glTexStorage2D(TEXTURE_2D, 1, DEPTH24, MAP, MAP);
            for (what, v) in [(0x2801, LINEAR), (0x2800, LINEAR), (0x2802, 0x812f), (0x2803, 0x812f), (0x884c, 0x884e)] {
                glTexParameteri(TEXTURE_2D, what, v);
            }
            glGenFramebuffers(1, &mut fb);
            glBindFramebuffer(FRAMEBUFFER, fb);
            glFramebufferTexture2D(FRAMEBUFFER, DEPTH_ATTACHMENT, TEXTURE_2D, map, 0);
        }
        Self { depth, show, map, fb }
    }
}

impl Scene for Shadow {
    fn draw(&mut self, t: f32, _: f32, to: &Target) {
        let l = M::ortho(2.4, 0.5, 9.0) * M::look([2.0, 4.0, 1.4], [0.0, -0.4, 0.0]);
        let r = M::turn(t * 0.6, t * 0.35);
        begin(self.fb, MAP, MAP, [0.0; 3]);
        // SAFETY: plain GL calls
        unsafe {
            glUseProgram(self.depth);
            set_matrix(self.depth, "l", &l);
            set_matrix(self.depth, "r", &r);
            glDrawArrays(TRIANGLES, 0, KNOT + 6);
        }
        to.begin([0.5, 0.58, 0.68]);
        // SAFETY: plain GL calls
        unsafe {
            glUseProgram(self.show);
            glBindTexture(TEXTURE_2D, self.map);
            set_matrix(self.show, "l", &l);
            set_matrix(self.show, "r", &r);
            let a = t * 0.2;
            let m = to.perspective(0.8) * M::look([5.0 * a.sin(), 1.6, 5.0 * a.cos()], [0.0, -0.5, 0.0]);
            set_matrix(self.show, "m", &m);
            glDrawArrays(TRIANGLES, 0, KNOT + 6);
        }
    }
}
