/* CUDA driver API smoke test through NVIDIA's libcuda (dlopen, no toolkit): device query, PTX JIT, vector add. */
#include <stdio.h>
#include <stdlib.h>
#include <dlfcn.h>
typedef int CUresult; typedef unsigned long long CUdeviceptr;
static const char *ptx =
".version 6.0\n.target sm_53\n.address_size 64\n"
".visible .entry vadd(.param .u64 a, .param .u64 b, .param .u64 c, .param .u32 n) {\n"
" .reg .pred %p; .reg .b32 %r<6>; .reg .f32 %f<4>; .reg .b64 %rd<11>;\n"
" ld.param.u64 %rd1,[a]; ld.param.u64 %rd2,[b]; ld.param.u64 %rd3,[c]; ld.param.u32 %r1,[n];\n"
" mov.u32 %r2,%ctaid.x; mov.u32 %r3,%ntid.x; mov.u32 %r4,%tid.x; mad.lo.s32 %r5,%r2,%r3,%r4;\n"
" setp.ge.s32 %p,%r5,%r1; @%p bra DONE;\n"
" cvta.to.global.u64 %rd4,%rd1; cvta.to.global.u64 %rd5,%rd2; cvta.to.global.u64 %rd6,%rd3;\n"
" mul.wide.s32 %rd7,%r5,4; add.s64 %rd8,%rd4,%rd7; add.s64 %rd9,%rd5,%rd7; add.s64 %rd10,%rd6,%rd7;\n"
" ld.global.f32 %f1,[%rd8]; ld.global.f32 %f2,[%rd9]; add.f32 %f3,%f1,%f2; st.global.f32 [%rd10],%f3;\n"
"DONE: ret;\n}\n";
static void *h;
static void *sym(const char *n) { void *p = dlsym(h, n); if (!p) { printf("missing %s\n", n); exit(2); } return p; }
#define CK(call) do { CUresult r_ = (call); printf("%-40s -> %d\n", #call, r_); fflush(stdout); if (r_) return 10 + r_ % 200; } while (0)
int main(void) {
    h = dlopen("libcuda.so.1.1", RTLD_NOW);
    if (!h) h = dlopen("libcuda.so.1", RTLD_NOW); /* JetPack 6's file name */
    if (!h) { printf("dlopen: %s\n", dlerror()); return 1; }
    CUresult (*cuInit)(unsigned) = sym("cuInit");
    CUresult (*cuDriverGetVersion)(int *) = sym("cuDriverGetVersion");
    CUresult (*cuDeviceGetCount)(int *) = sym("cuDeviceGetCount");
    CUresult (*cuDeviceGet)(int *, int) = sym("cuDeviceGet");
    CUresult (*cuDeviceGetName)(char *, int, int) = sym("cuDeviceGetName");
    CUresult (*cuDeviceGetAttribute)(int *, int, int) = sym("cuDeviceGetAttribute");
    CUresult (*cuCtxCreate)(void **, unsigned, int) = sym("cuCtxCreate_v2");
    CUresult (*cuModuleLoadData)(void **, const void *) = sym("cuModuleLoadData");
    CUresult (*cuModuleGetFunction)(void **, void *, const char *) = sym("cuModuleGetFunction");
    CUresult (*cuMemAlloc)(CUdeviceptr *, size_t) = sym("cuMemAlloc_v2");
    CUresult (*cuMemcpyHtoD)(CUdeviceptr, const void *, size_t) = sym("cuMemcpyHtoD_v2");
    CUresult (*cuMemcpyDtoH)(void *, CUdeviceptr, size_t) = sym("cuMemcpyDtoH_v2");
    CUresult (*cuLaunchKernel)(void *, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned, void *, void **, void **) = sym("cuLaunchKernel");
    CUresult (*cuCtxSynchronize)(void) = sym("cuCtxSynchronize");
    int v = 0, n = 0, dev = 0, maj = 0, min = 0, sms = 0; char name[128] = "";
    CK(cuInit(0));
    CK(cuDriverGetVersion(&v)); printf("driver version %d\n", v);
    CK(cuDeviceGetCount(&n)); printf("devices %d\n", n);
    CK(cuDeviceGet(&dev, 0));
    CK(cuDeviceGetName(name, sizeof name, dev));
    cuDeviceGetAttribute(&maj, 75, dev); cuDeviceGetAttribute(&min, 76, dev); cuDeviceGetAttribute(&sms, 16, dev);
    printf("CUDA_DEVICE %s, compute %d.%d, %d SMs\n", name, maj, min, sms); fflush(stdout);
    void *ctx, *mod, *fn;
    CK(cuCtxCreate(&ctx, 0, dev));
    CK(cuModuleLoadData(&mod, ptx));
    CK(cuModuleGetFunction(&fn, mod, "vadd"));
    enum { N = 4096 }; static float a[N], b[N], c[N];
    for (int i = 0; i < N; i++) { a[i] = i; b[i] = 2 * i + 0.5f; }
    CUdeviceptr da, db, dc; unsigned nn = N;
    CK(cuMemAlloc(&da, sizeof a)); CK(cuMemAlloc(&db, sizeof b)); CK(cuMemAlloc(&dc, sizeof c));
    CK(cuMemcpyHtoD(da, a, sizeof a)); CK(cuMemcpyHtoD(db, b, sizeof b));
    void *args[] = { &da, &db, &dc, &nn };
    CK(cuLaunchKernel(fn, N / 128, 1, 1, 128, 1, 1, 0, 0, args, 0));
    CK(cuCtxSynchronize());
    CK(cuMemcpyDtoH(c, dc, sizeof c));
    int bad = 0; for (int i = 0; i < N; i++) if (c[i] != a[i] + b[i]) bad++;
    printf("CUDA_VADD %s (%d wrong of %d; c[100]=%g)\n", bad ? "WRONG" : "OK", bad, N, c[100]);
    return bad ? 3 : 0;
}
