use std::sync::Arc;

use pumpkin_config::plugins::PluginsConfig;
use pumpkin_util::text::TextComponent;

use crate::{
    entity::player::Player,
    net::DisconnectReason,
    plugin::{
        Payload,
        api::events::{player::player_join::PlayerJoinEvent, server::packet::PacketReceivedEvent},
    },
    server::Server,
};

const KICK_MESSAGE: &str = "Something went wrong while checking your connection. Please rejoin.";

/// The message shown to a player who cannot join or is removed because the fail-closed plugin
/// `plugin` is not running.
#[must_use]
pub fn unavailable_message(plugin: &str) -> TextComponent {
    TextComponent::text(format!(
        "Joining is paused: the required plugin {plugin} is not running. Please try again later."
    ))
}

/// The first enabled `fail_closed` plugin in `config` for which `running` is false. Joins must be
/// denied while there is one.
#[must_use]
pub fn first_unavailable(config: &PluginsConfig, running: impl Fn(&str) -> bool) -> Option<String> {
    if !config.enabled {
        return None;
    }
    config
        .overrides
        .iter()
        .filter(|(name, o)| o.enabled && o.fail_closed && !running(name))
        .map(|(name, _)| name)
        .min()
        .cloned()
}

/// Disconnects every online player after the fail-closed plugin `plugin` stopped working.
pub fn disconnect_all(server: &Server, plugin: &str) {
    tracing::error!(
        "Fail-closed plugin {plugin} stopped working; disconnecting all players and denying joins until it is loaded again"
    );
    let message = unavailable_message(plugin);
    for player in server.get_all_players() {
        player.kick(DisconnectReason::Kicked, &message);
    }
}

/// Cancels `event` when it is cancellable and returns the player to disconnect when it is a
/// join or received packet, for a handler of a `fail_closed` plugin that failed.
pub fn contain(event: &mut dyn Payload) -> Option<Arc<Player>> {
    if let Some(cancellable) = event.as_cancellable_mut() {
        cancellable.set_cancelled(true);
    }
    player_to_disconnect(event)
}

/// The player behind a join or received packet event.
#[must_use]
pub fn player_to_disconnect(event: &dyn Payload) -> Option<Arc<Player>> {
    if let Some(join) = event.downcast_ref::<PlayerJoinEvent>() {
        return Some(join.player.clone());
    }
    event
        .downcast_ref::<PacketReceivedEvent>()
        .map(|packet| packet.player.clone())
}

/// Disconnects `player` after a failed handler of the plugin `plugin`.
pub fn disconnect(player: &Player, plugin: &str) {
    tracing::warn!(
        "Disconnecting {} because a handler of fail-closed plugin {plugin} failed",
        player.gameprofile.name
    );
    player.kick(DisconnectReason::Kicked, &TextComponent::text(KICK_MESSAGE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{
        Cancellable,
        api::events::server::{
            server_command::ServerCommandEvent,
            server_load::{LoadType, ServerLoadEvent},
        },
    };
    use pumpkin_config::plugins::PluginOverride;

    #[test]
    fn cancellable_events_are_cancelled() {
        let mut event = ServerCommandEvent::new("stop".to_string());
        assert!(contain(&mut event).is_none());
        assert!(event.cancelled());
    }

    #[test]
    fn unavailable_fail_closed_plugins_block_joins() {
        let mut config = PluginsConfig::default();
        let guarded = PluginOverride {
            fail_closed: true,
            ..PluginOverride::default()
        };
        config.overrides.insert("auth".to_string(), guarded.clone());
        config.overrides.insert(
            "disabled_auth".to_string(),
            PluginOverride {
                enabled: false,
                ..guarded
            },
        );
        config
            .overrides
            .insert("other".to_string(), PluginOverride::default());
        assert_eq!(
            first_unavailable(&config, |_| false).as_deref(),
            Some("auth")
        );
        assert_eq!(first_unavailable(&config, |name| name == "auth"), None);
        config.enabled = false;
        assert_eq!(first_unavailable(&config, |_| false), None);
    }

    #[test]
    fn no_fail_closed_plugins_never_block() {
        assert_eq!(
            first_unavailable(&PluginsConfig::default(), |_| false),
            None
        );
    }

    #[test]
    fn other_events_pass_through_untouched() {
        let mut event = ServerLoadEvent::new(LoadType::Startup);
        assert!(event.as_cancellable_mut().is_none());
        assert!(contain(&mut event).is_none());
    }
}
