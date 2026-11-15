#pragma clang diagnostic ignored "-Wmissing-prototypes"

#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

struct DirectionalLight
{
    float3 direction;
    float3 color;
};

struct IncidentLight
{
    float3 color;
    float3 direction;
    short visible;
};

struct LambertMaterial
{
    float3 diffuseColor;
    float specularStrength;
};

struct ReflectedLight
{
    float3 directDiffuse;
    float3 directSpecular;
    float3 indirectDiffuse;
    float3 indirectSpecular;
};

struct DirectionalLight_1
{
    float3 direction;
    float3 color;
};

struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock
{
    float4x4 viewMatrix;
    packed_float3 cameraPosition;
    uint isOrthographic;
    float3 diffuse;
    packed_float3 emissive;
    float opacity;
    packed_float3 fogColor;
    float fogNear;
    float fogFar;
    uint receiveShadow;
    float3 ambientLightColor;
    DirectionalLight_1 directionalLights[1];
};

struct main0_out
{
    float4 pc_fragColor [[color(0)]];
};

struct main0_in
{
    float vFogDepth [[user(locn0)]];
    float3 vNormal [[user(locn1)]];
    float3 vViewPosition [[user(locn2)]];
};

static inline __attribute__((always_inline))
void getDirectionalLightInfo(DirectionalLight directionalLight, thread IncidentLight& light)
{
    light.color = directionalLight.color;
    light.direction = directionalLight.direction;
    light.visible = short(true);
}

static inline __attribute__((always_inline))
float3 BRDF_Lambert(float3 diffuseColor)
{
    return diffuseColor * 0.3183098733425140380859375;
}

static inline __attribute__((always_inline))
void RE_Direct_Lambert(IncidentLight directLight, float3 geometryPosition, float3 geometryNormal, float3 geometryViewDir, float3 geometryClearcoatNormal, LambertMaterial material, thread ReflectedLight& reflectedLight)
{
    float dotNL = fast::clamp(dot(geometryNormal, directLight.direction), 0.0, 1.0);
    float3 irradiance = directLight.color * dotNL;
    reflectedLight.directDiffuse += (irradiance * BRDF_Lambert(material.diffuseColor));
}

static inline __attribute__((always_inline))
float3 getAmbientLightIrradiance(float3 ambientLightColor)
{
    float3 irradiance = ambientLightColor;
    return irradiance;
}

static inline __attribute__((always_inline))
void RE_IndirectDiffuse_Lambert(float3 irradiance, float3 geometryPosition, float3 geometryNormal, float3 geometryViewDir, float3 geometryClearcoatNormal, LambertMaterial material, thread ReflectedLight& reflectedLight)
{
    reflectedLight.indirectDiffuse += (irradiance * BRDF_Lambert(material.diffuseColor));
}

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

fragment main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _162 [[buffer(0)]], bool gl_FrontFacing [[front_facing]])
{
    main0_out out = {};
    float4 diffuseColor = float4(_162.diffuse, _162.opacity);
    ReflectedLight reflectedLight = ReflectedLight{ float3(0.0), float3(0.0), float3(0.0), float3(0.0) };
    float3 totalEmissiveRadiance = float3(_162.emissive);
    float specularStrength = 1.0;
    float faceDirection = gl_FrontFacing ? 1.0 : (-1.0);
    float3 normal = fast::normalize(in.vNormal);
    float3 nonPerturbedNormal = normal;
    LambertMaterial material;
    material.diffuseColor = diffuseColor.xyz;
    material.specularStrength = specularStrength;
    float3 geometryPosition = -in.vViewPosition;
    float3 geometryNormal = normal;
    float3 _214;
    if (_162.isOrthographic != 0u)
    {
        _214 = float3(0.0, 0.0, 1.0);
    }
    else
    {
        _214 = fast::normalize(in.vViewPosition);
    }
    float3 geometryViewDir = _214;
    float3 geometryClearcoatNormal = float3(0.0);
    DirectionalLight directionalLight;
    directionalLight.direction = _162.directionalLights[0].direction;
    directionalLight.color = _162.directionalLights[0].color;
    IncidentLight param;
    getDirectionalLightInfo(directionalLight, param);
    IncidentLight directLight = param;
    ReflectedLight param_1 = reflectedLight;
    RE_Direct_Lambert(directLight, geometryPosition, geometryNormal, geometryViewDir, geometryClearcoatNormal, material, param_1);
    reflectedLight = param_1;
    float3 iblIrradiance = float3(0.0);
    float3 irradiance = getAmbientLightIrradiance(_162.ambientLightColor);
    ReflectedLight param_2 = reflectedLight;
    RE_IndirectDiffuse_Lambert(irradiance, geometryPosition, geometryNormal, geometryViewDir, geometryClearcoatNormal, material, param_2);
    reflectedLight = param_2;
    float3 outgoingLight = (reflectedLight.directDiffuse + reflectedLight.indirectDiffuse) + totalEmissiveRadiance;
    diffuseColor.w = 1.0;
    out.pc_fragColor = float4(outgoingLight, diffuseColor.w);
    float4 param_3 = out.pc_fragColor;
    out.pc_fragColor = linearToOutputTexel(param_3);
    float fogFactor = smoothstep(_162.fogNear, _162.fogFar, in.vFogDepth);
    float4 _295 = out.pc_fragColor;
    float3 _302 = mix(_295.xyz, float3(_162.fogColor), float3(fogFactor));
    out.pc_fragColor.x = _302.x;
    out.pc_fragColor.y = _302.y;
    out.pc_fragColor.z = _302.z;
    return out;
}

