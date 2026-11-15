//! Requests for ZoneMinder 1.39's EncoderTemplates and Menu_Items (#144).

use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct CreateEncoderTemplateRequest {
    /// The encoder the parameters are for, e.g. `libx264`.
    #[garde(length(chars, min = 1, max = 32))]
    pub encoder: String,
    #[garde(length(chars, min = 1, max = 64))]
    pub name: String,
    #[garde(skip)]
    pub description: Option<String>,
    /// `key=value` lines handed to the encoder.
    #[garde(skip)]
    pub params: String,
}

#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct UpdateEncoderTemplateRequest {
    #[garde(inner(length(chars, min = 1, max = 32)))]
    pub encoder: Option<String>,
    #[garde(inner(length(chars, min = 1, max = 64)))]
    pub name: Option<String>,
    /// `null` clears it; omit to leave it unchanged.
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    #[garde(skip)]
    pub description: Option<Option<String>>,
    #[garde(skip)]
    pub params: Option<String>,
}

fn valid_icon_type(v: &Option<String>, _: &()) -> garde::Result {
    match v.as_deref() {
        None | Some("material" | "fontawesome" | "image" | "none") => Ok(()),
        Some(_) => Err(garde::Error::new(
            "must be `material`, `fontawesome`, `image` or `none`",
        )),
    }
}

/// A menu link is put in an `href` for every user, so it must be somewhere to
/// go and not something to run: a path on this site, or an http(s) URL.
/// `javascript:` and `data:` links would run the link's author's script in
/// each user's session.
fn safe_link(value: &str) -> garde::Result {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let on_site = v.starts_with('/') && !v.starts_with("//") && !v.starts_with("/\\");
    let web = lower.starts_with("http://") || lower.starts_with("https://");
    let plain = !v.chars().any(char::is_control);
    if v.is_empty() || (plain && (on_site || web || v.starts_with('?') || v.starts_with('#'))) {
        Ok(())
    } else {
        Err(garde::Error::new(
            "must be a path starting with `/`, or an http(s) URL",
        ))
    }
}

fn valid_link(v: &Option<String>, _: &()) -> garde::Result {
    v.as_deref().map_or(Ok(()), safe_link)
}

fn valid_link_update(v: &Option<Option<String>>, _: &()) -> garde::Result {
    match v {
        Some(Some(link)) => safe_link(link),
        _ => Ok(()),
    }
}

#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct CreateMenuItemRequest {
    /// The navbar entry this row configures, e.g. `Console`.
    #[garde(length(chars, min = 1, max = 32))]
    pub menu_key: String,
    #[garde(skip)]
    pub enabled: Option<bool>,
    #[garde(inner(length(chars, max = 64)))]
    pub label: Option<String>,
    #[garde(skip)]
    pub sort_order: Option<i16>,
    #[garde(inner(length(chars, max = 128)))]
    pub icon: Option<String>,
    #[garde(custom(valid_icon_type))]
    pub icon_type: Option<String>,
    #[garde(inner(length(chars, max = 255)), custom(valid_link))]
    pub link: Option<String>,
}

/// Partial update. For `label`, `icon` and `link`, `null` clears the value.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct UpdateMenuItemRequest {
    #[garde(skip)]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    #[garde(skip)]
    pub label: Option<Option<String>>,
    #[garde(skip)]
    pub sort_order: Option<i16>,
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    #[garde(skip)]
    pub icon: Option<Option<String>>,
    #[garde(custom(valid_icon_type))]
    pub icon_type: Option<String>,
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    #[garde(custom(valid_link_update))]
    pub link: Option<Option<String>>,
}

/// Record the answer to the privacy statement (#189).
#[derive(Debug, Serialize, Deserialize, ToSchema, Validate)]
pub struct PrivacyRequest {
    /// Accept (send telemetry) or decline.
    #[garde(skip)]
    pub accept: bool,
}

#[cfg(test)]
mod link_tests {
    use super::*;

    fn item(link: &str) -> CreateMenuItemRequest {
        CreateMenuItemRequest {
            menu_key: "Docs".into(),
            link: Some(link.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_menu_link_goes_somewhere_and_runs_nothing() {
        for ok in [
            "/console",
            "https://wiki.example.com/cams",
            "http://nas.lan",
            "?view=log",
            "",
        ] {
            assert!(item(ok).validate().is_ok(), "{ok:?}");
        }
        for bad in [
            "javascript:alert(document.cookie)",
            " JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:x",
            "//evil.example.com",
            "/\\evil.example.com",
            "/ok\njavascript:alert(1)",
        ] {
            assert!(item(bad).validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_update_checks_the_link_and_may_clear_it() {
        let with = |link: Option<Option<&str>>| UpdateMenuItemRequest {
            link: link.map(|l| l.map(str::to_string)),
            ..Default::default()
        };
        assert!(with(None).validate().is_ok());
        assert!(with(Some(None)).validate().is_ok());
        assert!(with(Some(Some("/events"))).validate().is_ok());
        assert!(with(Some(Some("javascript:alert(1)"))).validate().is_err());
    }
}
