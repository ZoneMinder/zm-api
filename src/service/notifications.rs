//! ZoneMinder 1.39's Notifications (#175), as its CakePHP
//! `NotificationsController` serves them: every user manages their own push
//! tokens, and a System editor sees and manages everyone's.

use crate::util::time::db_datetime;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, NotSet, QueryFilter, QueryOrder, Set};

use crate::dto::request::notifications::{
    CreateNotificationRequest, PushState, UpdateNotificationRequest,
};
use crate::dto::response::notifications::NotificationResponse;
use crate::entity::notifications;
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;
use crate::util::utc_to_datetime;

/// Who is asking: their id, and whether they are a System editor.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub uid: u32,
    pub admin: bool,
}

impl Caller {
    fn owns(&self, row: &notifications::Model) -> bool {
        self.admin || row.user_id == Some(self.uid)
    }
}

fn not_found(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("notification_id".into(), id.to_string())],
        resource_type: ResourceType::User,
    })
}

/// Another user's row is a 404, so ids don't reveal registrations.
async fn find_owned(state: &AppState, caller: Caller, id: u32) -> AppResult<notifications::Model> {
    notifications::Entity::find_by_id(id)
        .one(state.db())
        .await?
        .filter(|row| caller.owns(row))
        .ok_or_else(|| not_found(id))
}

pub async fn list(state: &AppState, caller: Caller) -> AppResult<Vec<NotificationResponse>> {
    let mut q = notifications::Entity::find().order_by_asc(notifications::Column::Id);
    if !caller.admin {
        q = q.filter(notifications::Column::UserId.eq(caller.uid));
    }
    Ok(q.all(state.db())
        .await?
        .iter()
        .map(NotificationResponse::from)
        .collect())
}

pub async fn get(state: &AppState, caller: Caller, id: u32) -> AppResult<NotificationResponse> {
    Ok(NotificationResponse::from(
        &find_owned(state, caller, id).await?,
    ))
}

/// Register a token, or update its row if it is already registered. The
/// bool is whether a row was created.
pub async fn upsert(
    state: &AppState,
    caller: Caller,
    req: CreateNotificationRequest,
) -> AppResult<(bool, NotificationResponse)> {
    let owner = match req.user_id {
        Some(uid) if caller.admin => Some(uid),
        _ => Some(caller.uid),
    };
    let existing = notifications::Entity::find()
        .filter(notifications::Column::Token.eq(req.token.as_str()))
        .one(state.db())
        .await?;
    let (created, mut am) = match existing {
        Some(row) if !caller.owns(&row) => {
            return Err(AppError::PermissionDeniedError(
                "this token is registered to another user".into(),
            ))
        }
        Some(row) => (false, notifications::ActiveModel::from(row)),
        None => (
            true,
            notifications::ActiveModel {
                id: NotSet,
                created_on: Set(Some(db_datetime(crate::util::time::local_now()))),
                token: Set(req.token.clone()),
                interval: Set(0),
                push_state: Set(PushState::Enabled.as_str().into()),
                badge_count: Set(0),
                ..Default::default()
            },
        ),
    };
    am.user_id = Set(owner);
    am.platform = Set(req.platform.as_str().into());
    if let Some(v) = req.monitor_list {
        am.monitor_list = Set(Some(v));
    }
    if let Some(v) = req.interval {
        am.interval = Set(v);
    }
    if let Some(v) = req.push_state {
        am.push_state = Set(v.as_str().into());
    }
    if let Some(v) = req.app_version {
        am.app_version = Set(Some(v));
    }
    if let Some(v) = req.profile {
        am.profile = Set(Some(v));
    }
    if let Some(v) = req.badge_count {
        am.badge_count = Set(v);
    }
    if let Some(v) = req.last_notified_at {
        am.last_notified_at = Set(Some(utc_to_datetime(v)));
    }
    let saved = if created {
        am.insert(state.db()).await?
    } else {
        am.update(state.db()).await?
    };
    Ok((created, NotificationResponse::from(&saved)))
}

pub async fn update(
    state: &AppState,
    caller: Caller,
    id: u32,
    req: UpdateNotificationRequest,
) -> AppResult<NotificationResponse> {
    let row = find_owned(state, caller, id).await?;
    if let Some(token) = req.token.as_deref().filter(|t| *t != row.token) {
        let taken = notifications::Entity::find()
            .filter(notifications::Column::Token.eq(token))
            .one(state.db())
            .await?;
        if taken.is_some() {
            return Err(AppError::ConflictError(
                "this token is already registered".into(),
            ));
        }
    }
    let mut am: notifications::ActiveModel = row.into();
    if let Some(v) = req.token {
        am.token = Set(v);
    }
    if let Some(v) = req.platform {
        am.platform = Set(v.as_str().into());
    }
    if let Some(v) = req.monitor_list {
        am.monitor_list = Set(v);
    }
    if let Some(v) = req.interval {
        am.interval = Set(v);
    }
    if let Some(v) = req.push_state {
        am.push_state = Set(v.as_str().into());
    }
    if let Some(v) = req.app_version {
        am.app_version = Set(v);
    }
    if let Some(v) = req.profile {
        am.profile = Set(v);
    }
    if let Some(v) = req.badge_count {
        am.badge_count = Set(v);
    }
    if let Some(v) = req.last_notified_at {
        am.last_notified_at = Set(v.map(utc_to_datetime));
    }
    if let (Some(uid), true) = (req.user_id, caller.admin) {
        am.user_id = Set(Some(uid));
    }
    Ok(NotificationResponse::from(&am.update(state.db()).await?))
}

pub async fn delete(state: &AppState, caller: Caller, id: u32) -> AppResult<()> {
    find_owned(state, caller, id).await?;
    notifications::Entity::delete_by_id(id)
        .exec(state.db())
        .await?;
    Ok(())
}
