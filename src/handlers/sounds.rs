//! ZoneMinder's alarm sounds (#194).
//!
//! The watch page plays `ZM_WEB_ALARM_SOUND` from `ZM_DIR_SOUNDS` when
//! `ZM_WEB_SOUND_ON_ALARM` is on. ZoneMinder's PHP UI serves the file from
//! its web root. A client of zm-api alone has no web root, so this route
//! serves files from that directory.

use std::path::{Path as FsPath, PathBuf};

use axum::{
    body::Body,
    extract::{Path, Request},
    http::{header, HeaderValue},
    response::Response,
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::configure::zmconf::ZmConfig;
use crate::error::{AppError, AppResponseError, AppResult, Resource, ResourceType};

/// Where packages put ZoneMinder's web root when zm.conf doesn't say.
const DEFAULT_PATH_WEB: &str = "/usr/share/zoneminder/www";

/// `ZM_DIR_SOUNDS`, as ZoneMinder reads it: an absolute path, or one
/// relative to the web root (`ZM_PATH_WEB`). The build default is `sounds`.
pub fn sounds_dir(conf: &ZmConfig) -> PathBuf {
    let dir = conf
        .get("ZM_DIR_SOUNDS")
        .filter(|v| !v.is_empty())
        .unwrap_or("sounds");
    if FsPath::new(dir).is_absolute() {
        return PathBuf::from(dir);
    }
    let web = conf
        .get("ZM_PATH_WEB")
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_PATH_WEB);
    FsPath::new(web).join(dir)
}

/// The file `name` names in `dir`, or `None` when the name isn't a plain
/// file name, or the file isn't a regular file inside `dir` once links are
/// followed.
pub fn resolve_sound(dir: &FsPath, name: &str) -> Option<PathBuf> {
    let plain = !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0']);
    if !plain {
        return None;
    }
    let dir = dir.canonicalize().ok()?;
    let file = dir.join(name).canonicalize().ok()?;
    (file.starts_with(&dir) && file.is_file()).then_some(file)
}

fn content_type(file: &FsPath) -> &'static str {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ogg" | "oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "aac") => "audio/aac",
        Some("flac") => "audio/flac",
        Some("opus") => "audio/opus",
        Some("webm") => "audio/webm",
        _ => "application/octet-stream",
    }
}

/// An alarm sound
///
/// - A file from ZoneMinder's sounds directory (`ZM_DIR_SOUNDS`), for the
///   watch page to play on alarm when `ZM_WEB_SOUND_ON_ALARM` is on; the
///   option `ZM_WEB_ALARM_SOUND` names the file.
/// - Takes `?token=` as well as the header, so an `<audio>` element can load it.
#[utoipa::path(
    get,
    path = "/api/v3/system/sounds/{name}",
    params(("name" = String, Path, description = "File name in the sounds directory, e.g. snap.ogg")),
    responses(
        (status = 200, description = "The sound file", content_type = "audio/ogg"),
        (status = 206, description = "The byte range asked for with Range", content_type = "audio/ogg"),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 403, description = "Needs Stream View", body = AppResponseError),
        (status = 404, description = "No such sound", body = AppResponseError)
    ),
    security(("jwt" = [])),
    tag = "System"
)]
pub async fn get_sound(Path(name): Path<String>, req: Request) -> AppResult<Response> {
    let dir = sounds_dir(&ZmConfig::load());
    let not_found = || {
        AppError::NotFoundError(Resource {
            details: vec![("name".to_string(), name.clone())],
            resource_type: ResourceType::File,
        })
    };
    let file = tokio::task::spawn_blocking({
        let name = name.clone();
        move || resolve_sound(&dir, &name)
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(not_found)?;
    // ServeFile answers Range requests (206), which Safari's <audio> needs.
    let served = ServeFile::new(&file)
        .oneshot(req)
        .await
        .map_err(|_| not_found())?;
    let mut response = served.map(Body::new);
    if response.status().is_success() {
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&file)),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn conf(lines: &str) -> ZmConfig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("zm.conf"))
            .unwrap()
            .write_all(lines.as_bytes())
            .unwrap();
        ZmConfig::load_from_path(dir.path().to_str().unwrap())
    }

    #[test]
    fn the_directory_follows_zoneminders_rules() {
        assert_eq!(
            sounds_dir(&conf("")),
            PathBuf::from("/usr/share/zoneminder/www/sounds")
        );
        assert_eq!(
            sounds_dir(&conf("ZM_PATH_WEB=/srv/zm/www\n")),
            PathBuf::from("/srv/zm/www/sounds")
        );
        assert_eq!(
            sounds_dir(&conf("ZM_PATH_WEB=/srv/zm/www\nZM_DIR_SOUNDS=tones\n")),
            PathBuf::from("/srv/zm/www/tones")
        );
        assert_eq!(
            sounds_dir(&conf(
                "ZM_PATH_WEB=/srv/zm/www\nZM_DIR_SOUNDS=/var/lib/zm/sounds\n"
            )),
            PathBuf::from("/var/lib/zm/sounds")
        );
    }

    #[test]
    fn only_plain_names_of_files_inside_the_directory_resolve() {
        let root = tempfile::tempdir().unwrap();
        let sounds = root.path().join("sounds");
        std::fs::create_dir(&sounds).unwrap();
        std::fs::write(sounds.join("snap.ogg"), b"OggS").unwrap();
        std::fs::write(root.path().join("secret.txt"), b"no").unwrap();
        std::fs::create_dir(sounds.join("sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("secret.txt"), sounds.join("link.ogg"))
            .unwrap();

        assert!(resolve_sound(&sounds, "snap.ogg").is_some());
        for bad in [
            "",
            "../secret.txt",
            "..",
            ".hidden",
            "sub",
            "sub/x.ogg",
            "..\\secret.txt",
            "missing.ogg",
            "link.ogg",
        ] {
            assert_eq!(resolve_sound(&sounds, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn audio_types_by_extension() {
        assert_eq!(content_type(FsPath::new("snap.ogg")), "audio/ogg");
        assert_eq!(content_type(FsPath::new("a.WAV")), "audio/wav");
        assert_eq!(content_type(FsPath::new("a.mp3")), "audio/mpeg");
        assert_eq!(content_type(FsPath::new("a")), "application/octet-stream");
    }
}
