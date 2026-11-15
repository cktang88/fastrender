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

struct PhysicalMaterial
{
    float3 diffuseColor;
    float roughness;
    float3 specularColor;
    float specularF90;
    float dispersion;
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
    float uGrime;
    float3 diffuse;
    packed_float3 emissive;
    float roughness;
    float metalness;
    float opacity;
    char _m9_pad[8];
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
    float3 vViewPosition [[user(locn0)]];
    float vFogDepth [[user(locn1)]];
    float3 vNormal [[user(locn2)]];
};

static inline __attribute__((always_inline))
void getDirectionalLightInfo(DirectionalLight directionalLight, thread IncidentLight& light)
{
    light.color = directionalLight.color;
    light.direction = directionalLight.direction;
    light.visible = short(true);
}

static inline __attribute__((always_inline))
float pow2(float x)
{
    return x * x;
}

static inline __attribute__((always_inline))
float3 F_Schlick(float3 f0, float f90, float dotVH)
{
    float fresnel = exp2((((-5.554729938507080078125) * dotVH) - 6.9831600189208984375) * dotVH);
    return (f0 * (1.0 - fresnel)) + float3(f90 * fresnel);
}

static inline __attribute__((always_inline))
float V_GGX_SmithCorrelated(float alpha, float dotNL, float dotNV)
{
    float a2 = pow2(alpha);
    float gv = dotNL * sqrt(a2 + ((1.0 - a2) * pow2(dotNV)));
    float gl = dotNV * sqrt(a2 + ((1.0 - a2) * pow2(dotNL)));
    return 0.5 / fast::max(gv + gl, 9.9999999747524270787835121154785e-07);
}

static inline __attribute__((always_inline))
float D_GGX(float alpha, float dotNH)
{
    float a2 = pow2(alpha);
    float denom = (pow2(dotNH) * (a2 - 1.0)) + 1.0;
    return (0.3183098733425140380859375 * a2) / pow2(denom);
}

static inline __attribute__((always_inline))
float3 BRDF_GGX(float3 lightDir, float3 viewDir, float3 normal, PhysicalMaterial material)
{
    float3 f0 = material.specularColor;
    float f90 = material.specularF90;
    float roughness = material.roughness;
    float alpha = pow2(roughness);
    float3 halfDir = fast::normalize(lightDir + viewDir);
    float dotNL = fast::clamp(dot(normal, lightDir), 0.0, 1.0);
    float dotNV = fast::clamp(dot(normal, viewDir), 0.0, 1.0);
    float dotNH = fast::clamp(dot(normal, halfDir), 0.0, 1.0);
    float dotVH = fast::clamp(dot(viewDir, halfDir), 0.0, 1.0);
    float3 F = F_Schlick(f0, f90, dotVH);
    float V = V_GGX_SmithCorrelated(alpha, dotNL, dotNV);
    float D = D_GGX(alpha, dotNH);
    return F * (V * D);
}

static inline __attribute__((always_inline))
float3 BRDF_Lambert(float3 diffuseColor)
{
    return diffuseColor * 0.3183098733425140380859375;
}

static inline __attribute__((always_inline))
void RE_Direct_Physical(IncidentLight directLight, float3 geometryPosition, float3 geometryNormal, float3 geometryViewDir, float3 geometryClearcoatNormal, PhysicalMaterial material, thread ReflectedLight& reflectedLight)
{
    float dotNL = fast::clamp(dot(geometryNormal, directLight.direction), 0.0, 1.0);
    float3 irradiance = directLight.color * dotNL;
    reflectedLight.directSpecular += (irradiance * BRDF_GGX(directLight.direction, geometryViewDir, geometryNormal, material));
    reflectedLight.directDiffuse += (irradiance * BRDF_Lambert(material.diffuseColor));
}

static inline __attribute__((always_inline))
float3 getAmbientLightIrradiance(float3 ambientLightColor)
{
    float3 irradiance = ambientLightColor;
    return irradiance;
}

