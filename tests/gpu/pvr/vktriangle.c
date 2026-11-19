/* Offscreen rendering on the PowerVR (Mesa's pvr driver): a 64x64 RGBA8 colour attachment is
 * cleared to blue and, unless run with "clear", a triangle with red/green/blue corners is
 * drawn. The image is copied to a buffer and checked on the host; it is also written to
 * /tmp/vktriangle.ppm. Loads libvulkan.so.1 at run time. */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tri.vert.spv.h"
#include "tri.frag.spv.h"

#define W 64
#define H 64
static PFN_vkGetInstanceProcAddr gipa;
#define LOAD(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { printf("VKTRI_FAIL %s = %d\n", #x, r_); return 1; } } while (0)

static uint32_t find_type(VkPhysicalDeviceMemoryProperties *mp, uint32_t bits, VkMemoryPropertyFlags want) {
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want) return i;
    return 0;
}

int main(int argc, char **argv) {
    int draw = !(argc > 1 && strcmp(argv[1], "clear") == 0);
    void *lib = dlopen("libvulkan.so.1", RTLD_NOW);
    if (!lib) { printf("VKTRI_FAIL dlopen\n"); return 1; }
    gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
    VkInstance inst = NULL;
    LOAD(vkCreateInstance);
    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "vktriangle", 1, NULL, 0, VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &app };
    CHECK(vkCreateInstance(&ici, NULL, &inst));
    LOAD(vkEnumeratePhysicalDevices); LOAD(vkGetPhysicalDeviceMemoryProperties); LOAD(vkCreateDevice);
    LOAD(vkGetDeviceQueue); LOAD(vkCreateImage); LOAD(vkGetImageMemoryRequirements); LOAD(vkAllocateMemory);
    LOAD(vkBindImageMemory); LOAD(vkCreateImageView); LOAD(vkCreateRenderPass); LOAD(vkCreateFramebuffer);
    LOAD(vkCreateShaderModule); LOAD(vkCreatePipelineLayout); LOAD(vkCreateGraphicsPipelines);
    LOAD(vkCreateBuffer); LOAD(vkGetBufferMemoryRequirements); LOAD(vkBindBufferMemory); LOAD(vkMapMemory);
    LOAD(vkCreateCommandPool); LOAD(vkAllocateCommandBuffers); LOAD(vkBeginCommandBuffer); LOAD(vkCmdBeginRenderPass);
    LOAD(vkCmdBindPipeline); LOAD(vkCmdBindVertexBuffers); LOAD(vkCmdDraw); LOAD(vkCmdEndRenderPass);
    LOAD(vkCmdCopyImageToBuffer); LOAD(vkCmdPipelineBarrier); LOAD(vkEndCommandBuffer); LOAD(vkCreateFence);
    LOAD(vkQueueSubmit); LOAD(vkWaitForFences);

    uint32_t n = 1; VkPhysicalDevice pd;
    vkEnumeratePhysicalDevices(inst, &n, &pd);
    if (!n) { printf("VKTRI_FAIL no device\n"); return 1; }
    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci };
    VkDevice dev; CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, 0, 0, &q);

    VkImageCreateInfo imci = { VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, NULL, 0, VK_IMAGE_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM,
        { W, H, 1 }, 1, 1, VK_SAMPLE_COUNT_1_BIT, VK_IMAGE_TILING_OPTIMAL,
        VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, VK_SHARING_MODE_EXCLUSIVE, 0, NULL, VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage img; CHECK(vkCreateImage(dev, &imci, NULL, &img));
    VkMemoryRequirements mr; vkGetImageMemoryRequirements(dev, img, &mr);
    VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, NULL, mr.size, find_type(&mp, mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
    VkDeviceMemory imem; CHECK(vkAllocateMemory(dev, &mai, NULL, &imem)); CHECK(vkBindImageMemory(dev, img, imem, 0));
    VkImageViewCreateInfo ivci = { VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, NULL, 0, img, VK_IMAGE_VIEW_TYPE_2D, VK_FORMAT_R8G8B8A8_UNORM,
        {0}, { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    VkImageView view; CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

    VkAttachmentDescription ad = { 0, VK_FORMAT_R8G8B8A8_UNORM, VK_SAMPLE_COUNT_1_BIT, VK_ATTACHMENT_LOAD_OP_CLEAR, VK_ATTACHMENT_STORE_OP_STORE,
        VK_ATTACHMENT_LOAD_OP_DONT_CARE, VK_ATTACHMENT_STORE_OP_DONT_CARE, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
    VkAttachmentReference ar = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription sd = { 0, VK_PIPELINE_BIND_POINT_GRAPHICS, 0, NULL, 1, &ar };
    VkRenderPassCreateInfo rpci = { VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, NULL, 0, 1, &ad, 1, &sd };
    VkRenderPass rp; CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
    VkFramebufferCreateInfo fbci = { VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, NULL, 0, rp, 1, &view, W, H, 1 };
    VkFramebuffer fb; CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

    VkShaderModuleCreateInfo vsci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, sizeof(tri_vert_spv), (const uint32_t *)tri_vert_spv };
    VkShaderModuleCreateInfo fsci = { VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, NULL, 0, sizeof(tri_frag_spv), (const uint32_t *)tri_frag_spv };
    VkShaderModule vs, fs; CHECK(vkCreateShaderModule(dev, &vsci, NULL, &vs)); CHECK(vkCreateShaderModule(dev, &fsci, NULL, &fs));
    VkPipelineShaderStageCreateInfo st[2] = {
        { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_VERTEX_BIT, vs, "main" },
        { VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_FRAGMENT_BIT, fs, "main" } };
    VkVertexInputBindingDescription vb = { 0, 20, VK_VERTEX_INPUT_RATE_VERTEX };
    VkVertexInputAttributeDescription va[2] = { { 0, 0, VK_FORMAT_R32G32_SFLOAT, 0 }, { 1, 0, VK_FORMAT_R32G32B32_SFLOAT, 8 } };
    VkPipelineVertexInputStateCreateInfo vis = { VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, NULL, 0, 1, &vb, 2, va };
    VkPipelineInputAssemblyStateCreateInfo ias = { VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, NULL, 0, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
    VkViewport vp = { 0, 0, W, H, 0, 1 }; VkRect2D sc = { { 0, 0 }, { W, H } };
    VkPipelineViewportStateCreateInfo vps = { VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, NULL, 0, 1, &vp, 1, &sc };
    VkPipelineRasterizationStateCreateInfo rs = { VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, NULL, 0, 0, 0, VK_POLYGON_MODE_FILL,
        VK_CULL_MODE_NONE, VK_FRONT_FACE_COUNTER_CLOCKWISE, 0, 0, 0, 0, 1.0f };
    VkPipelineMultisampleStateCreateInfo ms = { VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, NULL, 0, VK_SAMPLE_COUNT_1_BIT };
    VkPipelineColorBlendAttachmentState cba = { 0 }; cba.colorWriteMask = 0xf;
    VkPipelineColorBlendStateCreateInfo cbs = { VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, NULL, 0, 0, 0, 1, &cba };
    VkPipelineLayoutCreateInfo plci = { VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
    VkPipelineLayout pl; CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
    VkGraphicsPipelineCreateInfo gpci = { VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, NULL, 0, 2, st, &vis, &ias, NULL, &vps, &rs, &ms,
        NULL, &cbs, NULL, pl, rp, 0 };
    VkPipeline pipe; CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

    /* vertex buffer: pos.xy, col.rgb; readback buffer */
    float verts[3][5] = { { 0.0f, -0.75f, 1, 0, 0 }, { 0.75f, 0.75f, 0, 1, 0 }, { -0.75f, 0.75f, 0, 0, 1 } };
    VkBufferCreateInfo vbci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, sizeof(verts), VK_BUFFER_USAGE_VERTEX_BUFFER_BIT };
    VkBufferCreateInfo rbci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, NULL, 0, W * H * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    VkBuffer vbuf, rbuf; CHECK(vkCreateBuffer(dev, &vbci, NULL, &vbuf)); CHECK(vkCreateBuffer(dev, &rbci, NULL, &rbuf));
    VkDeviceMemory vmem, rmem; void *vmap, *rmap;
    VkMemoryPropertyFlags host = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    vkGetBufferMemoryRequirements(dev, vbuf, &mr);
    mai.allocationSize = mr.size; mai.memoryTypeIndex = find_type(&mp, mr.memoryTypeBits, host);
    CHECK(vkAllocateMemory(dev, &mai, NULL, &vmem)); CHECK(vkBindBufferMemory(dev, vbuf, vmem, 0));
    CHECK(vkMapMemory(dev, vmem, 0, sizeof(verts), 0, &vmap)); memcpy(vmap, verts, sizeof(verts));
    vkGetBufferMemoryRequirements(dev, rbuf, &mr);
    mai.allocationSize = mr.size; mai.memoryTypeIndex = find_type(&mp, mr.memoryTypeBits, host);
    CHECK(vkAllocateMemory(dev, &mai, NULL, &rmem)); CHECK(vkBindBufferMemory(dev, rbuf, rmem, 0));
    CHECK(vkMapMemory(dev, rmem, 0, W * H * 4, 0, &rmap)); memset(rmap, 0xee, W * H * 4);

    VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
    VkCommandPool cp; CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cp));
    VkCommandBufferAllocateInfo cbai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
    VkCommandBuffer cb; CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
    VkCommandBufferBeginInfo cbbi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    CHECK(vkBeginCommandBuffer(cb, &cbbi));
    VkClearValue clear = { .color = { .float32 = { 0.0f, 0.0f, 1.0f, 1.0f } } };
    VkRenderPassBeginInfo rpbi = { VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, NULL, rp, fb, { { 0, 0 }, { W, H } }, 1, &clear };
    vkCmdBeginRenderPass(cb, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
    if (draw) {
        VkDeviceSize off = 0;
        vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
        vkCmdBindVertexBuffers(cb, 0, 1, &vbuf, &off);
        vkCmdDraw(cb, 3, 1, 0, 0);
    }
    vkCmdEndRenderPass(cb);
    VkBufferImageCopy bic = { 0, 0, 0, { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, { 0, 0, 0 }, { W, H, 1 } };
    vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rbuf, 1, &bic);
    VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, NULL, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_HOST_READ_BIT };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(cb));
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence f; CHECK(vkCreateFence(dev, &fci, NULL, &f));
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 0, NULL, NULL, 1, &cb };
    CHECK(vkQueueSubmit(q, 1, &si, f));
    CHECK(vkWaitForFences(dev, 1, &f, VK_TRUE, 20ull * 1000 * 1000 * 1000));

    uint8_t *px = rmap;
    FILE *ppm = fopen("/tmp/vktriangle.ppm", "wb");
    if (ppm) { fprintf(ppm, "P6\n%d %d\n255\n", W, H); for (int i = 0; i < W * H; i++) fwrite(px + 4 * i, 1, 3, ppm); fclose(ppm); }
    /* corner: the clear colour; centre (inside the triangle): a mix of red, green and blue */
    uint8_t *c = px, *m = px + 4 * (W * (H / 2 + 8) + W / 2);
    printf("corner %02x%02x%02x%02x centre %02x%02x%02x%02x\n", c[0], c[1], c[2], c[3], m[0], m[1], m[2], m[3]);
    int ok = c[0] == 0 && c[1] == 0 && c[2] == 255 && c[3] == 255;
    if (draw) ok = ok && m[0] > 16 && m[1] > 16 && m[2] > 16 && !(m[0] == 0 && m[1] == 0 && m[2] == 255);
    printf(ok ? "VKTRI_OK %s\n" : "VKTRI_FAIL %s\n", draw ? "triangle" : "clear");
    return !ok;
}
