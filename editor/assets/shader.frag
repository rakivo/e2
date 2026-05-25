#version 450

layout(location=0) in vec2 f_uv;
layout(location=1) in vec4 f_color;
layout(location=0) out vec4 out_color;

layout(set=0, binding=0) uniform texture2D tex;       // Atlas
layout(set=0, binding=1) uniform sampler   smp;
layout(set=0, binding=2) uniform texture2D blur_tex;  // Blurred scene
layout(set=0, binding=3) uniform sampler   blur_smp;

layout(push_constant) uniform PushConstants {
    vec2 screen_size;
} pc;

const float ATLAS_SIZE = 4096.0;
const float BLUR_SENTINEL = -1.0;

void main() {
    if (f_uv.x < -0.5) {
        vec2 screen_uv = vec2(gl_FragCoord.x / pc.screen_size.x,
                              gl_FragCoord.y / pc.screen_size.y);

        vec3 blur_val = texture(sampler2D(blur_tex, blur_smp), screen_uv).rgb;
        out_color = vec4(mix(blur_val, f_color.rgb, f_color.a), 1.0);

        return;
    }

    if (f_uv.x == 0.0 && f_uv.y == 0.0) {
        out_color = f_color;
        return;
    }

    vec2 uv = f_uv / ATLAS_SIZE;
    float a = texture(sampler2D(tex, smp), uv).r;
    out_color = f_color * a;
}
