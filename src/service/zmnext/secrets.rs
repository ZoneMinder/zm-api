//! Secrets in stored zm-next processing graphs.
//!
//! A few plugin settings are credentials: `output_mqtt.password`,
//! `output_webhook.auth_header` and `llm_event_review.api_key`. They are never
//! kept in `monitor_pipeline.graph_json` and never returned by the API. When a
//! graph is saved, each literal value is encrypted into the `zmnext_secret`
//! table and replaced in the graph by a reference:
//!
//! ```json
//! {"kind": "output_webhook", "cfg": {"auth_header": {"$secret": "notify.auth_header"}}}
//! ```
//!
//! That is the reference form of zm-next's worker control protocol ("Configure"
//! in `docs/Worker_Control_Protocol.md`). A worker that speaks it gets the
//! references plus a separate `secrets` map ([`SecretMap`]); an older worker that
//! reads its pipeline from stdin gets the values resolved in place
//! ([`resolve_in_place`]).
//!
//! Values are sealed with ChaCha20-Poly1305 under a key file (see
//! `[zmnext.secrets].key_file`), with the monitor id and secret name as
//! associated data, so a ciphertext copied to another row doesn't decrypt.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::Path;

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sea_orm::ConnectionTrait;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

/// `(plugin kind, cfg key)` pairs whose values are secrets.
pub const SECRET_KEYS: &[(&str, &str)] = &[
    ("output_mqtt", "password"),
    ("output_webhook", "auth_header"),
    ("llm_event_review", "api_key"),
];

/// Whether `key` in a `kind` node's cfg holds a secret.
pub fn is_secret_key(kind: &str, key: &str) -> bool {
    SECRET_KEYS.iter().any(|&(k, c)| k == kind && c == key)
}

/// The name a `{"$secret": name}` value refers to, if `v` is a reference.
pub fn reference_name(v: &Value) -> Option<&str> {
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get("$secret")?.as_str().filter(|n| !n.is_empty())
}

/// Secret name → plaintext, as sent in a configure's `secrets` map.
pub type SecretMap = BTreeMap<String, String>;

/// Callback for [`for_each_cfg`]: node kind, node identity, node cfg.
type CfgVisitor<'a> = dyn FnMut(&str, &str, &mut serde_json::Map<String, Value>) + 'a;

/// Walk every plugin node (`plugins` and nested `children`), calling `f` with
/// the node's kind, a stable identity (its `id`, or its path) and its cfg.
fn for_each_cfg(graph: &mut Value, f: &mut CfgVisitor<'_>) {
    fn walk(nodes: &mut Value, path: &str, f: &mut CfgVisitor<'_>) {
        let Some(arr) = nodes.as_array_mut() else {
            return;
        };
        for (i, node) in arr.iter_mut().enumerate() {
            let here = format!("{path}.{i}");
            let Some(obj) = node.as_object_mut() else {
                continue;
            };
            let kind = obj
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let ident = obj
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| here.trim_start_matches('.').to_string());
            for cfg_key in ["cfg", "config"] {
                if let Some(cfg) = obj.get_mut(cfg_key).and_then(Value::as_object_mut) {
                    f(&kind, &ident, cfg);
                }
            }
            if let Some(children) = obj.get_mut("children") {
                walk(children, &format!("{here}.children"), f);
            }
        }
    }
    if let Some(plugins) = graph.get_mut("plugins") {
        walk(plugins, "plugins", f);
    }
}

/// Replace every literal secret in `graph` with a reference, returning the
/// extracted `(name, value)` pairs. References already present are left alone.
pub fn extract_literals(graph: &mut Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for_each_cfg(graph, &mut |kind, ident, cfg| {
        for (key, value) in cfg.iter_mut() {
            if !is_secret_key(kind, key) {
                continue;
            }
            if let Some(literal) = value.as_str() {
                let name = format!("{ident}.{key}");
                out.push((name.clone(), literal.to_string()));
                *value = json!({ "$secret": name });
            }
        }
    });
    out
}

/// Names of every secret reference in `graph`.
pub fn references(graph: &Value) -> Vec<String> {
    let mut graph = graph.clone();
    let mut out = Vec::new();
    for_each_cfg(&mut graph, &mut |_, _, cfg| {
        for value in cfg.values() {
            if let Some(name) = reference_name(value) {
                out.push(name.to_string());
            }
        }
    });
    out.sort();
    out.dedup();
    out
}

