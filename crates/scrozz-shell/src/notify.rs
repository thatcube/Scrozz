//! Native notification command plans.
//!
//! User text is passed as an argument or environment value. No shell parses it.

use scrozz_core::{Error, Result, identity::PRODUCT_NAME};

use crate::{CommandPlan, SystemPlatform};

/// One user-visible desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Short heading.
    pub title: String,
    /// Explanatory body.
    pub body: String,
}

impl Notification {
    /// Creates a non-empty notification.
    ///
    /// # Errors
    ///
    /// Rejects empty, control-character-bearing, or unreasonably large text.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Result<Self> {
        let notification = Self {
            title: title.into(),
            body: body.into(),
        };
        for (name, value, limit) in [
            ("title", notification.title.as_str(), 160),
            ("body", notification.body.as_str(), 4_096),
        ] {
            if value.trim().is_empty()
                || value.len() > limit
                || value
                    .chars()
                    .any(|character| character.is_control() && character != '\n')
            {
                return Err(Error::InvalidRequest(format!(
                    "notification {name} is empty, oversized, or contains control characters"
                )));
            }
        }
        Ok(notification)
    }

    /// Computes a host-independent platform plan.
    #[must_use]
    pub fn plan(&self, platform: SystemPlatform) -> NotificationPlan {
        NotificationPlan::for_platform(platform, self)
    }
}

/// The exact subprocess used to show a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationPlan {
    platform: SystemPlatform,
    command: CommandPlan,
}

impl NotificationPlan {
    /// Computes a plan without executing it.
    #[must_use]
    pub fn for_platform(platform: SystemPlatform, notification: &Notification) -> Self {
        let command = match platform {
            SystemPlatform::MacOS => CommandPlan::new(
                "/usr/bin/osascript",
                [
                    "-e",
                    concat!(
                        "display notification ",
                        "(system attribute \"SCROZZ_NOTIFICATION_BODY\") ",
                        "with title ",
                        "(system attribute \"SCROZZ_NOTIFICATION_TITLE\")"
                    ),
                ],
            )
            .with_env("SCROZZ_NOTIFICATION_TITLE", &notification.title)
            .with_env("SCROZZ_NOTIFICATION_BODY", &notification.body),
            SystemPlatform::Windows => CommandPlan::new(
                "powershell.exe",
                [
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    concat!(
                        "$ErrorActionPreference='Stop';",
                        "[Windows.UI.Notifications.ToastNotificationManager,",
                        "Windows.UI.Notifications,ContentType=WindowsRuntime] > $null;",
                        "[Windows.Data.Xml.Dom.XmlDocument,Windows.Data.Xml.Dom.XmlDocument,",
                        "ContentType=WindowsRuntime] > $null;",
                        "$t=[Security.SecurityElement]::Escape($env:SCROZZ_NOTIFICATION_TITLE);",
                        "$b=[Security.SecurityElement]::Escape($env:SCROZZ_NOTIFICATION_BODY);",
                        "$x=New-Object Windows.Data.Xml.Dom.XmlDocument;",
                        "$x.LoadXml(\"<toast><visual><binding template='ToastGeneric'>",
                        "<text>$t</text><text>$b</text></binding></visual></toast>\");",
                        "$n=[Windows.UI.Notifications.ToastNotification]::new($x);",
                        "[Windows.UI.Notifications.ToastNotificationManager]::",
                        "CreateToastNotifier('Scrozz').Show($n)"
                    ),
                ],
            )
            .with_env("SCROZZ_NOTIFICATION_TITLE", &notification.title)
            .with_env("SCROZZ_NOTIFICATION_BODY", &notification.body),
            SystemPlatform::Linux => CommandPlan::new(
                "notify-send",
                [
                    "--app-name",
                    PRODUCT_NAME,
                    "--",
                    notification.title.as_str(),
                    notification.body.as_str(),
                ],
            ),
        };
        Self { platform, command }
    }

    /// The platform this plan targets.
    #[must_use]
    pub const fn platform(&self) -> SystemPlatform {
        self.platform
    }

    /// The exact command.
    #[must_use]
    pub fn command(&self) -> &CommandPlan {
        &self.command
    }

    /// Shows the notification.
    ///
    /// # Errors
    ///
    /// Returns a process or platform failure.
    pub fn apply(&self) -> Result<()> {
        self.command.apply("show notification")
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use super::*;

    #[test]
    fn plans_are_auditable_for_all_three_platforms() {
        let notification = Notification::new("Update ready", "Restart when convenient.").unwrap();
        let macos = notification.plan(SystemPlatform::MacOS);
        let windows = notification.plan(SystemPlatform::Windows);
        let linux = notification.plan(SystemPlatform::Linux);

        assert_eq!(macos.command().program(), Path::new("/usr/bin/osascript"));
        assert_eq!(macos.command().env().len(), 2);
        assert_eq!(windows.command().program(), Path::new("powershell.exe"));
        assert_eq!(windows.command().env().len(), 2);
        assert_eq!(linux.command().program(), Path::new("notify-send"));
    }

    #[test]
    fn notification_text_never_becomes_a_shell_command() {
        let notification =
            Notification::new("\"; do shell script \"bad", "$(touch /tmp/no)").unwrap();
        for platform in [
            SystemPlatform::MacOS,
            SystemPlatform::Windows,
            SystemPlatform::Linux,
        ] {
            let plan = notification.plan(platform);
            assert_ne!(plan.command().program(), Path::new("sh"));
            assert_ne!(plan.command().program(), Path::new("cmd.exe"));
        }
        let mac = notification.plan(SystemPlatform::MacOS);
        assert!(mac.command().arg_eq(OsStr::new(
            "display notification (system attribute \"SCROZZ_NOTIFICATION_BODY\") with title (system attribute \"SCROZZ_NOTIFICATION_TITLE\")"
        )));
        assert!(
            !mac.command()
                .args()
                .iter()
                .any(|argument| argument == OsStr::new("$(touch /tmp/no)"))
        );
    }

    #[test]
    fn invalid_notification_text_is_rejected() {
        assert!(Notification::new("", "body").is_err());
        assert!(Notification::new("title", "\0body").is_err());
    }
}
