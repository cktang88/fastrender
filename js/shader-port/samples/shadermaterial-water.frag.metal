#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock
{
    float4x4 viewMatrix;
    packed_float3 cameraPosition;
    uint isOrthographic;
    float3 uColor;
};

struct main0_out
{
    float4 pc_fragColor [[color(0)]];
};

struct main0_in
{
    float2 vUv [[user(locn0)]];
    float vWave [[user(locn1)]];
};

fragment main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _15 [[buffer(0)]], texture2d<float> uMap [[texture(0)]], sampler uMapSmplr [[sampler(0)]])
{
    main0_out out = {};
    out.pc_fragColor = float4((_15.uColor * (0.800000011920928955078125 + in.vWave)) * uMap.sample(uMapSmplr, in.vUv).xyz, 1.0);
    return out;
}

