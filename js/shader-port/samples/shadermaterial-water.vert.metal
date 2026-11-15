#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock
{
    float4x4 modelMatrix;
    float4x4 modelViewMatrix;
    float4x4 projectionMatrix;
    float4x4 viewMatrix;
    float3x3 normalMatrix;
    packed_float3 cameraPosition;
    uint isOrthographic;
    float uTime;
};

struct main0_out
{
    float2 vUv [[user(locn0)]];
    float vWave [[user(locn1)]];
    float4 gl_Position [[position]];
};

struct main0_in
{
    float3 position [[attribute(0)]];
    float2 uv [[attribute(2)]];
};

vertex main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _33 [[buffer(0)]])
{
    main0_out out = {};
    out.vUv = in.uv;
    float3 p = in.position;
    out.vWave = sin((p.x * 3.0) + _33.uTime) * 0.100000001490116119384765625;
    p.z += out.vWave;
    out.gl_Position = (_33.projectionMatrix * _33.modelViewMatrix) * float4(p, 1.0);
    return out;
}

