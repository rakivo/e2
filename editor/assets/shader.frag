#version 450

layout(location=0) in vec2 f_uv;
layout(location=1) in vec3 f_uv2;
layout(location=2) in vec4 f_color;
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

    // SOFT FLASHLIGHT
    if (f_uv.x > 49000.0) {
        vec2  center    = vec2(f_uv.x - 49000.0, f_uv.y);
        float radius    = f_uv2.x;
        float dist      = length(vec2(gl_FragCoord.x, gl_FragCoord.y) - center);

        float intensity = exp(-pow(dist / (radius * 0.7), 2.0));
        intensity      *= smoothstep(radius, radius * 0.8, dist);

        float alpha     = intensity * f_color.a;
        if (alpha <= 0.01) discard;

        out_color = vec4(f_color.rgb * alpha, alpha);
        return;
    }

    // ROUNDED RECT OUTLINE
    if (f_uv.x > 39000.0) {
        float rx        = f_uv.x - 39000.0;
        float ry        = f_uv.y;
        float hw        = (f_uv2.x - rx) * 0.5;
        float hh        = (f_uv2.y - ry) * 0.5;
        float radius    = f_color.a * min(hw, hh);
        float thickness = f_uv2.z   * min(hw, hh);

        vec2  center = vec2(rx + hw, ry + hh);
        vec2  p      = vec2(gl_FragCoord.x, gl_FragCoord.y) - center;
        vec2  q      = abs(p) - vec2(hw - radius, hh - radius);
        float dist   = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;

        float aa    = fwidth(dist);
        float outer = smoothstep(aa, -aa, dist);
        float inner = smoothstep(-thickness + aa, -thickness - aa, dist);
        float alpha = outer - inner;
        if (alpha <= 0.01) discard;
        out_color = vec4(f_color.rgb * alpha, alpha);
        return;
    }

    // ROUNDED RECT FILLED
    if (f_uv.x > 29000.0) {
        float rx     = f_uv.x - 29000.0;
        float ry     = f_uv.y;
        float hw     = (f_uv2.x - rx) * 0.5;
        float hh     = (f_uv2.y - ry) * 0.5;
        float radius = f_uv2.z * min(hw, hh);

        vec2  center = vec2(rx + hw, ry + hh);
        vec2  p      = vec2(gl_FragCoord.x, gl_FragCoord.y) - center;
        vec2  q      = abs(p) - vec2(hw - radius, hh - radius);
        float dist   = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;

        float aa    = fwidth(dist);
        float alpha = smoothstep(aa, -aa, dist) * f_color.a;
        if (alpha <= 0.01) discard;
        out_color = vec4(f_color.rgb * alpha, alpha);
        return;
    }

    // SDF LINES
    if (f_uv.x > 19000.0) {
        float dist = abs(f_uv.y);

        // fwidth gets the change in UV space per pixel. Since we normalized our UVs
        // to visual radius, this represents exactly 1 screen pixel in UV space.
        float aa = fwidth(f_uv.y);

        float alpha = smoothstep(1.0 + aa, 1.0 - aa, dist);
        if (alpha <= 0.01) discard;

        float a = f_color.a * alpha;
        out_color = vec4(f_color.rgb * a, a);
        return;
    }

    // SDF CIRCLES
    if (f_uv.x > 9000.0) {
        vec2 sdf_uv = f_uv - vec2(10000.0);
        float dist = length(sdf_uv);

        // Convert dist to pixels: radius maps UV 0..1 to 0..radius_px
        // fwidth gives UV change per pixel, so 1/fwidth = pixels per UV unit
        float px_size = 1.0 / fwidth(dist);  // pixels per UV unit = radius in pixels
        float aa = 1.0 / px_size;            // 1 pixel in UV space

        float alpha = smoothstep(1.0 + aa, 1.0 - aa, dist);
        if (alpha <= 0.01) discard;
        float a = f_color.a * alpha;
        out_color = vec4(f_color.rgb * a, a);
        return;
    }

    vec2 uv = f_uv / ATLAS_SIZE;
    float a = texture(sampler2D(tex, smp), uv).r;
    out_color = f_color * a;
}
