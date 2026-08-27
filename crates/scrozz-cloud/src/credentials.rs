//! Credential sources, ordered without ever putting a secret on argv.

use std::{
    ffi::OsString,
    fmt,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::{
    error::{Error, Result},
    redact::Secret,
};

/// A read-only environment source, abstracted so precedence is testable.
pub trait Environment {
    /// Reads one variable.
    fn get(&self, key: &str) -> Option<String>;
}

/// The current process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// An access key, secret key and optional temporary session token.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: Secret,
    session_token: Option<Secret>,
}

impl Credentials {
    /// Builds and validates a credential set.
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: Secret,
        session_token: Option<Secret>,
    ) -> Result<Self> {
        let access_key_id = access_key_id.into();
        if access_key_id.trim().is_empty()
            || !access_key_id
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b',' | b'='))
            || secret_access_key.is_empty()
        {
            return Err(Error::Credentials(
                "a valid ASCII access-key id and nonempty secret access key are both required"
                    .to_owned(),
            ));
        }
        if session_token.as_ref().is_some_and(|token| {
            token.is_empty()
                || token
                    .expose_text()
                    .is_none_or(|value| value.bytes().any(|byte| byte.is_ascii_control()))
        }) {
            return Err(Error::Credentials(
                "the session token must be nonempty UTF-8".to_owned(),
            ));
        }
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }

    /// Access-key id used in the SigV4 credential scope.
    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// Secret access-key bytes used by HMAC.
    #[must_use]
    pub fn secret_access_key(&self) -> &[u8] {
        self.secret_access_key.expose()
    }

    /// Temporary credential token, when present.
    #[must_use]
    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_ref().and_then(Secret::expose_text)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// A program whose stdout is the secret access key.
#[derive(Clone)]
pub struct CredentialCommand {
    /// Executable path or name. No shell is involved.
    pub program: PathBuf,
    /// Arguments such as a password-manager item name. Never the secret itself.
    pub args: Vec<OsString>,
    /// Access-key id paired with the command's secret, when the environment does
    /// not supply one.
    pub access_key_id: Option<String>,
}

impl fmt::Debug for CredentialCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialCommand")
            .field("program", &self.program)
            .field("args", &"[REDACTED]")
            .field(
                "access_key_id",
                &self.access_key_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Where the selected credentials came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialOrigin {
    /// `SCROZZ_S3_*`, with `AWS_*` fallbacks.
    Environment,
    /// A configured command supplied the secret on stdout.
    Command,
    /// An in-memory value, normally read from stdin.
    Explicit,
}

/// Credentials plus their non-secret provenance.
pub struct ResolvedCredentials {
    /// Selected credentials.
    pub credentials: Credentials,
    /// Selected source.
    pub origin: CredentialOrigin,
}

impl fmt::Debug for ResolvedCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredentials")
            .field("credentials", &self.credentials)
            .field("origin", &self.origin)
            .finish()
    }
}

/// Executes a credential command. Public so a platform store can provide its
/// own adapter without changing resolution policy.
pub trait CredentialCommandRunner {
    /// Returns only the command's secret stdout.
    fn secret(&self, command: &CredentialCommand) -> Result<Secret>;
}

#[derive(Debug, Clone, Copy, Default)]
struct SystemCommandRunner;

impl CredentialCommandRunner for SystemCommandRunner {
    fn secret(&self, command: &CredentialCommand) -> Result<Secret> {
        const TIMEOUT: Duration = Duration::from_secs(30);
        const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

        let mut child = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                Error::Credentials(format!(
                    "the credential command could not be started: {}",
                    error.kind()
                ))
            })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Credentials(
                "the credential command stdout pipe could not be opened".to_owned(),
            ));
        };
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("scrozz-credential-output".to_owned())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes);
                if let Err(error) = output_tx.send((result, bytes)) {
                    let (_, mut bytes) = error.0;
                    bytes.fill(0);
                }
            }) {
            Ok(reader) => drop(reader),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Credentials(format!(
                    "the credential command output reader could not start: {}",
                    error.kind()
                )));
            }
        }
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    scrub_available_output(&output_rx);
                    return Err(Error::Credentials(
                        "the credential command exceeded its 30-second timeout".to_owned(),
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    scrub_available_output(&output_rx);
                    return Err(Error::Credentials(format!(
                        "the credential command status could not be read: {}",
                        error.kind()
                    )));
                }
            }
        };
        let remaining = TIMEOUT.saturating_sub(started.elapsed());
        let (read_result, mut bytes) = output_rx.recv_timeout(remaining).map_err(|error| {
            let reason = match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    "the credential command stdout remained open past its 30-second timeout"
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    "the credential command output reader stopped unexpectedly"
                }
            };
            Error::Credentials(reason.to_owned())
        })?;
        if let Err(error) = read_result {
            bytes.fill(0);
            return Err(Error::Credentials(format!(
                "the credential command output could not be read: {}",
                error.kind()
            )));
        }

        fn scrub_available_output(
            output: &std::sync::mpsc::Receiver<(std::io::Result<usize>, Vec<u8>)>,
        ) {
            if let Ok((_, mut bytes)) = output.try_recv() {
                bytes.fill(0);
            }
        }
        if !status.success() {
            bytes.fill(0);
            return Err(Error::Credentials(format!(
                "the credential command exited with status {}",
                status
            )));
        }
        if bytes.len() > MAX_OUTPUT_BYTES as usize {
            bytes.fill(0);
            return Err(Error::Credentials(
                "the credential command output exceeded 64 KiB".to_owned(),
            ));
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
            bytes.fill(0);
            return Err(Error::Credentials(
                "the credential command must print exactly one nonempty secret line".to_owned(),
            ));
        }
        Ok(Secret::new(bytes))
    }
}

