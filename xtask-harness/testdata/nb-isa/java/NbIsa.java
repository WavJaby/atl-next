// SPDX-License-Identifier: MIT
// Test driver of `xtask test nb-isa` (xtask-harness/src/nb_isa.rs), run on host ART; never shipped. The gate judges ART's and the bridge's lines, not this class: it only has to get the runtime through Runtime::Start.
public final class NbIsa {
    public static void main(String[] args) {
        System.out.println("nb-isa: main ran");
    }
}
