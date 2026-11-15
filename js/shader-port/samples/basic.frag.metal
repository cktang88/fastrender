#pragma clang diagnostic ignored "-Wmissing-prototypes"

#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock
{
    float4x4 viewMatrix;
    packed_float3 cameraPosition;
    uint isOrthographic;
    packed_float3 diffuse;
    float opacity;
    packed_float3 fogColor;
    float fogNear;
    float fogFar;
};

struct ReflectedLight
{
    float3 directDiffuse;
    float3 directSpecular;
    float3 indirectDiffuse;
    float3 indirectSpecular;
};

struct main0_out
{
    float4 pc_fragColor [[color(0)]];
};

struct main0_in
{
    float vFogDepth [[user(locn1)]];
};

static inline __attribute__((always_inline))
float4 sRGBTransferOETF(thread const float4& value)
{
    return float4(mix((pow(value.xyz, float3(0.416660010814666748046875)) * 1.05499994754791259765625) - float3(0.054999999701976776123046875), value.xyz * 12.9200000762939453125, float3(value.xyz <= float3(0.003130800090730190277099609375))), value.w);
}

static inline __attribute__((always_inline))
float4 linearToOutputTexel(thread const float4& value)
{
    float4 param = float4(value.xyz * float3x3(float3(1.0, -0.0, -0.0), float3(-0.0, 1.0, 0.0), float3(0.0, 0.0, 1.0)), value.w);
    return sRGBTransferOETF(param);
}

fragment main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _78 [[buffer(0)]])
{
    main0_out out = {};
    float4 diffuseColor = float4(_78.diffuse[0], _78.diffuse[1], _78.diffuse[2], _78.opacity);
    float specularStrength = 1.0;
    ReflectedLight reflectedLight = ReflectedLight{ float3(0.0), float3(0.0), float3(0.0), float3(0.0) };
    reflectedLight.indirectDiffuse += float3(1.0);
    reflectedLight.indirectDiffuse *= diffuseColor.xyz;
    float3 outgoingLight = reflectedLight.indirectDiffuse;
    diffuseColor.w = 1.0;
    out.pc_fragColor = float4(outgoingLight, diffuseColor.w);
    float4 param = out.pc_fragColor;
    out.pc_fragColor = linearToOutputTexel(param);
    float fogFactor = smoothstep(_78.fogNear, _78.fogFar, in.vFogDepth);
    float4 _136 = out.pc_fragColor;
    float3 _143 = mix(_136.xyz, float3(_78.fogColor), float3(fogFactor));
    out.pc_fragColor.x = _143.x;
    out.pc_fragColor.y = _143.y;
    out.pc_fragColor.z = _143.z;
    return out;
}

