#version 450

#define USE_FOO



#ifdef USE_FOO

#endif
// a comment line is skipped when looking for the insertion point




layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; }; layout(std140) uniform FrFrame { mat4 projectionMatrix; mat4 viewMatrix; vec3 cameraPosition; };
uniform mat3 modelMatrixInverse;
uniform mat4 normalMatrix2;
uniform mat3 modelViewMatrix;

float precisionSafe( float x ) { return x; } precision highp float;
void main() {
	gl_Position = projectionMatrix * modelViewMatrix * modelMatrix * vec4( normalMatrix * vec3( 0.0 ), 1.0 );
}
uniform mat4 customMatrix;


uniform float a, viewMatrix;
uniform mat4 viewMatrix[2];
uniform vec3 cameraPosition = vec3(0.0);
