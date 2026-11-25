/* The stock JetPack EGL and OpenGL ES stack on the emulated Orin GPU: a pbuffer context,
 * a red clear and a read back. Built freestanding against glibc only; the NVIDIA libraries
 * are loaded with dlopen so the probe needs no EGL or GLES headers. */
extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern char *dlerror(void);

int main(void) {
    void *lib = dlopen("libEGL.so.1", 2);
    if (!lib) {
        printf("EGL library: %s\n", dlerror());
        return 1;
    }
    void *(*get_proc)(const char *) = dlsym(lib, "eglGetProcAddress");
    int (*get_error)(void) = dlsym(lib, "eglGetError");
    unsigned (*initialize)(void *, int *, int *) = dlsym(lib, "eglInitialize");
    if (!get_proc || !get_error || !initialize)
        return 2;
    unsigned (*query_devices)(int, void **, int *) = get_proc("eglQueryDevicesEXT");
    void *(*platform_display)(unsigned, void *, const int *) = get_proc("eglGetPlatformDisplayEXT");
    if (!query_devices || !platform_display)
        return 3;
    int count = 0, major = 0, minor = 0;
    void *devices[8] = {0};
    unsigned result = query_devices(8, devices, &count);
    printf("eglQueryDevicesEXT=%u count=%d error=%#x\n", result, count, get_error());
    /* EGL_PLATFORM_DEVICE_EXT, or EGL_PLATFORM_SURFACELESS_MESA without a device. */
    void *display = count ? platform_display(0x313f, devices[0], 0) : platform_display(0x31dd, 0, 0);
    printf("eglGetPlatformDisplayEXT=%p error=%#x\n", display, get_error());
    if (!display)
        return 4;
    result = initialize(display, &major, &minor);
    printf("eglInitialize=%u version=%d.%d error=%#x\n", result, major, minor, get_error());
    if (!result)
        return 5;
    unsigned (*bind_api)(unsigned) = dlsym(lib, "eglBindAPI");
    unsigned (*choose_config)(void *, const int *, void **, int, int *) = dlsym(lib, "eglChooseConfig");
    void *(*create_context)(void *, void *, void *, const int *) = dlsym(lib, "eglCreateContext");
    void *(*create_pbuffer)(void *, void *, const int *) = dlsym(lib, "eglCreatePbufferSurface");
    unsigned (*make_current)(void *, void *, void *, void *) = dlsym(lib, "eglMakeCurrent");
    if (!bind_api || !choose_config || !create_context || !create_pbuffer || !make_current)
        return 6;
    /* PBUFFER surfaces, OpenGL ES 2 renderable, RGB 888. */
    const int config_attribs[] = {0x3033, 1, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038};
    void *config;
    if (!bind_api(0x30a0) || !choose_config(display, config_attribs, &config, 1, &count) || !count) {
        printf("eglChooseConfig error=%#x\n", get_error());
        return 7;
    }
    const int context_attribs[] = {0x3098, 2, 0x3038};
    const int surface_attribs[] = {0x3057, 32, 0x3056, 32, 0x3038};
    void *context = create_context(display, config, 0, context_attribs);
    printf("eglCreateContext=%p error=%#x\n", context, get_error());
    if (!context)
        return 8;
    void *surface = create_pbuffer(display, config, surface_attribs);
    printf("eglCreatePbufferSurface=%p error=%#x\n", surface, get_error());
    if (!surface || !make_current(display, surface, surface, context))
        return 9;
    const char *(*get_string)(unsigned) = get_proc("glGetString");
    void (*clear_color)(float, float, float, float) = get_proc("glClearColor");
    void (*clear)(unsigned) = get_proc("glClear");
    void (*read_pixels)(int, int, int, int, unsigned, unsigned, void *) = get_proc("glReadPixels");
    if (!get_string || !clear_color || !clear || !read_pixels)
        return 10;
    printf("GL renderer: %s\n", get_string(0x1f01));
    clear_color(1, 0, 0, 1);
    clear(0x4000);
    unsigned char pixel[4] = {0};
    read_pixels(16, 16, 1, 1, 0x1908, 0x1401, pixel);
    printf("GL pixel: %u %u %u %u\n", pixel[0], pixel[1], pixel[2], pixel[3]);
    return pixel[0] != 255 || pixel[1] || pixel[2] || pixel[3] != 255;
}
