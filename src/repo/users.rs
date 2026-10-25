use crate::dto::PaginationParams;
use crate::entity::users::{Entity as Users, Model as UserModel};
use crate::error::AppResult;
use sea_orm::*;

pub async fn find_all(db: &DatabaseConnection) -> AppResult<Vec<UserModel>> {
    Ok(Users::find().all(db).await?)
}

pub async fn find_paginated(
    db: &DatabaseConnection,
    params: &PaginationParams,
) -> AppResult<(Vec<UserModel>, u64)> {
    let paginator = Users::find().paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

pub async fn find_by_id(db: &DatabaseConnection, id: u32) -> AppResult<Option<UserModel>> {
    Ok(Users::find_by_id(id).one(db).await?)
}

// Find a user by username, filtered to those whose accounts are enabled AND
// whose API access is enabled. ZoneMinder maintains a separate `APIEnabled`
// column intended to let an admin disable the API for a user without
// suspending their web-UI login — honour it at the auth boundary so a
// disabled-API user can't acquire a token.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn find_by_username_and_status(
    db: &DatabaseConnection,
    username: &str,
    is_enabled: bool,
) -> AppResult<Option<UserModel>> {
    Ok(Users::find()
        .filter(
            crate::entity::users::Column::Username
                .eq(username)
                .and(crate::entity::users::Column::Enabled.eq(is_enabled))
                .and(crate::entity::users::Column::ApiEnabled.eq(1u8)),
        )
        .one(db)
        .await?)
}

/// Apply a partial update. `password` is expected to already be hashed by the
/// caller (the service layer). Only provided fields change.
pub async fn update(
    db: &DatabaseConnection,
    id: u32,
    req: &crate::dto::request::UpdateUserRequest,
    hashed_password: Option<String>,
) -> AppResult<Option<UserModel>> {
    use sea_orm::{ActiveModelTrait, Set};
    let Some(model) = find_by_id(db, id).await? else {
        return Ok(None);
    };
    let mut active: crate::entity::users::ActiveModel = model.into();

    if let Some(e) = &req.email {
        active.email = Set(e.clone());
    }
    if let Some(en) = req.enabled {
        active.enabled = Set(en);
    }
    if let Some(pw) = hashed_password {
        active.password = Set(pw);
    }
    if let Some(n) = &req.name {
        active.name = Set(n.clone());
    }
    if let Some(p) = &req.phone {
        active.phone = Set(p.clone());
    }
    if let Some(l) = &req.language {
        active.language = Set(Some(l.clone()));
    }
    if let Some(hv) = &req.home_view {
        active.home_view = Set(hv.clone());
    }
    if let Some(a) = req.api_enabled {
        active.api_enabled = Set(a);
    }
    if let Some(mb) = &req.max_bandwidth {
        active.max_bandwidth = Set(Some(mb.clone()));
    }
    if let Some(t) = req.token_min_expiry {
        active.token_min_expiry = Set(t);
    }
    if let Some(role) = req.role_id {
        active.role_id = Set(role);
    }
    // Permission levels are validated/parsed from their string form.
    let p = &req.permissions;
    if let Some(v) = p.stream_level()? {
        active.stream = Set(v);
    }
    if let Some(v) = p.events_level()? {
        active.events = Set(v);
    }
    if let Some(v) = p.control_level()? {
        active.control = Set(v);
    }
    if let Some(v) = p.monitors_level()? {
        active.monitors = Set(v);
    }
    if let Some(v) = p.groups_level()? {
        active.groups = Set(v);
    }
    if let Some(v) = p.devices_level()? {
        active.devices = Set(v);
    }
    if let Some(v) = p.snapshots_level()? {
        active.snapshots = Set(v);
    }
    if let Some(v) = p.system_level()? {
        active.system = Set(v);
    }

    let updated = active.update(db).await?;
    Ok(Some(updated))
}

