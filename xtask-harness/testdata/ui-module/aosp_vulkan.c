/* SPDX-License-Identifier: MIT */
/* Stand-in for AOSP's libvulkan.so (soname libvulkan.so, as the NDK stub an app links names it): a value no Vulkan loader returns, so the app's result tells which libvulkan its import bound to. */
int vkCreateInstance(const void *info, const void *allocator, void *instance) {
    (void)info;
    (void)allocator;
    (void)instance;
    return 4242;
}