/// Replace references with their values, for a worker that reads a plain
/// pipeline. Errors name the first reference with no value.
pub fn resolve_in_place(graph: &mut Value, secrets: &SecretMap) -> Result<(), String> {
    let mut missing: Option<String> = None;
    for_each_cfg(graph, &mut |_, _, cfg| {
        for value in cfg.values_mut() {
            let Some(name) = reference_name(value).map(str::to_string) else {
                continue;
            };
            match secrets.get(&name) {
                Some(v) => *value = Value::String(v.clone()),
                None => {
                    missing.get_or_insert(name);
                }
            }
        }
    });
    match missing {
        Some(name) => Err(format!("secret `{name}` is referenced but not stored")),
        None => Ok(()),
    }
}

/// The encryption key for stored secrets.
pub struct SecretKey(Key);

impl SecretKey {
    /// Load the key from `path`, creating it (32 random bytes, mode 0600) if the
    /// file doesn't exist.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == 32 => Ok(Self(Key::from(
                <[u8; 32]>::try_from(bytes.as_slice()).expect("length checked above"),
            ))),
            Ok(bytes) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} holds {} bytes; a zm-next secrets key is 32",
                    path.display(),
                    bytes.len()
                ),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // rand's thread RNG is a CSPRNG seeded from the OS.
                let key = Key::from(rand::random::<[u8; 32]>());
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                match opts.open(path) {
                    Ok(mut f) => {
                        f.write_all(key.as_slice())?;
                        f.sync_all()?;
                        Ok(Self(key))
                    }
                    // Another process created it first: use theirs.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Self::load_or_create(path)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    fn for_tests() -> Self {
        Self(Key::from([7u8; 32]))
    }

    fn aad(monitor_id: u32, name: &str) -> Vec<u8> {
        format!("zmnext-secret:v1:{monitor_id}:{name}").into_bytes()
    }

    /// Encrypt `value` for `(monitor_id, name)`.
    pub fn seal(&self, monitor_id: u32, name: &str, value: &str) -> String {
        let cipher = ChaCha20Poly1305::new(&self.0);
        let nonce = Nonce::from(rand::random::<[u8; 12]>());
        let ct = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad: &Self::aad(monitor_id, name),
                },
            )
            .expect("ChaCha20-Poly1305 encryption does not fail for in-memory input");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    /// Decrypt a value sealed for `(monitor_id, name)`.
    pub fn open(&self, monitor_id: u32, name: &str, sealed: &str) -> Result<String, String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sealed)
            .map_err(|e| format!("secret `{name}` is not valid base64: {e}"))?;
        if raw.len() < 12 {
            return Err(format!("secret `{name}` is truncated"));
        }
        let (nonce, ct) = raw.split_at(12);
        let nonce = Nonce::try_from(nonce).map_err(|_| format!("secret `{name}` is truncated"))?;
        let cipher = ChaCha20Poly1305::new(&self.0);
        let plain = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ct,
                    aad: &Self::aad(monitor_id, name),
                },
            )
            .map_err(|_| {
                format!("secret `{name}` does not decrypt with the configured key file")
            })?;
        String::from_utf8(plain).map_err(|_| format!("secret `{name}` is not UTF-8"))
    }
}

/// The `secrets_salt` zm-api sends in a monitor's configure: derived from the
/// secrets key and the monitor id, so it's stable without being stored and
/// unguessable without the key file.
pub fn secrets_salt(key_file: &Path, monitor_id: u32) -> AppResult<String> {
    use sha2::{Digest, Sha256};
    let key = SecretKey::load_or_create(key_file).map_err(|e| key_error(key_file, e))?;
    let mut hasher = Sha256::new();
    hasher.update(key.0.as_slice());
    hasher.update(format!("zmnext-secrets-salt:{monitor_id}").as_bytes());
    let digest = hasher.finalize();
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..16]))
}

fn key_error(path: &Path, e: std::io::Error) -> AppError {
    AppError::InternalServerError(format!(
        "zm-next secrets key {} is unusable: {e}",
        path.display()
    ))
}

