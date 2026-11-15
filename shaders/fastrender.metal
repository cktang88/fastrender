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
    float4 fog2;         // three.js fog: near|density, far, mode (0 exp, 1 linear, 2 exp2, 3 none), w: tone mapping (three constants)
};
struct DrawArgs { uint index_count; atomic_uint instance_count; uint index_start; int base_vertex; uint base_instance; };

// ------------------------------------------------------------------ GPU culling

// one kernel for the camera list and the shadow-caster list: the caller passes the 6 planes
struct CullParams { float4 planes[6]; float4 cam_pos_prop_dist; uint count; };

kernel void cull(device const Instance* inst [[buffer(0)]],
                 constant CullParams& cp [[buffer(1)]],
                 device uint* visible [[buffer(2)]],
                 device DrawArgs& args [[buffer(3)]],
                 uint id [[thread_position_in_grid]]) {
    if (id >= cp.count) return;
    float3 c = inst[id].a.xyz;
    float r = length(inst[id].b.xyz);
    for (int p = 0; p < 6; p++)
        if (dot(cp.planes[p].xyz, c) + cp.planes[p].w < -r) return;
    // distance cull of small props (where fog has mostly hidden them)
    if (r < 1.5 && distance(c, cp.cam_pos_prop_dist.xyz) > cp.cam_pos_prop_dist.w) return;
    uint slot = atomic_fetch_add_explicit(&args.instance_count, 1, memory_order_relaxed);
    visible[slot] = id;
}

// ------------------------------------------------------------------ geometry

struct VOut {
    float4 pos [[position]];
    float3 wpos;
    half3 nrm [[flat]];   // faces are flat: no interpolation, fewer varyings to bin
    half3 albedo;         // interpolated: voxel vertices carry baked AO
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
                           device const uint* visible [[buffer(2)]],
                           constant Frame& f [[buffer(3)]]) {
    Instance i = inst[visible[iid]];
    float3 w = rotate_y(float3(verts[vid].pos) * i.b.xyz, i.a.w) + i.a.xyz;
    ShadowOut o;
    o.pos = f.shadow_vp * float4(w, 1.0);
    return o;
}

// ------------------------------------------------------------------ voxel chunks
// 8-byte vertices from src/voxel.rs `pack`: w0 = x|y<<6|z<<12 (chunk-local) | face dir<<18 | emit<<21,
// w1 = linear rgb 10:10:10 with the mesher's AO baked in. [[vertex_id]] includes baseVertex and
// [[instance_id]] includes baseInstance, which is the chunk index (origin lookup).

constant half3 FACE_N[6] = { half3(1, 0, 0), half3(-1, 0, 0), half3(0, 1, 0), half3(0, -1, 0), half3(0, 0, 1), half3(0, 0, -1) };

static float3 voxel_pos(uint w0, int4 origin) {
    return float3(w0 & 63u, (w0 >> 6) & 63u, (w0 >> 12) & 63u) + float3(origin.xyz);
}

vertex VOut vs_voxel(uint vid [[vertex_id]], uint iid [[instance_id]],
                     device const uint2* verts [[buffer(0)]],
                     device const int4* chunks [[buffer(1)]],
                     constant Frame& f [[buffer(3)]]) {
    uint2 v = verts[vid];
    float3 w = voxel_pos(v.x, chunks[iid]);
    VOut o;
    o.pos = f.view_proj * float4(w, 1.0);
    o.wpos = w;
    o.nrm = FACE_N[min((v.x >> 18) & 7u, 5u)];
    o.albedo = half3(half(v.y & 1023u), half((v.y >> 10) & 1023u), half((v.y >> 20) & 1023u)) / 1023.0h;
    o.albedo += o.albedo * half((v.x >> 21) & 127u) / 127.0h * 3.0h; // emissive blocks glow
    return o;
}

vertex ShadowOut vs_voxel_shadow(uint vid [[vertex_id]], uint iid [[instance_id]],
                                 device const uint2* verts [[buffer(0)]],
                                 device const int4* chunks [[buffer(1)]],
                                 constant Frame& f [[buffer(3)]]) {
    ShadowOut o;
    o.pos = f.shadow_vp * float4(voxel_pos(verts[vid].x, chunks[iid]), 1.0);
    return o;
}

