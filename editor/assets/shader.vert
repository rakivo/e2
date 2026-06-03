#version 450

layout(location=0) in vec2 pos;
layout(location=1) in vec2 uv;
layout(location=2) in vec3 uv2;
layout(location=3) in vec4 color;

layout(location=0) out vec2 f_uv;
layout(location=1) out vec3 f_uv2;
layout(location=2) out vec4 f_color;

void main() {
    gl_Position = vec4(pos.x, -pos.y, 0.0, 1.0);
    f_uv    = uv;
    f_uv2   = uv2;
    f_color = color;
}
