#version 450

#define USE_FOO



#ifdef USE_FOO

#endif
// a comment line is skipped when looking for the insertion point



layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };
uniform mat4 projectionMatrix;
uniform mat3 modelMatrixInverse;
uniform mat4 normalMatrix2;
uniform mat3 modelViewMatrix;
uniform mat4 modelMatrix; // trailing comments keep a line as is
float precisionSafe( float x ) { return x; } precision highp float;
void main() {
	gl_Position = projectionMatrix * modelViewMatrix * modelMatrix * vec4( normalMatrix * vec3( 0.0 ), 1.0 );
}