// ------------------------------------------------------------------ scripted boxes (JS three-shim)
// 3x4 world rows already scaled by the box half extents (the cube spans [-1,1]); see src/script.rs.
struct ScriptInstance { float4 r0; float4 r1; float4 r2; uint color; uint pad0; uint pad1; uint pad2; };

static float3 script_world(ScriptInstance i, float3 p) {
    float4 p4 = float4(p, 1.0);
    return float3(dot(i.r0, p4), dot(i.r1, p4), dot(i.r2, p4));
}

vertex VOut vs_script(uint vid [[vertex_id]], uint iid [[instance_id]],
                      device const Vertex* verts [[buffer(0)]],
                      device const ScriptInstance* inst [[buffer(1)]],
                      constant Frame& f [[buffer(3)]]) {
    ScriptInstance i = inst[iid];
    Vertex v = verts[vid];
    float3 w = script_world(i, float3(v.pos));
    // normal: rotation x non-uniform scale -> inverse-transpose = columns divided by their squared length
    float3 c0 = float3(i.r0.x, i.r1.x, i.r2.x), c1 = float3(i.r0.y, i.r1.y, i.r2.y), c2 = float3(i.r0.z, i.r1.z, i.r2.z);
    float3 n = float3(v.nrm);
    float3 wn = c0 * (n.x / max(dot(c0, c0), 1e-8)) + c1 * (n.y / max(dot(c1, c1), 1e-8)) + c2 * (n.z / max(dot(c2, c2), 1e-8));
    VOut o;
    o.pos = f.view_proj * float4(w, 1.0);
    o.wpos = w;
    o.nrm = half3(normalize(wn));
    o.albedo = unpack_rgb(as_type<float>(i.color));
    return o;
}

vertex ShadowOut vs_script_shadow(uint vid [[vertex_id]], uint iid [[instance_id]],
                                  device const Vertex* verts [[buffer(0)]],
                                  device const ScriptInstance* inst [[buffer(1)]],
                                  constant Frame& f [[buffer(3)]]) {
    ShadowOut o;
    o.pos = f.shadow_vp * float4(script_world(inst[iid], float3(verts[vid].pos)), 1.0);
    return o;
}

// ------------------------------------------------------------------ lighting helpers

static half sun_shadow(float3 wpos, depth2d<float> sm, constant Frame& f) {
    float4 sp = f.shadow_vp * float4(wpos, 1.0);
    float3 p = sp.xyz / sp.w;
    float2 uv = p.xy * float2(0.5, -0.5) + 0.5;
    if (any(uv < 0.0) || any(uv > 1.0) || p.z > 1.0) return 1.0h;
    constexpr sampler s(coord::normalized, filter::linear, address::clamp_to_edge, compare_func::less_equal);
    half sh = half(sm.sample_compare(s, uv, p.z - 0.0008));
    // fade out toward the edge of the shadow box instead of a hard cut-off
    float edge = max(abs(uv.x * 2.0 - 1.0), abs(uv.y * 2.0 - 1.0));
    return mix(sh, 1.0h, half(smoothstep(0.8, 1.0, edge)));
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

static half3 point_light(float3 wpos, half3 n, half3 albedo, constant Light& L) {
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
                          constant Light* lights [[buffer(1)]],
                          depth2d<float> sm [[texture(0)]]) {
    half3 n = in.nrm;
    half3 c = sun_light(in.wpos, n, in.albedo, sun_shadow(in.wpos, sm, f), f);
    for (uint i = 0; i < f.counts.y; i++) c += point_light(in.wpos, n, in.albedo, lights[i]);
    return half4(tonemap(apply_fog(c, in.wpos, f)), 1.0h);
}

// ------------------------------------------------------------------ single-pass deferred
// Attachments: 0 hdr (rg11b10f), 1 albedo (rgba8), 2 normal (rgba8), 3 depth copy (r32f, NDC z:
// the depth attachment itself can't be read in-tile),
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
    half3 n = in.nrm;
    half sh = sun_shadow(in.wpos, sm, f);
    g.albedo = half4(in.albedo, 0.0h);
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
                          constant Light* lights [[buffer(1)]],
                          constant Frame& f [[buffer(3)]]) {
    Light L = lights[iid];
    float3 w = float3(verts[vid].pos) * L.pos_radius.w + L.pos_radius.xyz;
    LightVOut o;
    o.pos = f.view_proj * float4(w, 1.0);
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
                        constant Light* lights [[buffer(1)]]) {
    // additive 0 is a no-op; discard would cost more on TBDR than the add
    if (g.depth >= 1.0) return 0.0h;
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
    // scale.xy = content / texture size, scale.zw = half a texel: clamp to the content edge so the
    // stale texels outside the content rectangle never bleed in
    constexpr sampler s(coord::normalized, filter::linear, address::clamp_to_edge);
    return src.sample(s, min(in.uv * scale.xy, scale.xy - scale.zw));
}

