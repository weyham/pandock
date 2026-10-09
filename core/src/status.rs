use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionStatus {
    NotConfigured,
    Stopped,
    WaitingForAuthorization,
    Connecting,
    Connected,
    Disconnected { reason: String },
    Error { reason: String },
}

impl ConnectionStatus {
    pub fn cloud_color(&self) -> &'static str {
        match self {
            Self::Connected => "green",
            Self::Connecting | Self::WaitingForAuthorization | Self::Disconnected { .. } => {
                "yellow"
            }
            Self::Error { .. } => "red",
            Self::NotConfigured | Self::Stopped => "gray",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_colors_follow_product_rule() {
        assert_eq!(ConnectionStatus::Connected.cloud_color(), "green");
        assert_eq!(
            ConnectionStatus::WaitingForAuthorization.cloud_color(),
            "yellow"
        );
        assert_eq!(ConnectionStatus::Connecting.cloud_color(), "yellow");
        assert_eq!(
            ConnectionStatus::Disconnected {
                reason: "offline".into()
            }
            .cloud_color(),
            "yellow"
        );
        assert_eq!(
            ConnectionStatus::Error {
                reason: "bad token".into()
            }
            .cloud_color(),
            "red"
        );
        assert_eq!(ConnectionStatus::Stopped.cloud_color(), "gray");
    }
}
