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
    vec2  screen_size;
    float time;
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

    // SEARCH MATCH WAVE
    if (f_uv.x > 59000.0) {
        float rx = f_uv.x - 59000.0;
        float ry = f_uv.y;
        float rw = f_uv2.x;
        float rh = f_uv2.y;

        vec2 uv = (gl_FragCoord.xy - vec2(rx, ry)) / vec2(rw, rh);
        float t = pc.time * 7.9;

        //
        // Each row of pixels gets its own wave phase offset based on y
        //
        float y = uv.y;
        float x = uv.x;

        //
        // Trochoidal layers
        //
        float w = 0.0;
        w += sin(x * 6.2831 - t * 1.0000 + y * 0.30) * 0.20;
        w += sin(x * 4.7123 - t * 1.6180 + y * 0.52) * 0.18;
        w += sin(x * 9.4247 - t * 1.4142 + y * 0.18) * 0.14;
        w += sin(x * 3.1415 - t * 1.7320 + y * 0.71) * 0.16;
        w += sin(x * 7.8539 - t * 1.2360 + y * 0.13) * 0.12;
        w += sin(x * 5.4977 - t * 1.9318 + y * 0.61) * 0.10;
        w += sin(x * 2.3561 - t * 1.1180 + y * 0.44) * 0.10;

        // float brightness = 0.7 + 0.3 * w;

        // vec3 base   = vec3(0.50, 0.04, 0.06);
        // vec3 bright = vec3(0.86, 0.08, 0.14);
        // vec3 col = mix(base, bright, brightness);

        // float alpha = f_color.a * clamp(0.8 + 0.2 * w, 0.0, 1.0);
        // if (alpha <= 0.01) discard;

        // =========================================

        // float brightness = 0.7 + 0.3 * w;
        // vec3 col = f_color.rgb * brightness;

        // float alpha = f_color.a * clamp(0.8 + 0.2 * w, 0.0, 1.0);
        // if (alpha <= 0.01) discard;

        // out_color = vec4(col * alpha, alpha);

        // =========================================

        // 1. Map the wave from [-1.0, 1.0] to a clean [0.0, 1.0] range
        float wave_normalized = w * 0.5 + 0.5;

        // 2. Modulate the input color directly so the hue NEVER changes.
        // At the peak, it hits 100% of your Rust color. At the valley, it drops to 70%.
        float brightness = mix(0.70, 1.0, wave_normalized);
        vec3 col = f_color.rgb * brightness;

        // 3. Keep the alpha baseline high so it stays punchy
        float alpha = f_color.a * clamp(0.9 + 0.1 * w, 0.0, 1.0);
        if (alpha <= 0.01) discard;

        out_color = vec4(col * alpha, alpha);

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