// ------------------------------------------------------------------ MetalFX temporal: motion from depth
// Motion vectors for MTLFXTemporalScaler without any per-shader velocity output: each pixel's surface
// point is rebuilt from the depth buffer with the inverse of the *jittered* view-projection the frame
// was drawn with, then projected with this frame's and the previous frame's *unjittered*
// view-projections. Output: prev_uv - cur_uv in UV of the content rect (Rust mirror and conventions:
// core::reproject_motion); the scaler's motionVectorScale = content size turns it into pixels.
// Exact for static geometry under any camera motion; objects moving on their own get camera-only
// motion (ghosting), see docs/notes/metalfx-temporal.md.

struct MotionParams {
    float4x4 inv_cur_jittered;
    float4x4 cur;
    float4x4 prev;
    float4 content; // xy = content size in pixels, zw unused
};

kernel void motion_from_depth(depth2d<float, access::read> depth [[texture(0)]],
                              texture2d<half, access::write> motion [[texture(1)]],
                              constant MotionParams& p [[buffer(0)]],
                              uint2 gid [[thread_position_in_grid]]) {
    if (float(gid.x) >= p.content.x || float(gid.y) >= p.content.y) return;
    float d = depth.read(gid);
    float2 uv = (float2(gid) + 0.5) / p.content.xy;
    float4 w = p.inv_cur_jittered * float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), d, 1.0);
    w /= w.w;
    float4 c = p.cur * float4(w.xyz, 1.0);
    float4 q = p.prev * float4(w.xyz, 1.0);
    float2 m = 0.0;
    if (c.w > 1e-6 && q.w > 1e-6) {
        float2 cuv = c.xy / c.w * float2(0.5, -0.5) + 0.5;
        float2 puv = q.xy / q.w * float2(0.5, -0.5) + 0.5;
        m = puv - cuv;
    }
    motion.write(half4(half2(m), 0.0h, 0.0h), gid);
}

// ------------------------------------------------------------------ three.js scenes (protocol v2)
// A real three.js scene graph, walked by js/fastrender-three.js. Geometry keeps three's streams
// (position, normal, uv, colour); materials are a table; lights use three.js r155+ units.

struct ThreeObj { float4 r0; float4 r1; float4 r2; uint material; uint flags; uint color_size; uint streams; }; // streams: 1 uv, 2 instance colours
struct ThreeMat { float4 color; float4 emissive_rough; float4 params; float4 map_xform; uint4 misc; };
// params: metalness, model (1 basic, 2 lambert, 3 standard, 4 phong), alphaTest, has_map; misc.x: flipY
struct ThreeLight { uint kind; uint cast; packed_float3 color; packed_float3 pos; packed_float3 dir; float distance; float decay; packed_float3 extra; };

constant uint T_INSTANCED = 32u;
constant uint T_VCOLOR = 64u;
constant uint T_RECEIVE = 2u;

struct TOut {
    float4 pos [[position]];
    float3 wpos;
    float3 nrm;
    float2 uv;
    half4 vcol;
    float view_depth;
    uint mat [[flat]];
    uint flags [[flat]];
};

static float3 rows_mul(float4 r0, float4 r1, float4 r2, float4 p) { return float3(dot(r0, p), dot(r1, p), dot(r2, p)); }

// normal transform by the cofactor matrix (= det * inverse-transpose): right for any affine map,
// including shear from non-uniformly scaled parents; sign(det) keeps mirrored objects facing out
static float3 rows_normal(float4 r0, float4 r1, float4 r2, float3 n) {
    float3 c0 = float3(r0.x, r1.x, r2.x), c1 = float3(r0.y, r1.y, r2.y), c2 = float3(r0.z, r1.z, r2.z);
    float3 cn = cross(c1, c2) * n.x + cross(c2, c0) * n.y + cross(c0, c1) * n.z;
    return dot(c0, cross(c1, c2)) < 0.0 ? -cn : cn;
}

