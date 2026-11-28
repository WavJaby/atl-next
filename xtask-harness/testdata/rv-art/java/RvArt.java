// SPDX-License-Identifier: MIT
// rv-art fixture (rv_art.rs): `hello` prints one line; `loop <reps>` runs hot() reps times and prints the sum the gate recomputes in Rust.
// hot() uses what the riscv64 code paths specialise on Zba/Zbb: long[] indexing (sh3add), rotates, bitCount, min/max, so a JIT that emits an extension the CPU lacks dies by SIGILL.
public class RvArt {
    static long hot(int n) {
        long[] a = new long[64];
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (long) i * i ^ (s >>> 3);
            s = Long.rotateLeft(s, 7) + Integer.bitCount(i);
            a[i & 63] += Math.max(s, (long) i) - Math.min(s, (long) -i);
        }
        long t = 0;
        for (long v : a) {
            t ^= v;
        }
        return s ^ t;
    }

    public static void main(String[] args) {
        if (args[0].equals("hello")) {
            System.out.println("rv-art hello");
            return;
        }
        int reps = Integer.parseInt(args[1]);
        long r = 0;
        for (int k = 0; k < reps; k++) {
            r += hot(10000);
        }
        System.out.println("rv-art sum " + r);
    }
}
