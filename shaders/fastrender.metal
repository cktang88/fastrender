// fastrender shaders. Loaded at runtime (newLibraryWithSource) so editing this file while the app
// runs hot-reloads it. Struct layouts must match src/core.rs.
#include <metal_stdlib>
using namespace metal;

struct Instance { float4 a; float4 b; };            // a: centre.xyz, yaw; b: half.xyz, rgba8 bits
struct Light { float4 pos_radius; float4 color; };
struct Vertex { packed_float3 pos; packed_float3 nrm; };
struct Frame {
    float4x4 view_proj;
    float4x4 inv_view_proj;
    float4x4 shadow_vp;
    float4 cam_pos;      // xyz, w = time
    float4 sun_dir;      // xyz towards the sun, w = intensity
    float4 fog;          // rgb, w = density
    float4 frustum[6];
    uint4 counts;        // x instances, y lights
    float4 viewport;     // xy content px, zw target px
};
struct DrawArgs { uint index_count; atomic_uint instance_count; uint index_start; int base_vertex; uint base_instance; };

// ------------------------------------------------------------------ GPU culling

kernel void cull(device const Instance* inst [[buffer(0)]],
                 constant Frame& f [[buffer(1)]],
                 device uint* visible [[buffer(2)]],
                 device DrawArgs& args [[buffer(3)]],
                 uint id [[thread_position_in_grid]]) {
    if (id >= f.counts.x) return;
    float3 c = inst[id].a.xyz;
    float r = length(inst[id].b.xyz);
    for (int p = 0; p < 6; p++)
        if (dot(f.frustum[p].xyz, c) + f.frustum[p].w < -r) return;
    // distance cull of small props (fog hides them anyway)
    if (r < 1.5 && distance(c, f.cam_pos.xyz) > 220.0) return;
    uint slot = atomic_fetch_add_explicit(&args.instance_count, 1, memory_order_relaxed);
    visible[slot] = id;
}

// ------------------------------------------------------------------ geometry

struct VOut {
    float4 pos [[position]];
    float3 wpos;
    half3 nrm;
    half3 albedo;
};

static float3 rotate_y(float3 p, float yaw) {
    float s = sin(yaw), c = cos(yaw);
    return float3(c * p.x + s * p.z, p.y, -s * p.x + c * p.z);
}

static half3 unpack_rgb(float bits) {
    uint u = as_type<uint>(bits);
    return half3(half(u & 255u), half((u >> 8) & 255u), half((u >> 16) & 255u)) / 255.0h;
}

vertex VOut vs_main(uint vid [[vertex_id]], uint iid [[instance_id]],
                    device const Vertex* verts [[buffer(0)]],
                    device const Instance* inst [[buffer(1)]],
                    device const uint* visible [[buffer(2)]],
                    constant Frame& f [[buffer(3)]]) {
    Instance i = inst[visible[iid]];
    Vertex v = verts[vid];
    float3 w = rotate_y(float3(v.pos) * i.b.xyz, i.a.w) + i.a.xyz;
    VOut o;
    o.pos = f.view_proj * float4(w, 1.0);
    o.wpos = w;
    o.nrm = half3(rotate_y(float3(v.nrm), i.a.w));
    o.albedo = unpack_rgb(i.b.w);
    // cheap variation: darker toward the ground, like grime / AO
    o.albedo *= half(mix(0.75, 1.0, saturate(w.y / 6.0)));
    return o;
}

struct ShadowOut { float4 pos [[position]]; };

vertex ShadowOut vs_shadow(uint vid [[vertex_id]], uint iid [[instance_id]],
                           device const Vertex* verts [[buffer(0)]],
                           device const Instance* inst [[buffer(1)]],
                           constant Frame& f [[buffer(3)]]) {
    Instance i = inst[iid];
    float3 w = rotate_y(float3(verts[vid].pos) * i.b.xyz, i.a.w) + i.a.xyz;
    ShadowOut o;
    o.pos = f.shadow_vp * float4(w, 1.0);
    return o;
}

// ------------------------------------------------------------------ lighting helpers

static half sun_shadow(float3 wpos, depth2d<float> sm, constant Frame& f) {
    float4 sp = f.shadow_vp * float4(wpos, 1.0);
    float3 p = sp.xyz / sp.w;
    float2 uv = p.xy * float2(0.5, -0.5) + 0.5;
    if (any(uv < 0.0) || any(uv > 1.0) || p.z > 1.0) return 1.0h;
    constexpr sampler s(coord::normalized, filter::linear, address::clamp_to_edge, compare_func::less_equal);
    return half(sm.sample_compare(s, uv, p.z - 0.0008));
}

static half3 sky(half3 n) {
    return mix(half3(0.22h, 0.20h, 0.18h), half3(0.40h, 0.50h, 0.72h), n.y * 0.5h + 0.5h);
}

static half3 sun_light(float3 wpos, half3 n, half3 albedo, half sh, constant Frame& f) {
    half3 l = half3(f.sun_dir.xyz);
    half ndl = saturate(dot(n, l));
    half3 c = albedo * (sky(n) * 0.45h + half3(1.0h, 0.92h, 0.8h) * half(f.sun_dir.w) * ndl * sh);
    return c;
}

static half3 point_light(float3 wpos, half3 n, half3 albedo, Light L) {
    float3 d = L.pos_radius.xyz - wpos;
    float dist2 = dot(d, d);
    float r = L.pos_radius.w;
    if (dist2 > r * r) return 0.0h;
    half att = half(saturate(1.0 - dist2 / (r * r)));
    att *= att;
    half ndl = saturate(dot(n, half3(d * rsqrt(dist2))));
    return albedo * half3(L.color.rgb) * (ndl * att);
}