static float3 three_world(ThreeObj o, device const float4* inst, uint iid, float3 p, thread float3& n) {
    float4 p4 = float4(p, 1.0);
    if (o.flags & T_INSTANCED) {
        float4 i0 = inst[iid * 3], i1 = inst[iid * 3 + 1], i2 = inst[iid * 3 + 2];
        p4 = float4(rows_mul(i0, i1, i2, p4), 1.0);
        n = rows_normal(i0, i1, i2, n);
    }
    n = rows_normal(o.r0, o.r1, o.r2, n);
    return rows_mul(o.r0, o.r1, o.r2, p4);
}

vertex TOut vs_three(uint vid [[vertex_id]], uint iid [[instance_id]],
                     device const packed_float3* pos [[buffer(0)]],
                     device const packed_float3* nrm [[buffer(1)]],
                     device const float2* uv [[buffer(2)]],
                     constant Frame& f [[buffer(3)]],
                     device const float* col [[buffer(4)]],
                     constant ThreeObj& o [[buffer(5)]],
                     device const float4* inst [[buffer(6)]],
                     device const packed_float3* icol [[buffer(7)]]) {
    float3 n = float3(nrm[vid]);
    float3 w = three_world(o, inst, iid, float3(pos[vid]), n);
    TOut out;
    out.pos = f.view_proj * float4(w, 1.0);
    out.wpos = w;
    out.nrm = n;
    out.uv = (o.streams & 1u) ? uv[vid] : float2(0.0);
    half4 c = half4(1.0h);
    if (o.flags & T_VCOLOR) {
        uint k = o.color_size;
        c = half4(half(col[vid * k]), half(col[vid * k + 1]), half(col[vid * k + 2]), k > 3 ? half(col[vid * k + 3]) : 1.0h);
    }
    if ((o.flags & T_INSTANCED) && (o.streams & 2u)) c.rgb *= half3(float3(icol[iid]));
    out.vcol = c;
    out.view_depth = out.pos.w;
    out.mat = o.material;
    out.flags = o.flags;
    return out;
}

vertex ShadowOut vs_three_shadow(uint vid [[vertex_id]], uint iid [[instance_id]],
                                 device const packed_float3* pos [[buffer(0)]],
                                 constant Frame& f [[buffer(3)]],
                                 constant ThreeObj& o [[buffer(5)]],
                                 device const float4* inst [[buffer(6)]]) {
    float3 n = float3(0.0, 1.0, 0.0);
    ShadowOut out;
    out.pos = f.shadow_vp * float4(three_world(o, inst, iid, float3(pos[vid]), n), 1.0);
    return out;
}

// three.js's own programs (protocol v3): the native depth-only caster path of every shadow map (directional, spot,
// point-cube faces). Bound like three's translated depth program (src/xlat.rs): FrObject buffer(2) (filled with an
// identity view, so modelViewMatrix = modelMatrix), FrFrame buffer(3) (projectionMatrix = the shadow camera's
// view-projection, y-flipped so the map is stored as WebGL stores it), position at buffer(4), instanceMatrix columns
// at buffer(5). Same arithmetic as three's project_vertex + SPIRV-Cross's clip-space fixup.
struct FrObject3 { float4x4 model; float4x4 model_view; float4 normal[3]; };
struct FrFrame3 { float4x4 projection; float4x4 view; float4 camera; };
vertex ShadowOut vs_three_depth(uint vid [[vertex_id]], uint iid [[instance_id]],
                                device const packed_float3* pos [[buffer(4)]],
                                constant FrObject3& o [[buffer(2)]],
                                constant FrFrame3& f [[buffer(3)]],
                                device const float4x4* inst [[buffer(5)]],
                                constant uint& instanced [[buffer(6)]]) {
    float4 p = float4(float3(pos[vid]), 1.0);
    if (instanced != 0u) p = inst[iid] * p;
    ShadowOut out;
    out.pos = f.projection * (o.model_view * p);
    out.pos.z = (out.pos.z + out.pos.w) * 0.5;
    return out;
}

