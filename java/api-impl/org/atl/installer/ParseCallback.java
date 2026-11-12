// SPDX-License-Identifier: MIT
package org.atl.installer;

import android.content.res.TypedArray;

import com.android.internal.pm.parsing.pkg.PackageImpl;
import com.android.internal.pm.pkg.parsing.ParsingPackage;
import com.android.internal.pm.pkg.parsing.ParsingPackageUtils;
import com.android.internal.util.ArrayUtils;

import java.util.Set;

/**
 * The installer's ParsingPackageUtils.Callback (AGENTS Languages: a forwarder, because JNI cannot create an object implementing a Java interface).
 * Adapts and forwards only: every answer is a value the installer (atl-app-launcher installer.rs) passes in; startParsingPackage is PackageParser2.Callback's (PackageParser2.java:192-197).
 */
public final class ParseCallback implements ParsingPackageUtils.Callback {
    private final String[] mAvailableFeatures;
    private final Set<String> mHiddenApiWhitelistedApps;
    private final Set<String> mInstallConstraintsAllowlist;

    public ParseCallback(String[] availableFeatures, Set<String> hiddenApiWhitelistedApps,
            Set<String> installConstraintsAllowlist) {
        mAvailableFeatures = availableFeatures;
        mHiddenApiWhitelistedApps = hiddenApiWhitelistedApps;
        mInstallConstraintsAllowlist = installConstraintsAllowlist;
    }

    @Override
    public boolean hasFeature(String feature) {
        return ArrayUtils.contains(mAvailableFeatures, feature);
    }

    @Override
    public ParsingPackage startParsingPackage(String packageName, String baseApkPath, String path,
            TypedArray manifestArray, boolean isCoreApp) {
        return PackageImpl.forParsing(packageName, baseApkPath, path, manifestArray, isCoreApp,
                this);
    }

    @Override
    public Set<String> getHiddenApiWhitelistedApps() {
        return mHiddenApiWhitelistedApps;
    }

    @Override
    public Set<String> getInstallConstraintsAllowlist() {
        return mInstallConstraintsAllowlist;
    }
}
