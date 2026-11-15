#version 300 es
#define USE_BATCHING
precision highp float;
uniform mat4 modelViewMatrix;
uniform mat4 projectionMatrix;
#ifdef USE_BATCHING
	#if ! defined( GL_ANGLE_multi_draw )
	#define gl_DrawID _gl_DrawID
	uniform int _gl_DrawID;
	#endif
	uniform highp sampler2D batchingTexture;
#endif
in vec3 position;
void main() {
	float id = float( gl_DrawID );
	gl_Position = projectionMatrix * modelViewMatrix * vec4( position + id, 1.0 );
}
