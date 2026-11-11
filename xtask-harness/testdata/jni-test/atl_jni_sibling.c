// SPDX-License-Identifier: MIT
// bridge-1 fixture: dlopen()ed by atl_jni_test.c; exports one symbol.
__attribute__((visibility("default")))
int atl_sibling_answer(void)
{
    return 42;
}