static half3 apply_fog(half3 c, float3 wpos, constant Frame& f) {
    float d = distance(wpos, f.cam_pos.xyz);
    half k = half(1.0 - exp(-d * f.fog.w));
    return mix(c, half3(f.fog.rgb), k);
}

static half3 tonemap(half3 c) {
    // ACES fit (Narkowicz), then gamma 2.2: output is display-referred, stored in an 8-bit target
    c = saturate((c * (2.51h * c + 0.03h)) / (c * (2.43h * c + 0.59h) + 0.14h));
    return pow(c, half3(1.0h / 2.2h));
}

// ------------------------------------------------------------------ forward

fragment half4 fs_forward(VOut in [[stage_in]],
                          constant Frame& f [[buffer(0)]],
                          device const Light* lights [[buffer(1)]],
                          depth2d<float> sm [[texture(0)]]) {
    half3 n = normalize(in.nrm);
    half3 c = sun_light(in.wpos, n, in.albedo, sun_shadow(in.wpos, sm, f), f);
    for (uint i = 0; i < f.counts.y; i++) c += point_light(in.wpos, n, in.albedo, lights[i]);
    return half4(tonemap(apply_fog(c, in.wpos, f)), 1.0h);
}

// ------------------------------------------------------------------ single-pass deferred
// Attachments: 0 hdr (rgba16f), 1 albedo+shadow (rgba8), 2 normal (rgba8), 3 linear depth (r32f),
// 4 output (bgra8, the only one stored when memoryless is on).

struct GBuffer {
    half4 hdr [[color(0)]];
    half4 albedo [[color(1)]];
    half4 normal [[color(2)]];
    float depth [[color(3)]];
};

fragment GBuffer fs_gbuffer(VOut in [[stage_in]],
                            constant Frame& f [[buffer(0)]],
                            depth2d<float> sm [[texture(0)]]) {
    GBuffer g;
    half3 n = normalize(in.nrm);
    half sh = sun_shadow(in.wpos, sm, f);
    g.albedo = half4(in.albedo, sh);
    g.normal = half4(n * 0.5h + 0.5h, 0.0h);
    g.depth = in.pos.z;
    // sun + sky are resolved here already: the full-screen pass only adds fog, the light
    // volumes add point lights on top
    g.hdr = half4(sun_light(in.wpos, n, in.albedo, sh, f), 1.0h);
    return g;
}

struct FsqOut { float4 pos [[position]]; float2 uv; };

vertex FsqOut vs_fsq(uint vid [[vertex_id]]) {
    float2 p = float2((vid << 1) & 2, vid & 2);
    FsqOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = float2(p.x, 1.0 - p.y);
    return o;
}

static float3 world_from_depth(float4 frag_pos, float depth, constant Frame& f) {
    float2 ndc = frag_pos.xy / f.viewport.xy * float2(2.0, -2.0) + float2(-1.0, 1.0);
    float4 w = f.inv_view_proj * float4(ndc, depth, 1.0);
    return w.xyz / w.w;
}

struct LightVOut { float4 pos [[position]]; uint light [[flat]]; };

vertex LightVOut vs_light(uint vid [[vertex_id]], uint iid [[instance_id]],
                          device const Vertex* verts [[buffer(0)]],
                          device const Light* lights [[buffer(1)]],
                          constant Frame& f [[buffer(3)]]) {
    Light L = lights[iid];
    float3 w = float3(verts[vid].pos) * L.pos_radius.w + L.pos_radius.xyz;
    LightVOut o;
    o.pos = f.view_proj * float4(w, 1.0);
    // volumes that cross the near plane: clamp so the back faces still rasterise
    o.pos.z = min(o.pos.z, o.pos.w * 0.9999);
    o.light = iid;
    return o;
}

struct GRead {
    half4 albedo [[color(1)]];
    half4 normal [[color(2)]];
    float depth [[color(3)]];
};

// additive (hardware blend) into color(0), reading the G-buffer from tile memory
fragment half4 fs_light(LightVOut in [[stage_in]], GRead g,
                        constant Frame& f [[buffer(0)]],
                        device const Light* lights [[buffer(1)]]) {
    if (g.depth >= 1.0) discard_fragment();
    float3 wpos = world_from_depth(in.pos, g.depth, f);
    half3 n = normalize(g.normal.xyz * 2.0h - 1.0h);
    return half4(point_light(wpos, n, g.albedo.rgb, lights[in.light]), 0.0h);
}

struct TonemapIn { half4 hdr [[color(0)]]; float depth [[color(3)]]; };
struct TonemapOut { half4 out [[color(4)]]; };

fragment TonemapOut fs_tonemap(FsqOut in [[stage_in]], TonemapIn t, constant Frame& f [[buffer(0)]]) {
    TonemapOut o;
    half3 c = t.hdr.rgb;
    if (t.depth < 1.0) c = apply_fog(c, world_from_depth(in.pos, t.depth, f), f);
    else c = half3(f.fog.rgb);
    o.out = half4(tonemap(c), 1.0h);
    return o;
}

// ------------------------------------------------------------------ upscale / present

fragment half4 fs_blit(FsqOut in [[stage_in]],
                       texture2d<half> src [[texture(0)]],
                       constant float4& scale [[buffer(0)]]) {
    constexpr sampler s(coord::normalized, filter::linear, address::clamp_to_edge);
    return src.sample(s, in.uv * scale.xy);
}
