/* SPDX-License-Identifier: MIT */
/* Stand-in for an app's native library that links -lvulkan (DT_NEEDED libvulkan.so): app_probe returns what its vkCreateInstance import returns. */
int vkCreateInstance(const void *info, const void *allocator, void *instance);

int app_probe(void) { return vkCreateInstance(0, 0, 0); }
