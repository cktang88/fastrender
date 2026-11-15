#version 300 es
#extension GL_EXT_clip_cull_distance : require
#define USE_FOO
precision highp float;
	precision mediump int ;
precision highp sampler2D;
#ifdef USE_FOO
	uniform mat3 normalMatrix;
#endif
// a comment line is skipped when looking for the insertion point

uniform highp mat4 modelMatrix;
uniform   mat4   modelViewMatrix ;
uniform mat4 projectionMatrix;
uniform mat3 modelMatrixInverse;
uniform mat4 normalMatrix2;
uniform mat3 modelViewMatrix;
uniform mat4 modelMatrix; // trailing comments keep a line as is
float precisionSafe( float x ) { return x; } precision highp float;
void main() {
	gl_Position = projectionMatrix * modelViewMatrix * modelMatrix * vec4( normalMatrix * vec3( 0.0 ), 1.0 );
}
uniform mat4 modelViewMatrix, projectionMatrix, customMatrix; // RawShaderMaterial style
uniform highp vec3 cameraPosition;// eye
uniform mat4 viewMatrix , modelMatrix ;
uniform float a, viewMatrix;
uniform mat4 viewMatrix[2];
uniform vec3 cameraPosition = vec3(0.0);
