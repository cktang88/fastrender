#version 300 es
#define SHADER_NAME painted_metal #414234
  # define  SHADER_NAME  Glass "lobby" (dürer) \
#define SHADER_NAME MeshStandardMaterial
#define SHADER_NAME_X a#b
#define texture2D texture
#define F(sampler) (sampler + 1.0)
precision highp float;
uniform float buffer; // a ShaderMaterial uniform named like a Vulkan keyword
uniform mat4 modelMatrix, precise;
struct Shared { float shared; vec2 samplerShadow; };
uniform Shared u_shared;
/* a block comment: sampler buffer
   still the comment: precise shared */ float after_comment_sampler = 1.0; float sampler_ok = 2.0;
vec4 textureBicubic( sampler2D sampler, vec2 uv, float lod ) {
	// sampler in a line comment stays
	return texture2D( sampler, uv ) + vec4( u_shared.shared + buffer + 1e5 + F( 2.0 ) ); /* sampler */ float precise_2 = 0.0;
}
void main() {
	float texture3D = 1.0, xsampler = 2.0, sampler1 = 3.0, Sampler = 4.0;
	gl_Position = vec4( texture3D + xsampler + sampler1 + Sampler + precise );
}
