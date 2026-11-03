// SPDX-License-Identifier: MIT
//! android.content.Intent: content/Intent.java writeToParcel / readFromParcel.
//! The creator-token block exists only while aconfig flag android.security.prevent_intent_redirect is on (`preventIntentRedirect()`): is_fixed_read_only, so framework.jar compiles
//! the release's value in; the codec takes the same value from aconfig's evaluation of the release (crate::aconfig). `xtask test parcel` fails if framework.jar's value differs.
//! extras is a Bundle (Intent.java writeToParcel: `out.writeBundle(mExtras)`), kept as the parcelled Bundle with its binder objects. Not modelled: clipData (present = refused), creator-token nested intent keys (count > 0 = refused).
//! No `PartialEq`: a Bundle with binder objects has no bytes to compare.

use binder::SpIBinder;

use crate::aconfig::PREVENT_INTENT_REDIRECT;
use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub action: Option<String>,
    /// mData as a StringUri's string
    pub data: Option<String>,
    pub r#type: Option<String>,
    pub identifier: Option<String>,
    pub flags: i32,
    pub extended_flags: i32,
    pub package: Option<String>,
    pub component: Option<ComponentName>,
    pub source_bounds: Option<Rect>,
    /// mCategories in ArraySet order; empty = null
    pub categories: Vec<Option<String>>,
    pub selector: Option<Box<Intent>>,
    pub content_user_hint: i32,
    /// mExtras; None = null (an empty Bundle is `Some`: Intent.putExtra creates it on first use)
    pub extras: Option<Bundle>,
    pub original_intent: Option<Box<Intent>>,
    /// mCreatorTokenInfo: Some(creator token) when present
    pub creator_token: Option<Option<SpIBinder>>,
}

impl Part for Intent {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.action)?;
        w_uri(p, &self.data)?;
        w_str8(p, &self.r#type)?;
        w_str8(p, &self.identifier)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.extended_flags)?;
        w_str8(p, &self.package)?;
        w_component(p, &self.component)?;
        // `writeInt(1)` + Rect.writeToParcel, or 0: writeTypedObject's bytes
        w_typed(p, &self.source_bounds)?;
        w_i32(p, len_i32(self.categories.len())?)?;
        self.categories.iter().try_for_each(|c| w_str8(p, c))?;
        w_typed(p, &self.selector.as_deref().cloned())?;
        w_i32(p, 0)?; // clipData
        w_i32(p, self.content_user_hint)?;
        w_bundle(p, &self.extras)?;
        w_typed(p, &self.original_intent.as_deref().cloned())?;
        if PREVENT_INTENT_REDIRECT {
            match &self.creator_token {
                None => w_i32(p, 0)?,
                Some(token) => {
                    w_i32(p, 1)?;
                    w_binder(p, token)?;
                    w_i32(p, 0)?; // mNestedIntentKeys
                }
            }
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let action = r_str8(p)?;
        let data = r_uri(p)?;
        let r#type = r_str8(p)?;
        let identifier = r_str8(p)?;
        let flags = r_i32(p)?;
        let extended_flags = r_i32(p)?;
        let package = r_str8(p)?;
        let component = r_component(p)?;
        let source_bounds = r_typed(p)?;
        let n = r_i32(p)?;
        let categories = (0..n.max(0))
            .map(|_| r_str8(p))
            .collect::<Result<Vec<_>>>()?;
        let selector = r_typed::<Intent>(p)?.map(Box::new);
        if r_present(p)? {
            return Err(crate::refuse("android.content.Intent.clipData"));
        }
        let content_user_hint = r_i32(p)?;
        let extras = r_bundle(p)?;
        let original_intent = r_typed::<Intent>(p)?.map(Box::new);
        let mut creator_token = None;
        if PREVENT_INTENT_REDIRECT && r_present(p)? {
            let token = r_binder(p)?;
            if r_i32(p)? > 0 {
                return Err(crate::refuse(
                    "android.content.Intent.mCreatorTokenInfo.mNestedIntentKeys",
                ));
            }
            creator_token = Some(token);
        }
        Ok(Intent {
            action,
            data,
            r#type,
            identifier,
            flags,
            extended_flags,
            package,
            component,
            source_bounds,
            categories,
            selector,
            content_user_hint,
            extras,
            original_intent,
            creator_token,
        })
    }
}

impl Intent {
    /// Intent.hasFileDescriptors (Intent.java:9360): the extras hold a file descriptor.
    pub fn has_file_descriptors(&self) -> bool {
        self.extras
            .as_ref()
            .is_some_and(Bundle::has_file_descriptors)
    }
}

crate::codec!(Intent);