pub async fn create(
    db: &DatabaseConnection,
    req: &crate::dto::request::CreateUserRequest,
) -> AppResult<UserModel> {
    use crate::entity::sea_orm_active_enums as E;
    use crate::entity::users::ActiveModel as AM;
    use sea_orm::{ActiveModelTrait, Set};
    let p = &req.permissions;
    let am = AM {
        id: Default::default(),
        username: Set(req.username.clone()),
        password: Set(req.password.clone()),
        name: Set(req.name.clone().unwrap_or_default()),
        email: Set(req.email.clone()),
        phone: Set(req.phone.clone().unwrap_or_default()),
        language: Set(req.language.clone()),
        enabled: Set(req.enabled.unwrap_or(1)),
        // Omitted permissions default to View (the previous behaviour).
        stream: Set(p.stream_level()?.unwrap_or(E::Stream::View)),
        events: Set(p.events_level()?.unwrap_or(E::Events::View)),
        control: Set(p.control_level()?.unwrap_or(E::Control::View)),
        monitors: Set(p.monitors_level()?.unwrap_or(E::Monitors::View)),
        groups: Set(p.groups_level()?.unwrap_or(E::Groups::View)),
        devices: Set(p.devices_level()?.unwrap_or(E::Devices::View)),
        snapshots: Set(p.snapshots_level()?.unwrap_or(E::Snapshots::View)),
        system: Set(p.system_level()?.unwrap_or(E::System::View)),
        max_bandwidth: Set(req.max_bandwidth.clone()),
        token_min_expiry: Set(0),
        api_enabled: Set(req.api_enabled.unwrap_or(1)),
        home_view: Set(req
            .home_view
            .clone()
            .unwrap_or_else(|| "console".to_string())),
        role_id: Set(req.role_id),
    };
    am.insert(db).await.map_err(|e| match e.sql_err() {
        // `Users.Username` is unique.
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_)) => {
            crate::error::AppError::ConflictError(format!(
                "a user named {:?} already exists",
                req.username
            ))
        }
        _ => e.into(),
    })
}

/// Set a user's bcrypt password hash.
pub async fn set_password(db: &DatabaseConnection, id: u32, hash: String) -> AppResult<()> {
    use crate::entity::users::Column;
    use sea_orm::sea_query::Expr;
    Users::update_many()
        .col_expr(Column::Password, Expr::value(hash))
        .filter(Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(())
}

/// Raise the user's token-revocation floor: tokens issued before `min_iat`
/// (unix seconds) become invalid. Used by logout and password changes.
pub async fn set_token_min_expiry(db: &DatabaseConnection, id: u32, min_iat: u64) -> AppResult<()> {
    use crate::entity::users::Column;
    use sea_orm::sea_query::Expr;
    Users::update_many()
        .col_expr(Column::TokenMinExpiry, Expr::value(min_iat))
        .filter(Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn delete_by_id(db: &DatabaseConnection, id: u32) -> AppResult<bool> {
    use sea_orm::EntityTrait;
    let res = Users::delete_by_id(id).exec(db).await?;
    Ok(res.rows_affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::sea_orm_active_enums as E;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn mk_user(id: u32, username: &str, enabled: u8) -> UserModel {
        UserModel {
            id,
            username: username.to_string(),
            password: "pass".to_string(),
            name: "Name".to_string(),
            email: "user@example.com".to_string(),
            phone: "".to_string(),
            language: None,
            enabled,
            stream: E::Stream::View,
            events: E::Events::View,
            control: E::Control::View,
            monitors: E::Monitors::View,
            groups: E::Groups::View,
            devices: E::Devices::View,
            snapshots: E::Snapshots::View,
            system: E::System::View,
            max_bandwidth: None,
            token_min_expiry: 0,
            api_enabled: 1,
            home_view: "console".to_string(),
            role_id: None,
        }
    }

    #[tokio::test]
    async fn test_find_by_username_and_status_found() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![vec![mk_user(1, "admin", 1)]])
            .into_connection();

        let res = find_by_username_and_status(&db, "admin", true)
            .await
            .unwrap();
        assert!(res.is_some());
        let user = res.unwrap();
        assert_eq!(user.username, "admin");
        assert_eq!(user.enabled, 1);
    }

    #[tokio::test]
    async fn test_find_by_username_and_status_not_found() {
        let empty: Vec<UserModel> = Vec::new();
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![empty])
            .into_connection();

        let res = find_by_username_and_status(&db, "missing", true)
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn test_update_happy_path() {
        let initial = mk_user(42, "user", 1);
        let mut after = initial.clone();
        after.email = "new@example.com".into();
        after.enabled = 0;
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![vec![initial]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<UserModel, _, _>(vec![vec![after.clone()]])
            .into_connection();

        let req = crate::dto::request::UpdateUserRequest {
            email: Some("new@example.com".into()),
            enabled: Some(0),
            ..Default::default()
        };
        let updated = update(&db, 42, &req, None).await.unwrap().unwrap();
        assert_eq!(updated.email, "new@example.com");
        assert_eq!(updated.enabled, 0);
    }

    #[tokio::test]
    async fn test_delete_by_id_affects_rows() {
        let db_true = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        assert!(delete_by_id(&db_true, 1).await.unwrap());

        let db_false = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .into_connection();
        assert!(!delete_by_id(&db_false, 1).await.unwrap());
    }
}
