#version 450
layout(location = 0) in vec4 pos;
layout(location = 1) in vec4 col;
layout(location = 0) out vec4 v;
layout(push_constant) uniform P { vec4 k; } pc;
void main() { gl_Position = pos; v = col * (1.0 - pc.k.y) + vec4(pc.k.y * 0.5); }
