/* A Vulkan layer that marks every memory type DEVICE_LOCAL (for NVK on Tegra, which has no VRAM). */
#include <string.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>
static PFN_vkGetInstanceProcAddr next_gipa;
static PFN_vkGetPhysicalDeviceMemoryProperties next_mem;
static PFN_vkGetPhysicalDeviceMemoryProperties2 next_mem2;
static void fix(VkPhysicalDeviceMemoryProperties *p) {
    for (uint32_t i = 0; i < p->memoryTypeCount; i++) p->memoryTypes[i].propertyFlags |= VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT;
    for (uint32_t i = 0; i < p->memoryHeapCount; i++) p->memoryHeaps[i].flags |= VK_MEMORY_HEAP_DEVICE_LOCAL_BIT;
}
static VKAPI_ATTR void VKAPI_CALL mem(VkPhysicalDevice d, VkPhysicalDeviceMemoryProperties *p) { next_mem(d, p); fix(p); }
static VKAPI_ATTR void VKAPI_CALL mem2(VkPhysicalDevice d, VkPhysicalDeviceMemoryProperties2 *p) { next_mem2(d, p); fix(&p->memoryProperties); }
static VKAPI_ATTR VkResult VKAPI_CALL create_instance(const VkInstanceCreateInfo *ci, const VkAllocationCallbacks *a, VkInstance *inst) {
    VkLayerInstanceCreateInfo *li = (VkLayerInstanceCreateInfo *)ci->pNext;
    while (li && !(li->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO && li->function == VK_LAYER_LINK_INFO)) li = (VkLayerInstanceCreateInfo *)li->pNext;
    if (!li) return VK_ERROR_INITIALIZATION_FAILED;
    next_gipa = li->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    li->u.pLayerInfo = li->u.pLayerInfo->pNext;
    PFN_vkCreateInstance next_create = (PFN_vkCreateInstance)next_gipa(NULL, "vkCreateInstance");
    VkResult r = next_create(ci, a, inst);
    if (r == VK_SUCCESS) {
        next_mem = (PFN_vkGetPhysicalDeviceMemoryProperties)next_gipa(*inst, "vkGetPhysicalDeviceMemoryProperties");
        next_mem2 = (PFN_vkGetPhysicalDeviceMemoryProperties2)next_gipa(*inst, "vkGetPhysicalDeviceMemoryProperties2");
    }
    return r;
}
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL uma_GetInstanceProcAddr(VkInstance inst, const char *n) {
    if (!strcmp(n, "vkCreateInstance")) return (PFN_vkVoidFunction)create_instance;
    if (!strcmp(n, "vkGetInstanceProcAddr")) return (PFN_vkVoidFunction)uma_GetInstanceProcAddr;
    if (!strcmp(n, "vkGetPhysicalDeviceMemoryProperties")) return (PFN_vkVoidFunction)mem;
    if (!strcmp(n, "vkGetPhysicalDeviceMemoryProperties2") || !strcmp(n, "vkGetPhysicalDeviceMemoryProperties2KHR")) return (PFN_vkVoidFunction)mem2;
    return next_gipa ? next_gipa(inst, n) : NULL;
}
VKAPI_ATTR VkResult VKAPI_CALL uma_NegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
    if (v->loaderLayerInterfaceVersion > 2) v->loaderLayerInterfaceVersion = 2;
    v->pfnGetInstanceProcAddr = uma_GetInstanceProcAddr;
    v->pfnGetDeviceProcAddr = NULL;
    v->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