// three r170 and older pack shadow depth into RGBA8 (MeshDepthMaterial, RGBADepthPacking): packDepthToRGBA of
// gl_FragCoord.z (= 0.5 * z/w + 0.5 in GL; vs_three_depth's [[position]].z is exactly that), as packing.glsl.js r170
fragment float4 fs_three_pack_depth(ShadowOut in [[stage_in]]) {
    float v = in.pos.z;
    if (v <= 0.0) return float4(0.0);
    if (v >= 1.0) return float4(1.0);
    float vuf;
    float af = modf(v * 16777216.0, vuf);
    float bf = modf(vuf * (1.0 / 256.0), vuf);
    float gf = modf(vuf * (1.0 / 256.0), vuf);
    return float4(vuf * (1.0 / 255.0), gf * (256.0 / 255.0), bf * (256.0 / 255.0), af);
}

static float pow2(float x) { return x * x; }

// three.js r170 tone mapping (tonemapping_pars_fragment) and sRGB OETF, so native output matches
// the browser pixel for pixel modulo lighting differences
static float3 three_aces(float3 c, float exposure) {
    const float3x3 inm = float3x3(float3(0.59719, 0.07600, 0.02840), float3(0.35458, 0.90834, 0.13383), float3(0.04823, 0.01566, 0.83777));
    const float3x3 outm = float3x3(float3(1.60475, -0.10208, -0.00327), float3(-0.53108, 1.10813, -0.07276), float3(-0.07367, -0.00605, 1.07602));
    c *= exposure / 0.6;
    c = inm * c;
    float3 a = c * (c + 0.0245786) - 0.000090537, b = c * (0.983729 * c + 0.4329510) + 0.238081;
    return saturate(outm * (a / b));
}

static float3 three_agx(float3 c, float exposure) {
    const float3x3 srgb_to_2020 = float3x3(float3(0.6274, 0.0691, 0.0164), float3(0.3293, 0.9195, 0.0880), float3(0.0433, 0.0113, 0.8956));
    const float3x3 r2020_to_srgb = float3x3(float3(1.6605, -0.1246, -0.0182), float3(-0.5876, 1.1329, -0.1006), float3(-0.0728, -0.0083, 1.1187));
    const float3x3 inset = float3x3(float3(0.856627153315983, 0.137318972929847, 0.11189821299995),
                                    float3(0.0951212405381588, 0.761241990602591, 0.0767994186031903),
                                    float3(0.0482516061458583, 0.101439036467562, 0.811302368396859));
    const float3x3 outset = float3x3(float3(1.1271005818144368, -0.1413297634984383, -0.14132976349843826),
                                     float3(-0.11060664309660323, 1.157823702216272, -0.11060664309660294),
                                     float3(-0.016493938717834573, -0.016493938717834257, 1.2519364065950405));
    const float lo = -12.47393, hi = 4.026069;
    c *= exposure;
    c = inset * (srgb_to_2020 * c);
    c = saturate((log2(max(c, 1e-10)) - lo) / (hi - lo));
    float3 x2 = c * c, x4 = x2 * x2;
    c = 15.5 * x4 * x2 - 40.14 * x4 * c + 31.96 * x4 - 6.868 * x2 * c + 0.4298 * x2 + 0.1191 * c - 0.00232;
    c = pow(max(outset * c, 0.0), 2.2);
    return saturate(r2020_to_srgb * c);
}

static float3 three_srgb(float3 c) { return select(pow(c, 0.41666) * 1.055 - 0.055, c * 12.92, c <= 0.0031308); }

static half3 three_output(float3 c, uint mode, float exposure) {
    if (mode == 6u) c = three_agx(c, exposure);
    else if (mode == 0u) c = saturate(c);
    else if (mode == 1u) c = saturate(c * exposure);
    else c = three_aces(c, exposure);
    return half3(three_srgb(c));
}

// GGX specular (three.js BRDF_GGX: Schlick Fresnel, height-correlated Smith visibility, GGX D)
static float3 brdf_ggx(float3 L, float3 V, float3 N, float3 spec, float rough) {
    float a = max(rough * rough, 0.0025);
    float3 H = normalize(L + V);
    float dNL = saturate(dot(N, L)), dNV = saturate(dot(N, V)) + 1e-4, dNH = saturate(dot(N, H)), dVH = saturate(dot(V, H));
    float3 F = spec + (1.0 - spec) * pow(1.0 - dVH, 5.0);
    float a2 = a * a;
    float gv = dNL * sqrt(a2 + (1.0 - a2) * dNV * dNV), gl = dNV * sqrt(a2 + (1.0 - a2) * dNL * dNL);
    float Vis = 0.5 / max(gv + gl, 1e-6);
    float D = a2 / (M_PI_F * pow2(dNH * dNH * (a2 - 1.0) + 1.0));
    return F * (Vis * D);
}