/// Move literal secrets out of `graph` into the store for `monitor_id` and check
/// that every remaining reference has a stored value. `graph` is left holding
/// references only. Call [`prune_secrets`] once the graph itself is saved.
pub async fn stash_secrets<C: ConnectionTrait>(
    db: &C,
    key_file: &Path,
    monitor_id: u32,
    graph: &mut Value,
) -> AppResult<()> {
    let literals = extract_literals(graph);
    let wanted = references(graph);
    let extracted: HashSet<&str> = literals.iter().map(|(n, _)| n.as_str()).collect();
    let stored: HashSet<String> = crate::repo::zmnext_secret::find_by_monitor(db, monitor_id)
        .await?
        .into_iter()
        .map(|r| r.name)
        .collect();
    if let Some(unknown) = wanted
        .iter()
        .find(|n| !extracted.contains(n.as_str()) && !stored.contains(*n))
    {
        return Err(AppError::BadRequestError(format!(
            "`{{\"$secret\": \"{unknown}\"}}` refers to a secret this monitor doesn't have; \
             give the value instead"
        )));
    }

    if !literals.is_empty() {
        let key = SecretKey::load_or_create(key_file).map_err(|e| key_error(key_file, e))?;
        let now = crate::util::time::utc_now();
        for (name, value) in &literals {
            let sealed = key.seal(monitor_id, name, value);
            crate::repo::zmnext_secret::upsert(db, monitor_id, name, sealed, now).await?;
        }
    }
    Ok(())
}

/// Delete a monitor's stored secrets that `graph` (the saved graph, or `None`
/// once the graph is deleted) doesn't reference.
pub async fn prune_secrets<C: ConnectionTrait>(
    db: &C,
    monitor_id: u32,
    graph: Option<&Value>,
) -> AppResult<()> {
    let keep = graph.map(references).unwrap_or_default();
    crate::repo::zmnext_secret::delete_unreferenced(db, monitor_id, &keep).await?;
    Ok(())
}

/// If a stored graph still holds literal secrets (saved before secrets were
/// split out), move them into the store and rewrite the row. Returns the graph
/// as it now stands. Rows without literals are left untouched.
pub async fn migrate_row(
    db: &sea_orm::DatabaseConnection,
    key_file: &Path,
    row: crate::entity::monitor_pipeline::Model,
) -> AppResult<crate::entity::monitor_pipeline::Model> {
    let Ok(mut graph) = serde_json::from_str::<Value>(&row.graph_json) else {
        return Ok(row);
    };
    if extract_literals(&mut graph.clone()).is_empty() {
        return Ok(row);
    }
    stash_secrets(db, key_file, row.monitor_id, &mut graph).await?;
    let now = crate::util::time::utc_now();
    let saved = crate::repo::monitor_pipeline::upsert(
        db,
        row.monitor_id,
        serde_json::to_string(&graph)?,
        row.version,
        now,
    )
    .await?;
    prune_secrets(db, row.monitor_id, Some(&graph)).await?;
    tracing::info!(
        "zm-next: moved literal secrets out of monitor {}'s stored pipeline graph",
        row.monitor_id
    );
    Ok(saved)
}

/// Run [`migrate_row`] over every stored graph. Logs and continues past a row
/// that fails, so one bad row doesn't leave the rest in plain form.
pub async fn migrate_stored_graphs(db: &sea_orm::DatabaseConnection, key_file: &Path) {
    use sea_orm::EntityTrait;
    let rows = match crate::entity::monitor_pipeline::Entity::find()
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("zm-next: could not scan stored pipeline graphs for secrets: {e}");
            return;
        }
    };
    for row in rows {
        let id = row.monitor_id;
        if let Err(e) = migrate_row(db, key_file, row).await {
            tracing::error!("zm-next: monitor {id}: could not move secrets out of its graph: {e}");
        }
    }
}

