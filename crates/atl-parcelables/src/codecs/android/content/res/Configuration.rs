// SPDX-License-Identifier: MIT
//! android.content.res.Configuration: content/res/Configuration.java writeToParcel / readFromParcel; the LocaleList as writeTypedObject of os/LocaleList.java (its language tags, a String8).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Configuration {
    pub font_scale: f32,
    pub mcc: i32,
    pub mnc: i32,
    /// mLocaleList's language tags ("en-US,fr"); None = a null LocaleList
    pub locale_list: Option<String>,
    pub user_set_locale: bool,
    pub touchscreen: i32,
    pub keyboard: i32,
    pub keyboard_hidden: i32,
    pub hard_keyboard_hidden: i32,
    pub navigation: i32,
    pub navigation_hidden: i32,
    pub orientation: i32,
    pub screen_layout: i32,
    pub color_mode: i32,
    pub ui_mode: i32,
    pub screen_width_dp: i32,
    pub screen_height_dp: i32,
    pub smallest_screen_width_dp: i32,
    pub density_dpi: i32,
    pub compat_screen_width_dp: i32,
    pub compat_screen_height_dp: i32,
    pub compat_smallest_screen_width_dp: i32,
    pub window_configuration: WindowConfiguration,
    pub assets_seq: i32,
    pub seq: i32,
    pub font_weight_adjustment: i32,
    pub grammatical_gender: i32,
}

impl Part for Configuration {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_f32(p, self.font_scale)?;
        w_i32(p, self.mcc)?;
        w_i32(p, self.mnc)?;
        w_present(p, self.locale_list.is_some())?;
        if self.locale_list.is_some() {
            w_str8(p, &self.locale_list)?;
        }
        w_bool(p, self.user_set_locale)?;
        w_i32(p, self.touchscreen)?;
        w_i32(p, self.keyboard)?;
        w_i32(p, self.keyboard_hidden)?;
        w_i32(p, self.hard_keyboard_hidden)?;
        w_i32(p, self.navigation)?;
        w_i32(p, self.navigation_hidden)?;
        w_i32(p, self.orientation)?;
        w_i32(p, self.screen_layout)?;
        w_i32(p, self.color_mode)?;
        w_i32(p, self.ui_mode)?;
        w_i32(p, self.screen_width_dp)?;
        w_i32(p, self.screen_height_dp)?;
        w_i32(p, self.smallest_screen_width_dp)?;
        w_i32(p, self.density_dpi)?;
        w_i32(p, self.compat_screen_width_dp)?;
        w_i32(p, self.compat_screen_height_dp)?;
        w_i32(p, self.compat_smallest_screen_width_dp)?;
        self.window_configuration.write(p)?;
        w_i32(p, self.assets_seq)?;
        w_i32(p, self.seq)?;
        w_i32(p, self.font_weight_adjustment)?;
        w_i32(p, self.grammatical_gender)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let font_scale = r_f32(p)?;
        let mcc = r_i32(p)?;
        let mnc = r_i32(p)?;
        let locale_list = if r_present(p)? {
            // LocaleList.forLanguageTags(null) would throw: the tags are never null
            Some(r_str8(p)?.ok_or(binder::StatusCode::UNEXPECTED_NULL)?)
        } else {
            None
        };
        Ok(Configuration {
            font_scale,
            mcc,
            mnc,
            locale_list,
            // readFromParcel: `readInt() == 1`
            user_set_locale: r_i32(p)? == 1,
            touchscreen: r_i32(p)?,
            keyboard: r_i32(p)?,
            keyboard_hidden: r_i32(p)?,
            hard_keyboard_hidden: r_i32(p)?,
            navigation: r_i32(p)?,
            navigation_hidden: r_i32(p)?,
            orientation: r_i32(p)?,
            screen_layout: r_i32(p)?,
            color_mode: r_i32(p)?,
            ui_mode: r_i32(p)?,
            screen_width_dp: r_i32(p)?,
            screen_height_dp: r_i32(p)?,
            smallest_screen_width_dp: r_i32(p)?,
            density_dpi: r_i32(p)?,
            compat_screen_width_dp: r_i32(p)?,
            compat_screen_height_dp: r_i32(p)?,
            compat_smallest_screen_width_dp: r_i32(p)?,
            window_configuration: WindowConfiguration::read(p)?,
            assets_seq: r_i32(p)?,
            seq: r_i32(p)?,
            font_weight_adjustment: r_i32(p)?,
            grammatical_gender: r_i32(p)?,
        })
    }
}

crate::codec!(Configuration);
