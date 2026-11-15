#version 460
#define USE_BATCHING



#ifdef USE_BATCHING
	#if ! defined( GL_ANGLE_multi_draw )
	#define gl_DrawID _gl_DrawID
#define _gl_DrawID gl_BaseInstance
	#endif
	uniform highp sampler2D batchingTexture;
#endif
layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; }; layout(std140) uniform FrFrame { mat4 projectionMatrix; mat4 viewMatrix; vec3 cameraPosition; };
in vec3 position;
void main() {
	float id = float( gl_DrawID );
	gl_Position = projectionMatrix * modelViewMatrix * vec4( position + id, 1.0 );
}