/// Resolves credentials in the fixed order environment → command → explicit.
pub struct CredentialResolver<'a> {
    environment: &'a dyn Environment,
    runner: &'a dyn CredentialCommandRunner,
}

impl<'a> CredentialResolver<'a> {
    /// Uses the process command runner.
    #[must_use]
    pub fn new(environment: &'a dyn Environment) -> Self {
        static RUNNER: SystemCommandRunner = SystemCommandRunner;
        Self {
            environment,
            runner: &RUNNER,
        }
    }

    /// Uses an injected runner, principally for deterministic tests and native
    /// secret-store integrations.
    #[must_use]
    pub fn with_runner(
        environment: &'a dyn Environment,
        runner: &'a dyn CredentialCommandRunner,
    ) -> Self {
        Self {
            environment,
            runner,
        }
    }

    /// Resolves one complete set. Partial higher-priority sources may provide
    /// the access-key id or session token to a command, but never override a
    /// complete lower source with an incomplete credential.
    pub fn resolve(
        &self,
        command: Option<&CredentialCommand>,
        explicit: Option<Credentials>,
    ) -> Result<ResolvedCredentials> {
        self.resolve_lazy(command, || Ok(explicit))
    }

    /// Resolves credentials without reading the explicit fallback unless both
    /// higher-priority sources are unavailable.
    pub fn resolve_lazy<F>(
        &self,
        command: Option<&CredentialCommand>,
        explicit: F,
    ) -> Result<ResolvedCredentials>
    where
        F: FnOnce() -> Result<Option<Credentials>>,
    {
        let mut scrozz_access = nonempty(self.environment.get("SCROZZ_S3_ACCESS_KEY_ID"));
        let mut scrozz_secret = self
            .environment
            .get("SCROZZ_S3_SECRET_ACCESS_KEY")
            .filter(|value| !value.is_empty())
            .map(Secret::from_text);
        let mut scrozz_token = self
            .environment
            .get("SCROZZ_S3_SESSION_TOKEN")
            .filter(|value| !value.is_empty())
            .map(Secret::from_text);
        if scrozz_access
            .as_ref()
            .is_some_and(|value| !value.is_empty())
            && scrozz_secret
                .as_ref()
                .is_some_and(|value| !value.is_empty())
        {
            return Ok(ResolvedCredentials {
                credentials: Credentials::new(
                    scrozz_access.take().expect("checked above"),
                    scrozz_secret.take().expect("checked above"),
                    scrozz_token.take(),
                )?,
                origin: CredentialOrigin::Environment,
            });
        }

        let mut aws_access = nonempty(self.environment.get("AWS_ACCESS_KEY_ID"));
        let mut aws_secret = self
            .environment
            .get("AWS_SECRET_ACCESS_KEY")
            .filter(|value| !value.is_empty())
            .map(Secret::from_text);
        let mut aws_token = self
            .environment
            .get("AWS_SESSION_TOKEN")
            .filter(|value| !value.is_empty())
            .map(Secret::from_text);
        if aws_access.as_ref().is_some_and(|value| !value.is_empty())
            && aws_secret.as_ref().is_some_and(|value| !value.is_empty())
        {
            return Ok(ResolvedCredentials {
                credentials: Credentials::new(
                    aws_access.take().expect("checked above"),
                    aws_secret.take().expect("checked above"),
                    aws_token.take(),
                )?,
                origin: CredentialOrigin::Environment,
            });
        }

        if let Some(command) = command {
            let (access, token) = if let Some(access) = nonempty(scrozz_access) {
                (access, nonempty_secret(scrozz_token))
            } else if let Some(access) = nonempty(aws_access) {
                (access, nonempty_secret(aws_token))
            } else if let Some(access) = command.access_key_id.clone() {
                (access, None)
            } else {
                return Err(Error::Credentials(
                    "the credential command supplies only the secret; configure its \
                         access-key id with SCROZZ_S3_ACCESS_KEY_ID or AWS_ACCESS_KEY_ID"
                        .to_owned(),
                ));
            };
            let secret = self.runner.secret(command)?;
            return Ok(ResolvedCredentials {
                credentials: Credentials::new(access, secret, token)?,
                origin: CredentialOrigin::Command,
            });
        }

        if let Some(credentials) = explicit()? {
            return Ok(ResolvedCredentials {
                credentials,
                origin: CredentialOrigin::Explicit,
            });
        }

        Err(Error::Credentials(
            "set SCROZZ_S3_ACCESS_KEY_ID and SCROZZ_S3_SECRET_ACCESS_KEY, configure a \
             credential command, or pass --secret-key-stdin with an access-key id in the environment"
                .to_owned(),
        ))
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn nonempty_secret(value: Option<Secret>) -> Option<Secret> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct MapEnvironment(BTreeMap<String, String>);

    impl Environment for MapEnvironment {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    struct FakeRunner(&'static str);

    impl CredentialCommandRunner for FakeRunner {
        fn secret(&self, _command: &CredentialCommand) -> Result<Secret> {
            Ok(Secret::from_text(self.0))
        }
    }

    fn credentials(id: &str, secret: &str) -> Credentials {
        Credentials::new(id, Secret::from_text(secret), None).unwrap()
    }

    #[test]
    fn precedence_is_environment_then_command_then_explicit() {
        let mut env = MapEnvironment::default();
        env.0
            .insert("SCROZZ_S3_ACCESS_KEY_ID".into(), "environment-id".into());
        env.0.insert(
            "SCROZZ_S3_SECRET_ACCESS_KEY".into(),
            "environment-secret".into(),
        );
        let command = CredentialCommand {
            program: "ignored".into(),
            args: Vec::new(),
            access_key_id: Some("command-id".into()),
        };
        let runner = FakeRunner("command-secret");
        let resolved = CredentialResolver::with_runner(&env, &runner)
            .resolve(
                Some(&command),
                Some(credentials("explicit-id", "explicit-secret")),
            )
            .unwrap();
        assert_eq!(resolved.origin, CredentialOrigin::Environment);
        assert_eq!(resolved.credentials.access_key_id(), "environment-id");

        env.0.remove("SCROZZ_S3_SECRET_ACCESS_KEY");
        let resolved = CredentialResolver::with_runner(&env, &runner)
            .resolve(
                Some(&command),
                Some(credentials("explicit-id", "explicit-secret")),
            )
            .unwrap();
        assert_eq!(resolved.origin, CredentialOrigin::Command);
        assert_eq!(resolved.credentials.secret_access_key(), b"command-secret");

        let resolved = CredentialResolver::with_runner(&MapEnvironment::default(), &runner)
            .resolve(None, Some(credentials("explicit-id", "explicit-secret")))
            .unwrap();
        assert_eq!(resolved.origin, CredentialOrigin::Explicit);
    }

    #[test]
    fn environment_namespaces_are_complete_tuples_not_mixed_credentials() {
        let mut env = MapEnvironment::default();
        env.0
            .insert("SCROZZ_S3_ACCESS_KEY_ID".into(), "command-id".into());
        env.0
            .insert("AWS_SECRET_ACCESS_KEY".into(), "ambient-secret".into());
        let command = CredentialCommand {
            program: "ignored".into(),
            args: Vec::new(),
            access_key_id: None,
        };
        let resolved = CredentialResolver::with_runner(&env, &FakeRunner("command-secret"))
            .resolve(Some(&command), None)
            .unwrap();
        assert_eq!(resolved.origin, CredentialOrigin::Command);
        assert_eq!(resolved.credentials.access_key_id(), "command-id");
        assert_eq!(resolved.credentials.secret_access_key(), b"command-secret");
    }

    #[test]
    fn explicit_fallback_is_lazy() {
        let mut env = MapEnvironment::default();
        env.0
            .insert("SCROZZ_S3_ACCESS_KEY_ID".into(), "environment-id".into());
        env.0.insert(
            "SCROZZ_S3_SECRET_ACCESS_KEY".into(),
            "environment-secret".into(),
        );
        let resolved = CredentialResolver::new(&env)
            .resolve_lazy(None, || panic!("explicit credentials should not be read"))
            .unwrap();
        assert_eq!(resolved.origin, CredentialOrigin::Environment);
    }

    #[test]
    fn credential_debugging_is_redacted() {
        let value = credentials("visible-id", "never-print-this");
        let rendered = format!("{value:?}");
        assert!(!rendered.contains("visible-id"));
        assert!(!rendered.contains("never-print-this"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn header_delimiters_and_controls_are_rejected_in_credentials() {
        assert!(Credentials::new("bad/id", Secret::from_text("secret"), None).is_err());
        assert!(
            Credentials::new(
                "access",
                Secret::from_text("secret"),
                Some(Secret::from_text("token\ninjection")),
            )
            .is_err()
        );
    }
}
