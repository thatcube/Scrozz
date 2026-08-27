use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, HttpsUrl, Result};

/// A release stream selected by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    /// Production releases intended for general use.
    Stable,
    /// Opt-in preview releases that may change more frequently.
    Preview,
}

impl UpdateChannel {
    /// Returns the stable settings and command-line token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }
}

impl fmt::Display for UpdateChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for UpdateChannel {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "stable" => Ok(Self::Stable),
            "preview" => Ok(Self::Preview),
            _ => Err(Error::InvalidUpdateChannel(value.to_owned())),
        }
    }
}

/// Validated manifest and detached-signature locations for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateEndpoints {
    manifest: HttpsUrl,
    signature: HttpsUrl,
}

impl UpdateEndpoints {
    /// Validates the two HTTPS endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUrl`] when either endpoint is not an absolute
    /// HTTPS URL accepted by the fetch transport.
    pub fn new(manifest: impl Into<String>, signature: impl Into<String>) -> Result<Self> {
        Ok(Self {
            manifest: HttpsUrl::parse(manifest)?,
            signature: HttpsUrl::parse(signature)?,
        })
    }

    /// Returns the exact manifest URL.
    #[must_use]
    pub fn manifest(&self) -> &HttpsUrl {
        &self.manifest
    }

    /// Returns the exact detached-signature URL.
    #[must_use]
    pub fn signature(&self) -> &HttpsUrl {
        &self.signature
    }
}

/// One channel paired with its validated endpoint set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChannel {
    channel: UpdateChannel,
    endpoints: UpdateEndpoints,
}

impl ResolvedChannel {
    /// Returns the selected release channel.
    #[must_use]
    pub const fn channel(&self) -> UpdateChannel {
        self.channel
    }

    /// Returns its validated manifest and signature endpoints.
    #[must_use]
    pub const fn endpoints(&self) -> &UpdateEndpoints {
        &self.endpoints
    }
}

/// Endpoint availability for one selected channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEndpointStatus {
    /// Both endpoints are configured and validated.
    Available(ResolvedChannel),
    /// No trusted production endpoint has been configured for this channel.
    Disabled {
        /// The unavailable channel.
        channel: UpdateChannel,
        /// A stable human-readable explanation.
        reason: &'static str,
    },
}

impl ChannelEndpointStatus {
    /// Returns the channel represented by this status.
    #[must_use]
    pub const fn channel(&self) -> UpdateChannel {
        match self {
            Self::Available(resolved) => resolved.channel(),
            Self::Disabled { channel, .. } => *channel,
        }
    }

    /// Returns configured endpoints, if available.
    #[must_use]
    pub const fn endpoints(&self) -> Option<&UpdateEndpoints> {
        match self {
            Self::Available(resolved) => Some(resolved.endpoints()),
            Self::Disabled { .. } => None,
        }
    }
}

/// Trusted endpoint catalog used to resolve update channels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointCatalog {
    stable: Option<UpdateEndpoints>,
    preview: Option<UpdateEndpoints>,
}

impl EndpointCatalog {
    /// Returns the intentionally empty production catalog.
    ///
    /// Production locations stay disabled until a human deliberately supplies
    /// the final release endpoints in source.
    #[must_use]
    pub const fn production() -> Self {
        Self {
            stable: None,
            preview: None,
        }
    }

    /// Creates an injectable catalog for tests and downstream distributors.
    #[must_use]
    pub const fn new(stable: Option<UpdateEndpoints>, preview: Option<UpdateEndpoints>) -> Self {
        Self { stable, preview }
    }

    /// Reports whether the selected channel has configured endpoints.
    #[must_use]
    pub fn status(&self, channel: UpdateChannel) -> ChannelEndpointStatus {
        let endpoints = match channel {
            UpdateChannel::Stable => self.stable.as_ref(),
            UpdateChannel::Preview => self.preview.as_ref(),
        };
        match endpoints {
            Some(endpoints) => ChannelEndpointStatus::Available(ResolvedChannel {
                channel,
                endpoints: endpoints.clone(),
            }),
            None => ChannelEndpointStatus::Disabled {
                channel,
                reason: "no trusted production endpoint is configured for this channel",
            },
        }
    }

    /// Resolves a channel to validated endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UpdateChannelUnavailable`] when production endpoint
    /// configuration for the selected channel is absent.
    pub fn resolve(&self, channel: UpdateChannel) -> Result<ResolvedChannel> {
        let endpoints = match channel {
            UpdateChannel::Stable => self.stable.as_ref(),
            UpdateChannel::Preview => self.preview.as_ref(),
        }
        .ok_or(Error::UpdateChannelUnavailable(channel))?;
        Ok(ResolvedChannel {
            channel,
            endpoints: endpoints.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints(label: &str) -> UpdateEndpoints {
        UpdateEndpoints::new(
            format!("https://updates.example.test/{label}/manifest.json"),
            format!("https://updates.example.test/{label}/manifest.sig"),
        )
        .unwrap()
    }

    #[test]
    fn channels_use_stable_wire_tokens() {
        for (text, expected) in [
            ("stable", UpdateChannel::Stable),
            ("preview", UpdateChannel::Preview),
        ] {
            assert_eq!(text.parse::<UpdateChannel>().unwrap(), expected);
            assert_eq!(expected.to_string(), text);
            assert_eq!(
                serde_json::from_str::<UpdateChannel>(&format!("\"{text}\"")).unwrap(),
                expected
            );
        }
        assert!(matches!(
            "nightly".parse::<UpdateChannel>(),
            Err(Error::InvalidUpdateChannel(channel)) if channel == "nightly"
        ));
    }

    #[test]
    fn endpoint_catalog_resolves_only_configured_channels() {
        let stable = endpoints("stable");
        let catalog = EndpointCatalog::new(Some(stable.clone()), None);

        let resolved = catalog.resolve(UpdateChannel::Stable).unwrap();
        assert_eq!(resolved.channel(), UpdateChannel::Stable);
        assert_eq!(resolved.endpoints(), &stable);
        assert_eq!(
            catalog.status(UpdateChannel::Stable).channel(),
            UpdateChannel::Stable
        );
        assert!(matches!(
            catalog.status(UpdateChannel::Preview),
            ChannelEndpointStatus::Disabled {
                channel: UpdateChannel::Preview,
                ..
            }
        ));
        assert!(matches!(
            catalog.resolve(UpdateChannel::Preview),
            Err(Error::UpdateChannelUnavailable(UpdateChannel::Preview))
        ));
    }

    #[test]
    fn endpoints_reject_non_https_locations() {
        assert!(matches!(
            UpdateEndpoints::new(
                "http://updates.example.test/manifest.json",
                "https://updates.example.test/manifest.sig"
            ),
            Err(Error::InvalidUrl(_))
        ));
    }
}
