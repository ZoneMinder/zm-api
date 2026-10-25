//! Responses for ZoneMinder 1.39's Notifications (#175).

use serde::Serialize;
use utoipa::ToSchema;

use crate::dto::request::notifications::{PushPlatform, PushState};
use crate::dto::wrappers::DateTimeWrapper;
use crate::entity::notifications::Model;
use crate::util::naive_local_to_utc;

#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationResponse {
    pub id: u32,
    pub user_id: Option<u32>,
    pub token: String,
    /// `null` for a value this build doesn't know.
    pub platform: Option<PushPlatform>,
    pub monitor_list: Option<String>,
    pub interval: u32,
    pub push_state: Option<PushState>,
    pub app_version: Option<String>,
    pub profile: Option<String>,
    pub badge_count: i32,
    /// UTC, like event times.
    pub last_notified_at: Option<DateTimeWrapper>,
    pub created_on: Option<DateTimeWrapper>,
    pub updated_on: DateTimeWrapper,
}

impl From<&Model> for NotificationResponse {
    fn from(m: &Model) -> Self {
        Self {
            id: m.id,
            user_id: m.user_id,
            token: m.token.clone(),
            platform: PushPlatform::parse(&m.platform),
            monitor_list: m.monitor_list.clone(),
            interval: m.interval,
            push_state: PushState::parse(&m.push_state),
            app_version: m.app_version.clone(),
            profile: m.profile.clone(),
            badge_count: m.badge_count,
            last_notified_at: m
                .last_notified_at
                .map(|t| DateTimeWrapper(naive_local_to_utc(t))),
            created_on: m.created_on.map(|t| DateTimeWrapper(naive_local_to_utc(t))),
            updated_on: DateTimeWrapper(crate::util::timestamp_to_utc(m.updated_on)),
        }
    }
}