/// Decrypt the secrets `graph` references for `monitor_id`.
pub async fn load_secrets<C: ConnectionTrait>(
    db: &C,
    key_file: &Path,
    monitor_id: u32,
    graph: &Value,
) -> AppResult<SecretMap> {
    let wanted = references(graph);
    if wanted.is_empty() {
        return Ok(SecretMap::new());
    }
    let key = SecretKey::load_or_create(key_file).map_err(|e| key_error(key_file, e))?;
    let rows = crate::repo::zmnext_secret::find_by_monitor(db, monitor_id).await?;
    let mut out = SecretMap::new();
    for row in rows.into_iter().filter(|r| wanted.contains(&r.name)) {
        let value = key
            .open(monitor_id, &row.name, &row.ciphertext)
            .map_err(AppError::InternalServerError)?;
        out.insert(row.name, value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Value {
        json!({ "plugins": [
            { "id": "detect", "kind": "decode_detect", "cfg": { "api_key": "not-a-secret-here" },
              "children": [
                { "id": "notify", "kind": "output_webhook",
                  "cfg": { "url": "https://h/x", "auth_header": "Bearer t0k3n" } },
                { "kind": "output_mqtt", "cfg": { "username": "cam", "password": "hunter2" } }
              ] },
            { "id": "review", "kind": "llm_event_review", "config": { "api_key": "sk-123" } }
        ]})
    }

    #[test]
    fn extracts_only_secret_keys_of_their_own_plugin() {
        let mut g = graph();
        let mut got = extract_literals(&mut g);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("notify.auth_header".to_string(), "Bearer t0k3n".to_string()),
                (
                    "plugins.0.children.1.password".to_string(),
                    "hunter2".to_string()
                ),
                ("review.api_key".to_string(), "sk-123".to_string()),
            ]
        );
        let text = g.to_string();
        for secret in ["Bearer t0k3n", "hunter2", "sk-123"] {
            assert!(!text.contains(secret), "{secret} left in {text}");
        }
        // Same key name on a plugin that doesn't treat it as a secret stays put.
        assert_eq!(g["plugins"][0]["cfg"]["api_key"], "not-a-secret-here");
        // Non-secret siblings untouched.
        assert_eq!(g["plugins"][0]["children"][1]["cfg"]["username"], "cam");
        assert_eq!(
            g["plugins"][0]["children"][0]["cfg"]["auth_header"],
            json!({ "$secret": "notify.auth_header" })
        );
        assert_eq!(
            references(&g),
            vec![
                "notify.auth_header",
                "plugins.0.children.1.password",
                "review.api_key"
            ]
        );
    }

    #[test]
    fn extraction_is_idempotent() {
        let mut g = graph();
        extract_literals(&mut g);
        let once = g.clone();
        assert!(extract_literals(&mut g).is_empty());
        assert_eq!(g, once);
    }

    #[test]
    fn resolve_restores_values_and_reports_missing() {
        let original = graph();
        let mut g = original.clone();
        let map: SecretMap = extract_literals(&mut g).into_iter().collect();
        let mut resolved = g.clone();
        resolve_in_place(&mut resolved, &map).unwrap();
        assert_eq!(resolved, original);

        let mut partial = map.clone();
        partial.remove("review.api_key");
        let err = resolve_in_place(&mut g.clone(), &partial).unwrap_err();
        assert!(err.contains("review.api_key"), "{err}");
    }

    #[test]
    fn reference_shape_is_exact() {
        assert_eq!(reference_name(&json!({"$secret": "a"})), Some("a"));
        assert_eq!(reference_name(&json!({"$secret": ""})), None);
        assert_eq!(reference_name(&json!({"$secret": "a", "x": 1})), None);
        assert_eq!(reference_name(&json!("a")), None);
    }

    #[test]
    fn sealed_values_round_trip_and_are_bound_to_their_row() {
        let key = SecretKey::for_tests();
        let sealed = key.seal(3, "notify.auth_header", "Bearer t0k3n");
        assert!(!sealed.contains("t0k3n"));
        assert_eq!(
            key.open(3, "notify.auth_header", &sealed).unwrap(),
            "Bearer t0k3n"
        );
        // Moved to another monitor or name: refuses.
        assert!(key.open(4, "notify.auth_header", &sealed).is_err());
        assert!(key.open(3, "review.api_key", &sealed).is_err());
        // Two seals of the same value differ (fresh nonce).
        assert_ne!(sealed, key.seal(3, "notify.auth_header", "Bearer t0k3n"));
    }

    #[test]
    fn key_file_is_created_private_and_reused() {
        let dir = std::env::temp_dir().join(format!("zm_secret_key_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("k");
        let a = SecretKey::load_or_create(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let b = SecretKey::load_or_create(&path).unwrap();
        let sealed = a.seal(1, "n", "v");
        assert_eq!(b.open(1, "n", &sealed).unwrap(), "v");

        std::fs::write(&path, b"short").unwrap();
        assert!(SecretKey::load_or_create(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