static inline __attribute__((always_inline))
void RE_IndirectDiffuse_Physical(float3 irradiance, float3 geometryPosition, float3 geometryNormal, float3 geometryViewDir, float3 geometryClearcoatNormal, PhysicalMaterial material, thread ReflectedLight& reflectedLight)
{
    reflectedLight.indirectDiffuse += (irradiance * BRDF_Lambert(material.diffuseColor));
}

static inline __attribute__((always_inline))
float2 DFGApprox(float3 normal, float3 viewDir, float roughness)
{
    float dotNV = fast::clamp(dot(normal, viewDir), 0.0, 1.0);
    float4 r = (float4(-1.0, -0.0274999998509883880615234375, -0.572000026702880859375, 0.02199999988079071044921875) * roughness) + float4(1.0, 0.0425000004470348358154296875, 1.03999996185302734375, -0.039999999105930328369140625);
    float a004 = (fast::min(r.x * r.x, exp2((-9.27999973297119140625) * dotNV)) * r.x) + r.y;
    float2 fab = (float2(-1.03999996185302734375, 1.03999996185302734375) * a004) + r.zw;
    return fab;
}

static inline __attribute__((always_inline))
void computeMultiscattering(float3 normal, float3 viewDir, float3 specularColor, float specularF90, float roughness, thread float3& singleScatter, thread float3& multiScatter)
{
    float2 fab = DFGApprox(normal, viewDir, roughness);
    float3 Fr = specularColor;
    float3 FssEss = (Fr * fab.x) + float3(specularF90 * fab.y);
    float Ess = fab.x + fab.y;
    float Ems = 1.0 - Ess;
    float3 Favg = Fr + ((float3(1.0) - Fr) * 0.04761900007724761962890625);
    float3 Fms = (FssEss * Favg) / (float3(1.0) - (Favg * Ems));
    singleScatter += FssEss;
    multiScatter += (Fms * Ems);
}

