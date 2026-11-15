#version 450
#define SHADER_NAME painted_metal__414234
  # define  SHADER_NAME Glass__lobby__(d_rer)__
#define SHADER_NAME MeshStandardMaterial
#define SHADER_NAME_X a#b
#define texture2D texture
#define F(fr_kw_sampler) (fr_kw_sampler + 1.0)

layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };
uniform float fr_kw_buffer; // a ShaderMaterial uniform named like a Vulkan keyword
uniform mat4 fr_kw_precise;
struct Shared { float fr_kw_shared; vec2 fr_kw_samplerShadow; };
uniform Shared u_shared;
/* a block comment: sampler buffer
   still the comment: precise shared */ float after_comment_sampler = 1.0; float sampler_ok = 2.0;
vec4 textureBicubic( sampler2D fr_kw_sampler, vec2 uv, float lod ) {
	// sampler in a line comment stays
	return texture2D( fr_kw_sampler, uv ) + vec4( u_shared.fr_kw_shared + fr_kw_buffer + 1e5 + F( 2.0 ) ); /* sampler */ float precise_2 = 0.0;
}
void main() {
	float fr_kw_texture3D = 1.0, xsampler = 2.0, sampler1 = 3.0, Sampler = 4.0;
	gl_Position = vec4( fr_kw_texture3D + xsampler + sampler1 + Sampler + fr_kw_precise );
}
