use crate::context::{common, Context};
use crate::renderer::TemplateSource;
use crate::Error;
use std::path::Path;

fn lookup_mail_address(content: &str, user: &str) -> Option<String> {
    common::normalize_for_return(content.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        #[allow(clippy::get_first)] // to keep expression style consistent
        match fields.get(0)?.trim() == "user" && fields.get(1)?.trim() == user {
            true => fields.get(6).copied(),
            false => None,
        }
    }))
}

const DEFAULT_CONFIG: &str = "\
sendmail: mail-to-root
	comment Send mails to root@pam's email address
	mailto-user root@pam


matcher: default-matcher
    mode all
    target mail-to-root
    comment Route all notifications to mail-to-root
";

#[derive(Debug)]
pub struct PVEContext;

impl Context for PVEContext {
    fn lookup_email_for_user(&self, user: &str) -> Option<String> {
        let content = common::attempt_file_read("/etc/pve/user.cfg");
        content.and_then(|content| lookup_mail_address(&content, user))
    }

    fn default_sendmail_author(&self) -> String {
        "Proxmox VE".into()
    }

    fn default_sendmail_from(&self) -> String {
        let content = common::attempt_file_read("/etc/pve/datacenter.cfg");
        content
            .and_then(|content| common::lookup_datacenter_config_key(&content, "email_from"))
            .unwrap_or_else(|| String::from("root"))
    }

    fn http_proxy_config(&self) -> Option<String> {
        let content = common::attempt_file_read("/etc/pve/datacenter.cfg");
        content.and_then(|content| common::lookup_datacenter_config_key(&content, "http_proxy"))
    }

    fn default_config(&self) -> &'static str {
        DEFAULT_CONFIG
    }

    fn lookup_template(
        &self,
        filename: &str,
        namespace: Option<&str>,
        source: TemplateSource,
    ) -> Result<Option<String>, Error> {
        let path = match source {
            TemplateSource::Vendor => "/usr/share/pve-manager/templates",
            TemplateSource::Override => "/etc/pve/notification-templates",
        };

        let path = Path::new(&path)
            .join(namespace.unwrap_or("default"))
            .join(filename);

        let template_string = proxmox_sys::fs::file_read_optional_string(path)
            .map_err(|err| Error::Generic(format!("could not load template: {err}")))?;
        Ok(template_string)
    }
}

pub static PVE_CONTEXT: PVEContext = PVEContext;

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CONFIG: &str = "
user:root@pam:1:0:::root@example.com:::
user:test@pve:1:0:::test@example.com:::
user:no-mail@pve:1:0::::::
    ";

    #[test]
    fn test_parse_mail() {
        assert_eq!(
            lookup_mail_address(USER_CONFIG, "root@pam"),
            Some("root@example.com".to_string())
        );
        assert_eq!(
            lookup_mail_address(USER_CONFIG, "test@pve"),
            Some("test@example.com".to_string())
        );
        assert_eq!(lookup_mail_address(USER_CONFIG, "no-mail@pve"), None);
    }

    const DC_CONFIG: &str = "
email_from: user@example.com
http_proxy: http://localhost:1234
keyboard: en-us
";
    #[test]
    fn test_parse_dc_config() {
        assert_eq!(
            common::lookup_datacenter_config_key(DC_CONFIG, "email_from"),
            Some("user@example.com".to_string())
        );
        assert_eq!(
            common::lookup_datacenter_config_key(DC_CONFIG, "http_proxy"),
            Some("http://localhost:1234".to_string())
        );
        assert_eq!(common::lookup_datacenter_config_key(DC_CONFIG, "foo"), None);
    }
}
