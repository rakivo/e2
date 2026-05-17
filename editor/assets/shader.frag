#version 450
layout(location=0) in vec2 f_uv;
layout(location=1) in vec4 f_color;
layout(location=0) out vec4 out_color;

layout(set=0, binding=0) uniform texture2D tex;
layout(set=0, binding=1) uniform sampler   smp;

const float ATLAS_SIZE = 4096.0;

void main() {
    if (f_uv.x == 0.0 && f_uv.y == 0.0) {
        out_color = f_color;
        return;
    }
    vec2 uv = f_uv / ATLAS_SIZE;
    float a = texture(sampler2D(tex, smp), uv).r;
    out_color = f_color * a;
}