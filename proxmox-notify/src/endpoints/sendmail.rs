use proxmox_sendmail::Mail;
use serde::{Deserialize, Serialize};

use proxmox_schema::api_types::COMMENT_SCHEMA;
use proxmox_schema::{api, Updater};

use crate::context;
use crate::endpoints::common::mail;
use crate::renderer::TemplateType;
use crate::schema::{EMAIL_SCHEMA, ENTITY_NAME_SCHEMA, USER_SCHEMA};
use crate::{renderer, Content, Endpoint, Error, Notification, Origin};

pub(crate) const SENDMAIL_TYPENAME: &str = "sendmail";

#[api(
    properties: {
        name: {
            schema: ENTITY_NAME_SCHEMA,
        },
        mailto: {
            type: Array,
            items: {
                schema: EMAIL_SCHEMA,
            },
            optional: true,
        },
        "mailto-user": {
            type: Array,
            items: {
                schema: USER_SCHEMA,
            },
            optional: true,
        },
        comment: {
            optional: true,
            schema: COMMENT_SCHEMA,
        },
    },
)]
#[derive(Debug, Serialize, Deserialize, Updater, Default)]
#[serde(rename_all = "kebab-case")]
/// Config for Sendmail notification endpoints
pub struct SendmailConfig {
    /// Name of the endpoint
    #[updater(skip)]
    pub name: String,
    /// Mail address to send a mail to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[updater(serde(skip_serializing_if = "Option::is_none"))]
    pub mailto: Vec<String>,
    /// Users to send a mail to. The email address of the user
    /// will be looked up in users.cfg.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[updater(serde(skip_serializing_if = "Option::is_none"))]
    pub mailto_user: Vec<String>,
    /// `From` address for sent E-Mails.
    /// If the parameter is not set, the plugin will fall back to the
    /// email-from setting from node.cfg (PBS).
    /// If that is also not set, the plugin will default to root@$hostname,
    /// where $hostname is the hostname of the node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    /// Author of the mail. Defaults to 'Proxmox Backup Server ($hostname)'
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Deprecated.
    #[serde(skip_serializing)]
    #[updater(skip)]
    pub filter: Option<String>,
    /// Disable this target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable: Option<bool>,
    /// Origin of this config entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[updater(skip)]
    pub origin: Option<Origin>,
}

#[api]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The set of properties that can be deleted from a sendmail endpoint configuration.
pub enum DeleteableSendmailProperty {
    /// Delete `author`
    Author,
    /// Delete `comment`
    Comment,
    /// Delete `disable`
    Disable,
    /// Delete `from-address`
    FromAddress,
    /// Delete `mailto`
    Mailto,
    /// Delete `mailto-user`
    MailtoUser,
}

/// A sendmail notification endpoint.
pub struct SendmailEndpoint {
    pub config: SendmailConfig,
}

impl Endpoint for SendmailEndpoint {
    fn send(&self, notification: &Notification) -> Result<(), Error> {
        let recipients = mail::get_recipients(
            self.config.mailto.as_slice(),
            self.config.mailto_user.as_slice(),
        );

        let recipients_str: Vec<&str> = recipients.iter().map(String::as_str).collect();
        let mailfrom = self
            .config
            .from_address
            .clone()
            .unwrap_or_else(|| context().default_sendmail_from());

        match &notification.content {
            Content::Template {
                template_name,
                data,
            } => {
                let subject =
                    renderer::render_template(TemplateType::Subject, template_name, data)?;
                let html_part =
                    renderer::render_template(TemplateType::HtmlBody, template_name, data)?;
                let text_part =
                    renderer::render_template(TemplateType::PlaintextBody, template_name, data)?;

                let author = self
                    .config
                    .author
                    .clone()
                    .unwrap_or_else(|| context().default_sendmail_author());

                let mut mail = Mail::new(&author, &mailfrom, &subject, &text_part)
                    .with_html_alt(&html_part)
                    .with_unmasked_recipients();

                recipients_str.iter().for_each(|r| mail.add_recipient(r));

                mail.send()
                    .map_err(|err| Error::NotifyFailed(self.config.name.clone(), err.into()))
            }
            #[cfg(feature = "mail-forwarder")]
            Content::ForwardedMail { raw, uid, .. } => {
                Mail::forward(&recipients_str, &mailfrom, raw, *uid)
                    .map_err(|err| Error::NotifyFailed(self.config.name.clone(), err.into()))
            }
        }
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    /// Check if the endpoint is disabled
    fn disabled(&self) -> bool {
        self.config.disable.unwrap_or_default()
    }
}
