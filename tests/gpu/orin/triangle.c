/* One OpenGL ES 2 triangle through the stock JetPack driver: a vertex buffer, a GLSL
 * program compiled by NVIDIA's compiler, and two read-back pixels (inside and outside). */
extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);

typedef void *(*get_proc_t)(const char *);
static get_proc_t get_proc;
#define GL(ret, name, ...) ret (*name)(__VA_ARGS__) = get_proc(#name)

static const char *vertex_source =
    "attribute vec4 position;\n"
    "attribute vec4 color;\n"
    "varying vec4 shade;\n"
    "void main() { gl_Position = position; shade = color; }\n";
static const char *fragment_source =
    "precision mediump float;\n"
    "varying vec4 shade;\n"
    "void main() { gl_FragColor = shade; }\n";

int main(void) {
    void *lib = dlopen("libEGL.so.1", 2);
    if (!lib)
        return 1;
    get_proc = dlsym(lib, "eglGetProcAddress");
    int (*get_error)(void) = dlsym(lib, "eglGetError");
    unsigned (*initialize)(void *, int *, int *) = dlsym(lib, "eglInitialize");
    unsigned (*query_devices)(int, void **, int *) = get_proc("eglQueryDevicesEXT");
    void *(*platform_display)(unsigned, void *, const int *) = get_proc("eglGetPlatformDisplayEXT");
    unsigned (*bind_api)(unsigned) = dlsym(lib, "eglBindAPI");
    unsigned (*choose_config)(void *, const int *, void **, int, int *) = dlsym(lib, "eglChooseConfig");
    void *(*create_context)(void *, void *, void *, const int *) = dlsym(lib, "eglCreateContext");
    void *(*create_pbuffer)(void *, void *, const int *) = dlsym(lib, "eglCreatePbufferSurface");
    unsigned (*make_current)(void *, void *, void *, void *) = dlsym(lib, "eglMakeCurrent");
    int count = 0, major, minor;
    void *devices[8] = {0};
    query_devices(8, devices, &count);
    void *display = count ? platform_display(0x313f, devices[0], 0) : 0;
    if (!display || !initialize(display, &major, &minor))
        return 2;
    const int config_attribs[] = {0x3033, 1, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038};
    void *config;
    if (!bind_api(0x30a0) || !choose_config(display, config_attribs, &config, 1, &count) || !count)
        return 3;
    const int context_attribs[] = {0x3098, 2, 0x3038};
    const int surface_attribs[] = {0x3057, 32, 0x3056, 32, 0x3038};
    void *context = create_context(display, config, 0, context_attribs);
    void *surface = create_pbuffer(display, config, surface_attribs);
    if (!context || !surface || !make_current(display, surface, surface, context)) {
        printf("EGL error=%#x\n", get_error());
        return 4;
    }
    GL(unsigned, glCreateShader, unsigned);
    GL(void, glShaderSource, unsigned, int, const char **, const int *);
    GL(void, glCompileShader, unsigned);
    GL(void, glGetShaderiv, unsigned, unsigned, int *);
    GL(unsigned, glCreateProgram, void);
    GL(void, glAttachShader, unsigned, unsigned);
    GL(void, glBindAttribLocation, unsigned, unsigned, const char *);
    GL(void, glLinkProgram, unsigned);
    GL(void, glGetProgramiv, unsigned, unsigned, int *);
    GL(void, glUseProgram, unsigned);
    GL(void, glGenBuffers, int, unsigned *);
    GL(void, glBindBuffer, unsigned, unsigned);
    GL(void, glBufferData, unsigned, long, const void *, unsigned);
    GL(void, glVertexAttribPointer, unsigned, int, unsigned, unsigned char, int, const void *);
    GL(void, glEnableVertexAttribArray, unsigned);
    GL(void, glViewport, int, int, int, int);
    GL(void, glClearColor, float, float, float, float);
    GL(void, glClear, unsigned);
    GL(void, glDrawArrays, unsigned, int, int);
    GL(void, glFinish, void);
    GL(unsigned, glGetError, void);
    GL(void, glReadPixels, int, int, int, int, unsigned, unsigned, void *);

    unsigned shaders[2], program = glCreateProgram();
    const char *sources[2] = {vertex_source, fragment_source};
    for (int i = 0; i < 2; i++) {
        int ok = 0;
        shaders[i] = glCreateShader(i ? 0x8b30 : 0x8b31);
        glShaderSource(shaders[i], 1, &sources[i], 0);
        glCompileShader(shaders[i]);
        glGetShaderiv(shaders[i], 0x8b81, &ok);
        printf("compile %s=%d\n", i ? "fragment" : "vertex", ok);
        glAttachShader(program, shaders[i]);
    }
    glBindAttribLocation(program, 0, "position");
    glBindAttribLocation(program, 1, "color");
    int linked = 0;
    glLinkProgram(program);
    glGetProgramiv(program, 0x8b82, &linked);
    printf("link=%d\n", linked);
    glUseProgram(program);

    /* A triangle over the lower-left half: x, y and an RGBA colour, all green. */
    static const float vertices[] = {
        -1, -1, 0, 1, 0, 1,
        1, -1, 0, 1, 0, 1,
        -1, 1, 0, 1, 0, 1,
    };
    unsigned buffer;
    glGenBuffers(1, &buffer);
    glBindBuffer(0x8892, buffer);
    glBufferData(0x8892, sizeof vertices, vertices, 0x88e4);
    glVertexAttribPointer(0, 2, 0x1406, 0, 24, 0);
    glVertexAttribPointer(1, 4, 0x1406, 0, 24, (const void *)8);
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);
    glViewport(0, 0, 32, 32);
    glClearColor(1, 0, 0, 1);
    glClear(0x4000);
    glDrawArrays(4, 0, 3);
    glFinish();
    unsigned char inside[4] = {0}, outside[4] = {0};
    glReadPixels(4, 4, 1, 1, 0x1908, 0x1401, inside);
    glReadPixels(28, 28, 1, 1, 0x1908, 0x1401, outside);
    printf("GL error=%#x\n", glGetError());
    /* The surface, every fourth pixel from the top row down: # is green, . is red. */
    static unsigned char image[32 * 32 * 4];
    glReadPixels(0, 0, 32, 32, 0x1908, 0x1401, image);
    for (int y = 30; y >= 0; y -= 4) {
        char row[9] = {0};
        for (int x = 0; x < 8; x++) {
            const unsigned char *p = image + 4 * (32 * y + 4 * x + 1);
            row[x] = p[1] == 255 && p[0] == 0 ? '#' : p[0] == 255 && p[1] == 0 ? '.' : '?';
        }
        printf("triangle map: %s\n", row);
    }
    printf("triangle inside: %u %u %u %u\n", inside[0], inside[1], inside[2], inside[3]);
    printf("triangle outside: %u %u %u %u\n", outside[0], outside[1], outside[2], outside[3]);
    return !(inside[0] == 0 && inside[1] == 255 && inside[2] == 0 && outside[0] == 255 && outside[1] == 0);
}
