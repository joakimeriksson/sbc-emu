#version 450
layout(location = 0) in vec4 pos;
layout(location = 1) in vec4 uv;
layout(location = 0) out vec2 t;
void main() { gl_Position = pos; t = uv.xy; }
