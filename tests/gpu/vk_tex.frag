#version 450
layout(location = 0) in vec2 t;
layout(location = 0) out vec4 o;
layout(binding = 0) uniform sampler2D s;
layout(push_constant) uniform P { vec4 k; } pc;
void main() { vec4 c = texture(s, t); o = vec4(c.rgb, c.a * pc.k.x * 1.5); }
