extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern char *dlerror(void);
extern int strcmp(const char *, const char *);

// CUDA Driver API. Resolve the ABI from JetPack's original shared library.
int main(void) {
    void *lib = dlopen("libcuda.so.1", 2);
    if (!lib) {
        printf("CUDA library: %s\n", dlerror());
        return 1;
    }
    int (*init)(unsigned) = dlsym(lib, "cuInit");
    int (*get_count)(int *) = dlsym(lib, "cuDeviceGetCount");
    int (*get_name)(char *, int, int) = dlsym(lib, "cuDeviceGetName");
    if (!init || !get_count || !get_name)
        return 2;
    int status = init(0);
    printf("cuInit=%d\n", status);
    if (status)
        return 3;
    int count = 0;
    status = get_count(&count);
    printf("cuDeviceGetCount=%d devices=%d\n", status, count);
    if (status || !count)
        return 4;
    char name[128];
    status = get_name(name, sizeof(name), 0);
    if (!status)
        printf("CUDA device: %s\n", name);
    if (status || strcmp(name, "Orin") != 0)
        return 5;
    int (*get_attr)(int *, int, int) = dlsym(lib, "cuDeviceGetAttribute");
    int major = 0, minor = 0;
    if (!get_attr || get_attr(&major, 75, 0) || get_attr(&minor, 76, 0))
        return 6;
    printf("CUDA compute capability: %d.%d\n", major, minor);
    if (major != 8 || minor != 7)
        return 7;
    int (*create)(void **, unsigned, int) = dlsym(lib, "cuCtxCreate_v2");
    int (*destroy)(void *) = dlsym(lib, "cuCtxDestroy_v2");
    int (*alloc)(unsigned long *, unsigned long) = dlsym(lib, "cuMemAlloc_v2");
    int (*free_mem)(unsigned long) = dlsym(lib, "cuMemFree_v2");
    int (*upload)(unsigned long, const void *, unsigned long) = dlsym(lib, "cuMemcpyHtoD_v2");
    int (*download)(void *, unsigned long, unsigned long) = dlsym(lib, "cuMemcpyDtoH_v2");
    if (!create || !destroy || !alloc || !free_mem || !upload || !download)
        return 7;
    void *context;
    status = create(&context, 0, 0);
    printf("cuCtxCreate=%d\n", status);
    if (status)
        return 8;
    unsigned long device;
    status = alloc(&device, sizeof(unsigned));
    printf("cuMemAlloc=%d\n", status);
    if (status)
        return 9;
    unsigned input = 0xfeedcafe, output = 0;
    status = upload(device, &input, sizeof(input));
    printf("cuMemcpyHtoD=%d\n", status);
    if (status)
        return 10;
    status = download(&output, device, sizeof(output));
    printf("cuMemcpyDtoH=%d data=%#x\n", status, output);
    if (status || input != output)
        return 11;
    int (*load_module)(void **, const void *) = dlsym(lib, "cuModuleLoadData");
    int (*get_function)(void **, void *, const char *) = dlsym(lib, "cuModuleGetFunction");
    int (*launch)(void *, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned,
                  unsigned, void *, void **, void **) = dlsym(lib, "cuLaunchKernel");
    int (*synchronize)(void) = dlsym(lib, "cuCtxSynchronize");
    if (!load_module || !get_function || !launch || !synchronize)
        return 12;
    const char ptx[] =
        ".version 7.8\n.target sm_87\n.address_size 64\n"
        ".visible .entry write_value(.param .u64 target) {\n"
        "  .reg .b64 %p; .reg .b32 %v;\n"
        "  ld.param.u64 %p, [target];\n"
        "  mov.u32 %v, 0x24681357;\n"
        "  st.global.u32 [%p], %v;\n"
        "  ret;\n}\n";
    void *module, *function;
    status = load_module(&module, ptx);
    printf("cuModuleLoadData=%d\n", status);
    if (status)
        return 13;
    status = get_function(&function, module, "write_value");
    printf("cuModuleGetFunction=%d\n", status);
    if (status)
        return 14;
    void *args[] = {&device};
    status = launch(function, 1, 1, 1, 1, 1, 1, 0, 0, args, 0);
    printf("cuLaunchKernel=%d\n", status);
    if (status)
        return 15;
    status = synchronize();
    printf("cuCtxSynchronize=%d\n", status);
    if (status)
        return 16;
    output = 0;
    status = download(&output, device, sizeof(output));
    printf("CUDA kernel output=%#x status=%d\n", output, status);
    input = 0x24681357;
    free_mem(device);
    destroy(context);
    return status != 0 || input != output;
}
