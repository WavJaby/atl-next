// SPDX-License-Identifier: MIT
package com.example.atlkeystore;

import android.app.Activity;
import android.os.Bundle;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyInfo;
import android.security.keystore.KeyProperties;
import android.security.keystore.StrongBoxUnavailableException;
import android.util.Log;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.KeyStore;
import java.util.Arrays;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;

/**
 * `xtask test keystore` app stage: AndroidKeyStore through the public API, in an app process the launcher started as zygote's child would be (the provider comes from that start, not from this
 * code). First launch (no ciphertext file): AES-256-GCM key `ks-s2`, encrypt + decrypt, KeyInfo, StrongBox refusal; writes iv + ciphertext to its files dir. Next launch (a new atld run on the
 * same data root): loads the key and decrypts that file. One `AtlKeystoreApp: <check> <value>` line each, then `done`.
 */
public final class AesActivity extends Activity {
    private static final String TAG = "AtlKeystoreApp";
    private static final String ALIAS = "ks-s2";
    private static final String PROVIDER = "AndroidKeyStore";
    private static final byte[] PLAIN = "atl keystore s2 plaintext".getBytes(StandardCharsets.UTF_8);
    /** GCM: 12-byte IV, 128-bit tag (the provider's defaults). */
    private static final int IV_LEN = 12;
    private static final int TAG_BITS = 128;

    private static void line(String text) {
        Log.i(TAG, text);
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        File stored = new File(getFilesDir(), "ct.bin");
        try {
            if (stored.exists()) {
                restart(stored);
            } else {
                first(stored);
            }
        } catch (Throwable t) {
            line("error " + t);
            Log.e(TAG, "stage failed", t);
        }
        line("done");
        finish();
    }

    private static void first(File stored) throws Exception {
        KeyStore ks = KeyStore.getInstance(PROVIDER);
        ks.load(null);
        ks.deleteEntry(ALIAS);
        KeyGenerator gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER);
        gen.init(new KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setKeySize(256)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build());
        SecretKey key = gen.generateKey();
        Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
        enc.init(Cipher.ENCRYPT_MODE, key);
        byte[] iv = enc.getIV();
        byte[] ct = enc.doFinal(PLAIN);
        line("roundtrip " + Arrays.equals(PLAIN, decrypt(key, iv, ct)));
        KeyInfo info = (KeyInfo) SecretKeyFactory.getInstance(key.getAlgorithm(), PROVIDER).getKeySpec(key, KeyInfo.class);
        line("security-level " + info.getSecurityLevel());
        line("inside-secure-hardware " + info.isInsideSecureHardware());
        byte[] out = new byte[iv.length + ct.length];
        System.arraycopy(iv, 0, out, 0, iv.length);
        System.arraycopy(ct, 0, out, iv.length, ct.length);
        Files.write(stored.toPath(), out);
        try {
            KeyGenerator sb = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER);
            sb.init(new KeyGenParameterSpec.Builder(ALIAS + "-sb", KeyProperties.PURPOSE_ENCRYPT)
                    .setKeySize(256)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setIsStrongBoxBacked(true)
                    .build());
            sb.generateKey();
            line("strongbox generated");
        } catch (StrongBoxUnavailableException e) {
            line("strongbox unavailable");
        }
    }

    private static void restart(File stored) throws Exception {
        KeyStore ks = KeyStore.getInstance(PROVIDER);
        ks.load(null);
        SecretKey key = (SecretKey) ks.getKey(ALIAS, null);
        if (key == null) {
            line("restart key absent");
            return;
        }
        byte[] all = Files.readAllBytes(stored.toPath());
        byte[] iv = Arrays.copyOfRange(all, 0, IV_LEN);
        byte[] ct = Arrays.copyOfRange(all, IV_LEN, all.length);
        line("restart decrypt " + Arrays.equals(PLAIN, decrypt(key, iv, ct)));
        ks.deleteEntry(ALIAS);
    }

    private static byte[] decrypt(SecretKey key, byte[] iv, byte[] ct) throws Exception {
        Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
        dec.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(TAG_BITS, iv));
        return dec.doFinal(ct);
    }
}
