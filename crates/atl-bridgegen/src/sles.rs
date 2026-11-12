// SPDX-License-Identifier: MIT
//! OpenSL ES interface tables (opensl-es plan 2.2 item 1): each `struct SL<X>Itf_` of `OpenSLES.h` / `OpenSLES_Android.h` is one proxy object table, its slots the struct's function fields in order, as `JNINativeInterface` is for a guest `JNIEnv`. An SL interface is `const struct SL<X>Itf_ * const *`, so a guest proxy (word 0 its table, word 1 the host itf) is called through the same object stubs as a guest env.
//! The interface ID of a table is `SL_IID_<X>` with `<X>` upper-cased (`SLAndroidSimpleBufferQueueItf_` -> `SL_IID_ANDROIDSIMPLEBUFFERQUEUE`), the naming both headers follow; a table whose ID the headers do not declare has no IID row and is reached only as an out-param of a known call.

use anyhow::{Result, ensure};

use crate::ir::{ArchView, Ty};

/// Prefix and suffix of an SL interface record.
const PREFIX: &str = "SL";
const SUFFIX: &str = "Itf_";

/// The SL interface records of `view`, sorted by name: every `SL<X>Itf_` whose fields are all pointer-sized function pointers at slot * pointer size. A record of that name with any other field fails, naming it: the object stubs index slots by word.
pub fn tables(view: &ArchView) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (name, rec) in &view.records {
        if stem(name).is_none() {
            continue;
        }
        let word = rec.align;
        ensure!(
            rec.size == word * rec.fields.len() as u64,
            "{name}: size {} is not {} slots of {word} bytes",
            rec.size,
            rec.fields.len()
        );
        for (slot, f) in rec.fields.iter().enumerate() {
            let is_fn = matches!(f.ty.resolved(), Ty::Pointer { pointee, bits, .. } if u64::from(*bits) == word * 8 && matches!(pointee.resolved(), Ty::Function(_)));
            ensure!(
                f.offset == slot as u64 * word && is_fn,
                "{name}.{}: not a function pointer at slot {slot}",
                f.name
            );
        }
        out.push(name.clone());
    }
    out.sort();
    Ok(out)
}

/// `<X>` of `SL<X>Itf_`, never empty.
fn stem(record: &str) -> Option<&str> {
    record
        .strip_prefix(PREFIX)?
        .strip_suffix(SUFFIX)
        .filter(|s| !s.is_empty())
}

/// The interface-ID symbol of SL record `record` (`SLPlayItf_` -> `SL_IID_PLAY`).
pub fn iid(record: &str) -> Option<String> {
    Some(format!("SL_IID_{}", stem(record)?.to_ascii_uppercase()))
}

/// The Rust module name of SL record `record` (`SLPlayItf_` -> `sl_play`): snake case of the stem, so generated modules sit beside `env` / `vm` without lint exceptions.
pub fn module(record: &str) -> Option<String> {
    let stem = stem(record)?;
    let mut m = String::from("sl");
    let chars: Vec<char> = stem.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        // a new word: an upper-case letter after a lower-case one, or the last capital of an acronym run (`LEDArray` -> `led_array`); a digit run stays with the capitals after it (`3DGroup` -> `3d_group`)
        let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
        let acronym_end = i > 0
            && chars[i - 1].is_ascii_uppercase()
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
        if c.is_ascii_uppercase() && (i == 0 || prev_lower || acronym_end) || i == 0 {
            m.push('_');
        }
        m.push(c.to_ascii_lowercase());
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names as OpenSLES.h / OpenSLES_Android.h spell them; the IIDs are the headers' own `SL_IID_*` declarations.
    #[test]
    fn names() {
        assert_eq!(iid("SLPlayItf_").as_deref(), Some("SL_IID_PLAY"));
        assert_eq!(
            iid("SLAndroidSimpleBufferQueueItf_").as_deref(),
            Some("SL_IID_ANDROIDSIMPLEBUFFERQUEUE")
        );
        assert_eq!(iid("SLItf_"), None);
        assert_eq!(module("SLPlayItf_").as_deref(), Some("sl_play"));
        assert_eq!(
            module("SLAndroidSimpleBufferQueueItf_").as_deref(),
            Some("sl_android_simple_buffer_queue")
        );
        assert_eq!(
            module("SL3DGroupingItf_").as_deref(),
            Some("sl_3d_grouping")
        );
        assert_eq!(module("SLLEDArrayItf_").as_deref(), Some("sl_led_array"));
    }
}
