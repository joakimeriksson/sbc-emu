// Shared helpers for the WebGL demo pages: compile, link, report errors on the page and in the
// console (the test scripts grep the browser's output for "GLDEMO").
function glDemo(canvas, opts) {
  const gl = canvas.getContext('webgl2', opts || { antialias: false });
  const status = document.getElementById('status');
  const say = (s) => { if (status) status.textContent = s; console.log('GLDEMO ' + s); };
  if (!gl) { say('no WebGL2'); return null; }
  say('renderer ' + (gl.getParameter(gl.RENDERER) || '?'));
  const dbg = gl.getExtension('WEBGL_debug_renderer_info');
  if (dbg) say('renderer ' + gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL));
  gl.prog = (vs, fs) => {
    const p = gl.createProgram();
    for (const [t, s] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]]) {
      const sh = gl.createShader(t);
      gl.shaderSource(sh, s); gl.compileShader(sh);
      if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) { say('compile error: ' + gl.getShaderInfoLog(sh)); return null; }
      gl.attachShader(p, sh);
    }
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) { say('link error: ' + gl.getProgramInfoLog(p)); return null; }
    return p;
  };
  gl.say = say;
  return gl;
}

// Frames per second, shown on the page and logged every few seconds.
function fpsMeter(gl) {
  let n = 0, t0 = performance.now(), last = t0;
  return () => {
    n++;
    const t = performance.now();
    if (t - last > 3000) { gl.say('fps ' + (n * 1000 / (t - last)).toFixed(1) + ' frames ' + n); n = 0; last = t; }
    const e = gl.getError(); if (e) gl.say('GL error ' + e);
  };
}

// Animation time in seconds; "?t=2.5" in the URL freezes it, so one frame can be rendered on two
// drivers (V3D and llvmpipe) and compared pixel by pixel.
const FIXED_T = new URLSearchParams(location.search).get('t');
function demoTime(ms) { return FIXED_T !== null ? parseFloat(FIXED_T) : ms / 1000; }

const FULLSCREEN_VS = `#version 300 es
in vec2 p; out vec2 uv;
void main() { uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }`;

function fullscreenQuad(gl, prog) {
  const b = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, b);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(prog, 'p');
  gl.enableVertexAttribArray(loc); gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
}
