#version 450
#define varying in
layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; }; layout(std140) uniform FrFrame { mat4 projectionMatrix; mat4 viewMatrix; vec3 cameraPosition; };
layout(location = 0) out highp vec4 pc_fragColor;
#define gl_FragColor pc_fragColor
#define gl_FragDepthEXT gl_FragDepth
#define texture2D texture
#define textureCube texture
#define texture2DProj textureProj
#define texture2DLodEXT textureLod
#define texture2DProjLodEXT textureProjLod
#define textureCubeLodEXT textureLod
#define texture2DGradEXT textureGrad
#define texture2DProjGradEXT textureProjGrad
#define textureCubeGradEXT textureGrad

















	
#define HIGH_PRECISION
#define SHADER_TYPE ShaderMaterial
#define SHADER_NAME shadermaterial-layout-probe
#define USE_SHADOWMAP
#define SHADOWMAP_TYPE_PCF


uniform bool isOrthographic;
#define OPAQUE
vec4 LinearTransferOETF( in vec4 value ) {
	return value;
}
vec4 sRGBTransferEOTF( in vec4 value ) {
	return vec4( mix( pow( value.rgb * 0.9478672986 + vec3( 0.0521327014 ), vec3( 2.4 ) ), value.rgb * 0.0773993808, vec3( lessThanEqual( value.rgb, vec3( 0.04045 ) ) ) ), value.a );
}
vec4 sRGBTransferOETF( in vec4 value ) {
	return vec4( mix( pow( value.rgb, vec3( 0.41666 ) ) * 1.055 - vec3( 0.055 ), value.rgb * 12.92, vec3( lessThanEqual( value.rgb, vec3( 0.0031308 ) ) ) ), value.a );
}
vec4 linearToOutputTexel( vec4 value ) {
	return sRGBTransferOETF( vec4( mat3( 1.0000,0.0000,-0.0000,-0.0000,1.0000,-0.0000,-0.0000,0.0000,1.0000 ) * value.rgb, value.a ) );
}
float luminance( const in vec3 rgb ) {
	const vec3 weights = vec3( 0.2126, 0.7152, 0.0722 );
	return dot( weights, rgb );
}

struct L { vec3 pos; float power; vec3 tint; };


uniform vec3 uA; uniform float uB; uniform vec2 uOffs[3]; uniform int uCount; uniform bool uOn; uniform L uLights[2];
uniform mat2 uM2; uniform ivec4 uIv; uniform sampler2D uMap; uniform float uWeights[5]; varying vec3 vN; varying vec2 vUv;
void main() { vec3 c = uA * uB + vec3(uOffs[uCount].x, uWeights[4], float(uIv.y));
  for (int i = 0; i < 2; i++) c += uLights[i].tint * uLights[i].power * max(dot(vN, normalize(uLights[i].pos)), 0.0);
  vec2 t = uM2 * vUv; c *= texture2D(uMap, t).rgb; c += (normalMatrix * (modelMatrix * vec4(vN, 0.0)).xyz) * 0.0;
  gl_FragColor = vec4(uOn ? c : vec3(0.0), 1.0); }
