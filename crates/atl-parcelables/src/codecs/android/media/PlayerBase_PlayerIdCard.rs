// SPDX-License-Identifier: MIT
//! android.media.PlayerBase.PlayerIdCard (media/java/android/media/PlayerBase.java writeToParcel:476-481, PlayerIdCard(Parcel):498-504): what a player hands IAudioService.trackPlayer (PlayerBase.baseRegisterPlayer).
//! Written inline, as its writeToParcel does, with no presence flag: the player type, the AudioAttributes (AudioAttributes.rs), the IPlayer binder (writeStrongBinder, null when absent), the session id.
//! The IPlayer is a binder, so the bytes carry an object: the byte-equal check of `xtask test parcel` does not cover it; its own case goes through a Java Parcel (ParcelTruth `PlayerBase.PlayerIdCard:binder`).

use binder::SpIBinder;

use crate::codecs::android::media::AudioAttributes::AudioAttributes;
use crate::parcel::*;
use crate::parts::*;

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct PlayerBase_PlayerIdCard {
    /// AudioPlaybackConfiguration.PLAYER_TYPE_* (e.g. PLAYER_TYPE_JAM_SOUNDPOOL for SoundPool)
    pub player_type: i32,
    pub attributes: AudioAttributes,
    /// The player's IPlayer; None when the sender had none (Java: IPlayer may be null on unmarshal).
    pub player: Option<SpIBinder>,
    pub session_id: i32,
}

impl Part for PlayerBase_PlayerIdCard {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.player_type)?;
        self.attributes.write(p)?;
        w_binder(p, &self.player)?;
        w_i32(p, self.session_id)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(PlayerBase_PlayerIdCard {
            player_type: r_i32(p)?,
            attributes: AudioAttributes::read(p)?,
            player: r_binder(p)?,
            session_id: r_i32(p)?,
        })
    }
}

crate::codec!(PlayerBase_PlayerIdCard);
