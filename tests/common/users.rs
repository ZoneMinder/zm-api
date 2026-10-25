//! Users made the way an administrator makes them, who then log in (#235).
//!
//! `superuser_token()` mints a token for a user that isn't in the database,
//! which sees every monitor. Real users get their levels from their `Users`
//! row and role at login, and are limited by permission rows. [`UserSpec`]
//! creates the role and user through the API, adds the rows (through the API
//! where it has an endpoint, otherwise as ZoneMinder's web UI writes them),
//! and logs in through `POST /api/v3/auth/login`. Dropping the [`RealUser`]
//! deletes the user and role; their permission rows cascade.
#![allow(dead_code)]

use axum::http::{Method, StatusCode};
use sea_orm::{ActiveModelTrait, Set};
use serde_json::{json, Value};
use zm_api::entity::sea_orm_active_enums::Permission;

use super::fixtures::{unique_name, RowGuard};
use super::harness::{superuser_token, TestApp};

const PASSWORD: &str = "a-real-users-password-1";

const FEATURES: [&str; 8] = [
    "stream",
    "events",
    "control",
    "monitors",
    "groups",
    "devices",
    "snapshots",
    "system",
];

/// `unique_name` is unique per run, not per call, and usernames are unique.
fn next_name(label: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    unique_name(&format!("{label}{}", N.fetch_add(1, Ordering::Relaxed)))
}

/// What a user is given before they log in.
#[derive(Default)]
pub struct UserSpec {
    permissions: Value,
    role: Option<Value>,
    monitors: Vec<(u32, &'static str)>,
    groups: Vec<(u32, &'static str)>,
    role_monitors: Vec<(u32, Permission)>,
    role_groups: Vec<(u32, Permission)>,
}

/// A logged-in user. `token` is the access token login returned.
pub struct RealUser {
    pub id: u32,
    pub username: String,
    pub token: String,
    // Users first, then the role: dropped in this order.
    _guards: Vec<RowGuard>,
}

impl UserSpec {
    /// A user with these feature levels, e.g.
    /// `json!({"monitors": "View", "events": "View"})`. Unnamed features are
    /// `None`.
    pub fn new(permissions: Value) -> Self {
        Self {
            permissions,
            ..Default::default()
        }
    }

    /// Give the user a role with these feature levels.
    pub fn role(mut self, permissions: Value) -> Self {
        self.role = Some(permissions);
        self
    }

    /// A `Monitors_Permissions` row: `None`, `View`, `Edit` or `Inherit`.
    pub fn monitor(mut self, monitor_id: u32, permission: &'static str) -> Self {
        self.monitors.push((monitor_id, permission));
        self
    }

    /// A `Groups_Permissions` row.
    pub fn group(mut self, group_id: u32, permission: &'static str) -> Self {
        self.groups.push((group_id, permission));
        self
    }

    /// A `Role_Monitors_Permissions` row (needs [`role`](Self::role)).
    pub fn role_monitor(mut self, monitor_id: u32, permission: Permission) -> Self {
        self.role_monitors.push((monitor_id, permission));
        self
    }

    /// A `Role_Groups_Permissions` row (needs [`role`](Self::role)).
    pub fn role_group(mut self, group_id: u32, permission: Permission) -> Self {
        self.role_groups.push((group_id, permission));
        self
    }

    /// Create everything and log in.
    pub async fn login(self, app: &TestApp) -> RealUser {
        let admin = superuser_token();
        let mut guards = Vec::new();

        let role_id = match &self.role {
            None => None,
            Some(levels) => {
                let mut body = levels.clone();
                body["name"] = json!(next_name("RealRole"));
                let r = app.post_json("/api/v3/roles", &admin, &body).await;
                assert_eq!(r.status(), StatusCode::CREATED, "create role: {}", r.text());
                let id = r.json::<Value>()["id"].as_u64().unwrap() as u32;
                guards.push(RowGuard::new(
                    format!("User_Roles#{id}"),
                    move |db| async move {
                        use sea_orm::EntityTrait;
                        let _ = zm_api::entity::user_roles::Entity::delete_by_id(id)
                            .exec(&db)
                            .await;
                    },
                ));
                Some(id)
            }
        };

        let username = next_name("RealUser");
        // The levels sit beside the other fields, and the API defaults an
        // omitted one to View, so send all eight.
        let mut body = json!({
            "username": username,
            "password": PASSWORD,
            "email": format!("{username}@example.invalid"),
            "enabled": 1,
            "api_enabled": 1,
            "role_id": role_id,
        });
        for feature in FEATURES {
            body[feature] = self
                .permissions
                .get(feature)
                .cloned()
                .unwrap_or(json!("None"));
        }
        let r = app.post_json("/api/v3/users", &admin, &body).await;
        assert_eq!(r.status(), StatusCode::CREATED, "create user: {}", r.text());
        let id = r.json::<Value>()["id"].as_u64().unwrap() as u32;
        guards.insert(0, RowGuard::user(id));

        for (monitor_id, permission) in &self.monitors {
            let r = app
                .post_json(
                    "/api/v3/monitors-permissions",
                    &admin,
                    &json!({ "monitor_id": monitor_id, "user_id": id, "permission": permission }),
                )
                .await;
            assert!(r.status().is_success(), "monitor grant: {}", r.text());
        }
        for (group_id, permission) in &self.groups {
            let r = app
                .post_json(
                    "/api/v3/groups-permissions",
                    &admin,
                    &json!({ "group_id": group_id, "user_id": id, "permission": permission }),
                )
                .await;
            assert!(r.status().is_success(), "group grant: {}", r.text());
        }
        // The API has no endpoint for a role's rows; ZoneMinder's web UI
        // writes them directly.
        for (monitor_id, permission) in self.role_monitors {
            zm_api::entity::role_monitors_permissions::ActiveModel {
                role_id: Set(role_id.expect("role_monitor needs a role")),
                monitor_id: Set(monitor_id),
                permission: Set(permission),
                ..Default::default()
            }
            .insert(&app.db)
            .await
            .expect("role monitor grant");
        }
        for (group_id, permission) in self.role_groups {
            zm_api::entity::role_groups_permissions::ActiveModel {
                role_id: Set(role_id.expect("role_group needs a role")),
                group_id: Set(group_id),
                permission: Set(permission),
                ..Default::default()
            }
            .insert(&app.db)
            .await
            .expect("role group grant");
        }

        let r = app
            .request(Method::POST, "/api/v3/auth/login")
            .json(&json!({ "username": username, "password": PASSWORD }))
            .send()
            .await;
        assert_eq!(r.status(), StatusCode::OK, "login: {}", r.text());
        let token = r.json::<Value>()["access_token"]
            .as_str()
            .expect("access_token")
            .to_string();

        RealUser {
            id,
            username,
            token,
            _guards: guards,
        }
    }
}