static inline __attribute__((always_inline))
void RE_IndirectSpecular_Physical(float3 radiance, float3 irradiance, float3 clearcoatRadiance, float3 geometryPosition, float3 geometryNormal, float3 geometryViewDir, float3 geometryClearcoatNormal, PhysicalMaterial material, thread ReflectedLight& reflectedLight)
{
    float3 singleScattering = float3(0.0);
    float3 multiScattering = float3(0.0);
    float3 cosineWeightedIrradiance = irradiance * 0.3183098733425140380859375;
    float3 param = singleScattering;
    float3 param_1 = multiScattering;
    computeMultiscattering(geometryNormal, geometryViewDir, material.specularColor, material.specularF90, material.roughness, param, param_1);
    singleScattering = param;
    multiScattering = param_1;
    float3 totalScattering = singleScattering + multiScattering;
    float3 diffuse = material.diffuseColor * (1.0 - fast::max(fast::max(totalScattering.x, totalScattering.y), totalScattering.z));
    reflectedLight.indirectSpecular += (radiance * singleScattering);
    reflectedLight.indirectSpecular += (multiScattering * cosineWeightedIrradiance);
    reflectedLight.indirectDiffuse += (diffuse * cosineWeightedIrradiance);
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

fragment main0_out main0(main0_in in [[stage_in]], constant _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock& _487 [[buffer(0)]], bool gl_FrontFacing [[front_facing]])
{
    main0_out out = {};
    float3 clearcoatSpecularDirect = float3(0.0);
    float3 clearcoatSpecularIndirect = float3(0.0);
    float3 sheenSpecularDirect = float3(0.0);
    float3 sheenSpecularIndirect = float3(0.0);
    float4 diffuseColor = float4(_487.diffuse, _487.opacity);
    ReflectedLight reflectedLight = ReflectedLight{ float3(0.0), float3(0.0), float3(0.0), float3(0.0) };
    float3 totalEmissiveRadiance = float3(_487.emissive);
    float4 _519 = diffuseColor;
    float3 _521 = _519.xyz * mix(1.0, 0.60000002384185791015625, _487.uGrime * smoothstep(0.0, 2.0, in.vViewPosition.y + 1.0));
    diffuseColor.x = _521.x;
    diffuseColor.y = _521.y;
    diffuseColor.z = _521.z;
    float roughnessFactor = _487.roughness;
    float metalnessFactor = _487.metalness;
    float faceDirection = gl_FrontFacing ? 1.0 : (-1.0);
    float3 normal = fast::normalize(in.vNormal);
    float3 nonPerturbedNormal = normal;
    PhysicalMaterial material;
    material.diffuseColor = diffuseColor.xyz * (1.0 - metalnessFactor);
    float3 dxy = fast::max(abs(dfdx(nonPerturbedNormal)), abs(dfdy(nonPerturbedNormal)));
    float geometryRoughness = fast::max(fast::max(dxy.x, dxy.y), dxy.z);
    material.roughness = fast::max(roughnessFactor, 0.0524999983608722686767578125);
    material.roughness += geometryRoughness;
    material.roughness = fast::min(material.roughness, 1.0);
    material.specularColor = mix(float3(0.039999999105930328369140625), diffuseColor.xyz, float3(metalnessFactor));
    material.specularF90 = 1.0;
    float3 geometryPosition = -in.vViewPosition;
    float3 geometryNormal = normal;
    float3 _604;
    if (_487.isOrthographic != 0u)
    {
        _604 = float3(0.0, 0.0, 1.0);
    }
    else
    {
        _604 = fast::normalize(in.vViewPosition);
    }
    float3 geometryViewDir = _604;
    float3 geometryClearcoatNormal = float3(0.0);
    DirectionalLight directionalLight;
    directionalLight.direction = _487.directionalLights[0].direction;
    directionalLight.color = _487.directionalLights[0].color;
    IncidentLight param;
    getDirectionalLightInfo(directionalLight, param);
    IncidentLight directLight = param;
    ReflectedLight param_1 = reflectedLight;
    RE_Direct_Physical(directLight, geometryPosition, geometryNormal, geometryViewDir, geometryClearcoatNormal, material, param_1);
    reflectedLight = param_1;
    float3 iblIrradiance = float3(0.0);
    float3 irradiance = getAmbientLightIrradiance(_487.ambientLightColor);
    float3 radiance = float3(0.0);
    float3 clearcoatRadiance = float3(0.0);
    ReflectedLight param_2 = reflectedLight;
    RE_IndirectDiffuse_Physical(irradiance, geometryPosition, geometryNormal, geometryViewDir, geometryClearcoatNormal, material, param_2);
    reflectedLight = param_2;
    ReflectedLight param_3 = reflectedLight;
    RE_IndirectSpecular_Physical(radiance, iblIrradiance, clearcoatRadiance, geometryPosition, geometryNormal, geometryViewDir, geometryClearcoatNormal, material, param_3);
    reflectedLight = param_3;
    float3 totalDiffuse = reflectedLight.directDiffuse + reflectedLight.indirectDiffuse;
    float3 totalSpecular = reflectedLight.directSpecular + reflectedLight.indirectSpecular;
    float3 outgoingLight = (totalDiffuse + totalSpecular) + totalEmissiveRadiance;
    diffuseColor.w = 1.0;
    out.pc_fragColor = float4(outgoingLight, diffuseColor.w);
    float4 param_4 = out.pc_fragColor;
    out.pc_fragColor = linearToOutputTexel(param_4);
    float fogFactor = smoothstep(_487.fogNear, _487.fogFar, in.vFogDepth);
    float4 _708 = out.pc_fragColor;
    float3 _715 = mix(_708.xyz, float3(_487.fogColor), float3(fogFactor));
    out.pc_fragColor.x = _715.x;
    out.pc_fragColor.y = _715.y;
    out.pc_fragColor.z = _715.z;
    return out;
}

