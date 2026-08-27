//! Private sharing through storage the user already controls.
//!
//! The default build contains signing, encryption and request construction but
//! no network client. Enable `network` to add the small blocking transport used
//! by the application. There is no Scrozz endpoint, account or service.

#![forbid(unsafe_code)]

pub mod bundle;
pub mod config;
pub mod credentials;
pub mod digest;
pub mod encoding;
pub mod error;
pub mod lifecycle;
pub mod provider;
pub mod redact;
pub mod share;
pub mod sigv4;
pub mod transport;

pub use bundle::{EncryptedPayload, encrypt, render_viewer};
pub use config::{Branding, ConfigOverrides, ShareConfig};
pub use credentials::{
    CredentialCommand, CredentialOrigin, CredentialResolver, Credentials, Environment,
    ProcessEnvironment, ResolvedCredentials,
};
pub use error::{Error, Result};
pub use lifecycle::{
    EXPIRY_PREFIX, EXPIRY_TAG, Expiry, ObjectTag, expiry_prefix, lifecycle_prefix_rule_xml,
    lifecycle_rule_xml, lifecycle_versioned_prefix_rule_xml,
};
pub use provider::{AddressingStyle, ObjectTarget, ProviderConfig, ProviderKind};
pub use redact::Secret;
pub use share::{
    ExpiryPolicy, ShareClient, ShareInput, ShareOptions, ShareResult, client_from_environment,
    client_from_environment_lazy, unique_object_key,
};
#[cfg(feature = "network")]
pub use transport::UreqTransport;
pub use transport::{CancellationToken, HttpRequest, HttpResponse, RetryPolicy, Transport};
