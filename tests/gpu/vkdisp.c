/* Vulkan on HDMI without a window system: VK_KHR_display surface on the first display, a FIFO
 * swapchain, N frames cleared to a colour that cycles, presented through vc4.
 *   vkdisp [frames]          gcc -o vkdisp vkdisp.c -lvulkan */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <math.h>
#define CK(x) do { VkResult r_ = (x); if (r_) { printf("%s failed: %d\n", #x, r_); return 1; } } while (0)
int main(int argc, char **argv) {
    int frames = argc > 1 ? atoi(argv[1]) : 120;
    const char *ext[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_DISPLAY_EXTENSION_NAME};
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "vkdisp", 1, NULL, 0, VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &app, 0, NULL, 2, ext};
    VkInstance inst; printf("create instance\n"); fflush(stdout);
    VkResult r = vkCreateInstance(&ici, NULL, &inst); printf(" -> %d\n", r); fflush(stdout);
    if (r) return 1;
    uint32_t n = 4; VkPhysicalDevice pd[4];
    printf("enumerate\n"); fflush(stdout);
    r = vkEnumeratePhysicalDevices(inst, &n, pd); printf(" -> %d, %u devices\n", r, n); fflush(stdout);
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties p; vkGetPhysicalDeviceProperties(pd[i], &p);
        uint32_t nd = 0;
        PFN_vkGetPhysicalDeviceDisplayPropertiesKHR f = (void *)vkGetInstanceProcAddr(inst, "vkGetPhysicalDeviceDisplayPropertiesKHR");
        printf("%s: display props fn %p\n", p.deviceName, (void *)f); fflush(stdout);
        r = f(pd[i], &nd, NULL); printf(" -> %d, %u displays\n", r, nd); fflush(stdout);
        VkDisplayPropertiesKHR dp[4]; if (nd > 4) nd = 4;
        if (nd) { f(pd[i], &nd, dp); for (uint32_t k = 0; k < nd; k++) printf("  %s %ux%u\n", dp[k].displayName, dp[k].physicalResolution.width, dp[k].physicalResolution.height); }
    }
    /* present on the first device that has a display */
    VkPhysicalDevice gpu = pd[0];
    uint32_t nd = 1; VkDisplayPropertiesKHR dprop;
    CK(vkGetPhysicalDeviceDisplayPropertiesKHR(gpu, &nd, &dprop));
    uint32_t nm = 0; vkGetDisplayModePropertiesKHR(gpu, dprop.display, &nm, NULL);
    VkDisplayModePropertiesKHR *modes = calloc(nm, sizeof *modes); vkGetDisplayModePropertiesKHR(gpu, dprop.display, &nm, modes);
    uint32_t np = 0; vkGetPhysicalDeviceDisplayPlanePropertiesKHR(gpu, &np, NULL);
    printf("%u modes, %u planes; mode 0 %ux%u@%u\n", nm, np, modes[0].parameters.visibleRegion.width, modes[0].parameters.visibleRegion.height, modes[0].parameters.refreshRate);
    uint32_t plane = 0;
    for (; plane < np; plane++) {
        uint32_t c = 0; vkGetDisplayPlaneSupportedDisplaysKHR(gpu, plane, &c, NULL);
        VkDisplayKHR ds[8]; if (c > 8) c = 8; vkGetDisplayPlaneSupportedDisplaysKHR(gpu, plane, &c, ds);
        int ok = 0; for (uint32_t k = 0; k < c; k++) ok |= ds[k] == dprop.display;
        if (ok) break;
    }
    VkExtent2D ext2 = modes[0].parameters.visibleRegion;
    VkDisplaySurfaceCreateInfoKHR dsci = {VK_STRUCTURE_TYPE_DISPLAY_SURFACE_CREATE_INFO_KHR, NULL, 0, modes[0].displayMode, plane, 0,
                                          VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR, 1.0f, VK_DISPLAY_PLANE_ALPHA_OPAQUE_BIT_KHR, ext2};
    VkSurfaceKHR surf; CK(vkCreateDisplayPlaneSurfaceKHR(inst, &dsci, NULL, &surf));
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, 0, 1, &prio};
    const char *dext[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 1, dext, NULL};
    VkDevice dev; CK(vkCreateDevice(gpu, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, 0, 0, &q);
    uint32_t nf = 1; VkSurfaceFormatKHR sf; vkGetPhysicalDeviceSurfaceFormatsKHR(gpu, surf, &nf, &sf);
    VkSwapchainCreateInfoKHR sci = {VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, NULL, 0, surf, 3, sf.format, sf.colorSpace, ext2, 1,
                                    VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT, VK_SHARING_MODE_EXCLUSIVE, 0, NULL,
                                    VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR, VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, VK_PRESENT_MODE_FIFO_KHR, VK_TRUE, VK_NULL_HANDLE};
    VkSwapchainKHR sc; CK(vkCreateSwapchainKHR(dev, &sci, NULL, &sc));
    uint32_t ni = 0; vkGetSwapchainImagesKHR(dev, sc, &ni, NULL);
    VkImage imgs[8]; if (ni > 8) ni = 8; vkGetSwapchainImagesKHR(dev, sc, &ni, imgs);
    printf("swapchain: %u images, format %d\n", ni, sf.format); fflush(stdout);
    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, 0};
    VkCommandPool pool; CK(vkCreateCommandPool(dev, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo ai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, pool, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1};
    VkCommandBuffer cb; CK(vkAllocateCommandBuffers(dev, &ai, &cb));
    VkSemaphoreCreateInfo sei = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, NULL, 0};
    VkSemaphore acq, done; vkCreateSemaphore(dev, &sei, NULL, &acq); vkCreateSemaphore(dev, &sei, NULL, &done);
    VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, NULL, 0};
    VkFence fence; vkCreateFence(dev, &fci, NULL, &fence);
    for (int f = 0; f < frames; f++) {
        uint32_t idx; CK(vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acq, VK_NULL_HANDLE, &idx));
        VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, NULL};
        vkBeginCommandBuffer(cb, &bi);
        VkImageSubresourceRange rr = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageMemoryBarrier b1 = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, NULL, 0, VK_ACCESS_TRANSFER_WRITE_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                                   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED, imgs[idx], rr};
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b1);
        float t = f / 30.0f;
        VkClearColorValue cc = {{0.5f + 0.5f * sinf(t), 0.5f + 0.5f * sinf(t + 2.1f), 0.5f + 0.5f * sinf(t + 4.2f), 1}};
        vkCmdClearColorImage(cb, imgs[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &cc, 1, &rr);
        VkImageMemoryBarrier b2 = b1; b2.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT; b2.dstAccessMask = 0;
        b2.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; b2.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &b2);
        vkEndCommandBuffer(cb);
        VkPipelineStageFlags ws = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 1, &acq, &ws, 1, &cb, 1, &done};
        CK(vkQueueSubmit(q, 1, &si, fence));
        VkPresentInfoKHR pi = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, NULL, 1, &done, 1, &sc, &idx, NULL};
        CK(vkQueuePresentKHR(q, &pi));
        vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX); vkResetFences(dev, 1, &fence);
    }
    vkDeviceWaitIdle(dev);
    printf("presented %d frames\n", frames);
    printf("ok\n");
    return 0;
}