fragment half4 fs_three(TOut in [[stage_in]], bool front [[front_facing]],
                        constant Frame& f [[buffer(0)]],
                        constant ThreeLight* lights [[buffer(1)]],
                        device const ThreeMat* mats [[buffer(2)]],
                        depth2d<float> sm [[texture(0)]],
                        texture2d<half> map [[texture(1)]],
                        sampler map_s [[sampler(0)]]) {
    ThreeMat m = mats[in.mat];
    float4 base = m.color;
    if (m.params.w > 0.5) {
        float2 uv = in.uv * m.map_xform.xy + m.map_xform.zw;
        if (m.misc.x != 0u) uv.y = 1.0 - uv.y;
        base *= float4(map.sample(map_s, uv)); // the texture's own three.js filter/wrap/anisotropy
    }
    base *= float4(in.vcol);
    if (m.params.z > 0.0 && base.a < m.params.z) discard_fragment();
    float3 color;
    uint model = uint(m.params.y);
    if (model == 1u) {
        color = base.rgb; // MeshBasicMaterial: unlit
    } else {
        float3 N = normalize(in.nrm) * (front ? 1.0 : -1.0);
        float3 V = normalize(f.cam_pos.xyz - in.wpos);
        float metal = model == 3u ? m.params.x : 0.0;
        float rough = m.emissive_rough.w;
        float3 diffuse = base.rgb * (1.0 - metal);
        float3 spec = mix(float3(0.04), base.rgb, metal);
        bool specular = model != 2u;
        float3 direct = 0.0, irr = 0.0;
        for (uint i = 0; i < f.counts.y; i++) {
            ThreeLight l = lights[i];
            float3 lc = float3(l.color);
            if (l.kind == 1u) { irr += lc; continue; }
            if (l.kind == 2u) { irr += mix(float3(l.extra), lc, N.y * 0.5 + 0.5); continue; }
            float3 L;
            float att = 1.0;
            if (l.kind == 3u) {
                L = -float3(l.dir);
                if (l.cast != 0u && (in.flags & T_RECEIVE)) att = float(sun_shadow(in.wpos, sm, f));
            } else {
                float3 d = float3(l.pos) - in.wpos;
                float dist = length(d);
                L = d / max(dist, 1e-4);
                att = 1.0 / max(pow(dist, l.decay), 0.01);
                if (l.distance > 0.0) att *= pow2(saturate(1.0 - pow2(pow2(dist / l.distance))));
                if (l.kind == 5u) {
                    float cosA = cos(l.extra[0]), cosP = cos(l.extra[0] * (1.0 - l.extra[1]));
                    att *= smoothstep(cosA, cosP, dot(-L, float3(l.dir)));
                }
            }
            float nl = saturate(dot(N, L));
            if (nl <= 0.0 || att <= 0.0) continue;
            float3 rad = lc * att * nl;
            direct += rad * diffuse / M_PI_F;
            if (specular) direct += rad * brdf_ggx(L, V, N, spec, rough);
        }
        color = direct + irr * diffuse / M_PI_F;
    }
    color += m.emissive_rough.xyz;
    float3 outc = float3(three_output(max(color, 0.0), uint(f.fog2.w), f.cam_pos.w));
    // fog, in three's order: tonemapping_fragment -> colorspace_fragment -> fog_fragment, i.e. mixed in
    // output space; three converts the fog colour to the output colour space (getUnlitUniformColorSpace).
    // Depth = view-space distance along the camera axis.
    uint fm = uint(f.fog2.z);
    float3 fogc = three_srgb(saturate(f.fog.rgb));
    if (fm == 1u) outc = mix(outc, fogc, smoothstep(f.fog2.x, f.fog2.y, in.view_depth));
    else if (fm == 2u) outc = mix(outc, fogc, 1.0 - exp(-pow2(f.fog2.x * in.view_depth)));
    return half4(half3(outc), half(base.a));
}
