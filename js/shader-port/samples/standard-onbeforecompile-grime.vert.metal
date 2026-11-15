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
    float3 vViewPosition [[user(locn0)]];
    float vFogDepth [[user(locn1)]];
    float3 vNormal [[user(locn2)]];
    float4 gl_Position [[position]];
};

struct main0_in
{
    float3 position [[attribute(0)]];
    float3 normal [[attribute(1)]];
};

vertex main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _21 [[buffer(0)]])
{
    main0_out out = {};
    float3 objectNormal = in.normal;
    float3 transformedNormal = objectNormal;
    transformedNormal = _21.normalMatrix * transformedNormal;
    out.vNormal = fast::normalize(transformedNormal);
    float3 transformed = in.position;
    float4 mvPosition = float4(transformed, 1.0);
    mvPosition = _21.modelViewMatrix * mvPosition;
    out.gl_Position = _21.projectionMatrix * mvPosition;
    out.vViewPosition = -mvPosition.xyz;
    out.vFogDepth = -mvPosition.z;
    return out;
}

