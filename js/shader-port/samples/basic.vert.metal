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
};

struct main0_out
{
    float vFogDepth [[user(locn0)]];
    float4 gl_Position [[position]];
};

struct main0_in
{
    float3 position [[attribute(0)]];
};

vertex main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _27 [[buffer(0)]])
{
    main0_out out = {};
    float3 transformed = in.position;
    float4 mvPosition = float4(transformed, 1.0);
    mvPosition = _27.modelViewMatrix * mvPosition;
    out.gl_Position = _27.projectionMatrix * mvPosition;
    out.vFogDepth = -mvPosition.z;
    return out;
}

