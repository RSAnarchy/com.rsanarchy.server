//! `CommonPlayerSpawnInfo` for a guest: the login, respawn and travel packets share it.

use super::Hub;
use horizon_protocol::packets::play;

impl Hub {
    /// `CommonPlayerSpawnInfo`, with the player's `portalCooldown`.
    pub(super) fn spawn_info(
        &self,
        i: usize,
        portal_cooldown: i32,
        sea_level: i32,
    ) -> play::SpawnInfo {
        let g = &self.guests[i];
        let (dim, dim_type) = if g.dimension == "horizon:login" {
            (
                "horizon:login".to_owned(),
                self.sync
                    .id("minecraft:dimension_type", "minecraft:the_end")
                    .unwrap_or(0),
            )
        } else {
            (self.config.dimension.clone(), self.dimension_type)
        };
        play::SpawnInfo {
            dimension_type: dim_type,
            dimension: dim,
            seed: horizon_core::biome::obfuscate_seed(self.config.seed),
            game_type: g.game_mode,
            previous_game_type: None,
            debug: self.debug_world,
            flat: self.flat_world,
            // Player.getLastDeathLocation.
            last_death: g
                .last_death
                .as_ref()
                .map(|(dimension, pos)| play::GlobalPos {
                    dimension: dimension.clone(),
                    pos: *pos,
                }),
            portal_cooldown,
            sea_level,
        }
    }
}
