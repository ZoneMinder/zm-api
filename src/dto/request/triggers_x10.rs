use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `zmx10.pl` reads these as lists of X10 unit codes (`12+`, `!3-`, comma
/// separated). Nothing else belongs in one, least of all a control character.
fn x10_codes(value: &Option<String>, _: &()) -> garde::Result {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '!' | ',' | ' ');
    match value.as_deref() {
        Some(v) if !v.chars().all(ok) => Err(garde::Error::new(
            "may hold only unit codes: letters, digits, `+`, `-`, `!` and commas",
        )),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct CreateTriggerX10Request {
    #[garde(skip)]
    pub monitor_id: u32,
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub activation: Option<String>,
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub alarm_input: Option<String>,
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub alarm_output: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct UpdateTriggerX10Request {
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub activation: Option<String>,
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub alarm_input: Option<String>,
    #[garde(inner(length(chars, max = 32)), custom(x10_codes))]
    pub alarm_output: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(activation: &str) -> UpdateTriggerX10Request {
        UpdateTriggerX10Request {
            activation: Some(activation.into()),
            alarm_input: None,
            alarm_output: None,
        }
    }

    #[test]
    fn a_trigger_holds_unit_codes_and_nothing_else() {
        assert!(update("12+,!3-").validate().is_ok());
        assert!(update("").validate().is_ok());
        assert!(update("1;`id`").validate().is_err());
        assert!(update("1\n2").validate().is_err());
        assert!(update(&"1".repeat(33)).validate().is_err());
    }
}
