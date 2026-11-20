use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use chrono::Utc;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Window};
use tauri_plugin_store::StoreExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    account::login_refresh,
    device::{DeviceInfo, DeviceInfoWithPairing, get_usbmuxd, list_devices},
    error::AppError,
    pairing::{has_stored_rppairing, pairing_file},
    sideload::{IloaderSideloader, SideloadLock, SideloaderMutex, sideload_with_device},
};

static REFRESH_RUNNING: AtomicBool = AtomicBool::new(false);
const DEFAULT_INTERVAL_HOURS: u32 = 120;
const RETRY_AFTER_SECS: i64 = 6 * 60 * 60;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_ANISETTE_SERVER: &str = "mac-ani.jkcoxson.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ManagedApp {
    pub bundle_id: String,
    pub name: String,
    pub ipa_path: String,
    pub added_at: i64,
    pub last_refreshed_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
}

impl Default for ManagedApp {
    fn default() -> Self {
        Self {
            bundle_id: String::new(),
            name: String::new(),
            ipa_path: String::new(),
            added_at: 0,
            last_refreshed_at: None,
            last_attempt_at: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RefreshConfig {
    pub auto_refresh: bool,
    pub interval_hours: u32,
    pub auto_revoke_certs: bool,
    pub apps: Vec<ManagedApp>,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            auto_refresh: false,
            interval_hours: DEFAULT_INTERVAL_HOURS,
            auto_revoke_certs: false,
            apps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshSettings {
    pub auto_refresh: bool,
    pub interval_hours: u32,
    pub auto_revoke_certs: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshProgress<'a> {
    bundle_id: &'a str,
    name: &'a str,
    status: &'a str,
    message: Option<String>,
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    app.path().app_data_dir().map_err(|e| {
        AppError::Filesystem("Failed to get app data directory".into(), e.to_string())
    })
}

fn config_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(app_data_dir(app)?.join("refresh.json"))
}

fn library_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    let dir = app_data_dir(app)?.join("ipa_library");
    fs::create_dir_all(&dir).map_err(|e| {
        AppError::Filesystem("Failed to create IPA library".into(), e.to_string())
    })?;
    Ok(dir)
}

pub fn load_config(app: &AppHandle) -> RefreshConfig {
    let Ok(path) = config_path(app) else {
        return RefreshConfig::default();
    };
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!("Failed to parse refresh config: {e}");
            RefreshConfig::default()
        }),
        Err(_) => RefreshConfig::default(),
    }
}

fn save_config(app: &AppHandle, config: &RefreshConfig) -> Result<(), AppError> {
    let path = config_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            AppError::Filesystem("Failed to create config directory".into(), e.to_string())
        })?;
    }
    let json = serde_json::to_vec_pretty(config)
        .map_err(|e| AppError::Misc(format!("Failed to serialize refresh config: {e}")))?;
    fs::write(&path, json)
        .map_err(|e| AppError::Filesystem("Failed to write refresh config".into(), e.to_string()))
}

fn read_ipa_metadata(path: &Path) -> Result<(String, String), AppError> {
    let file = fs::File::open(path)
        .map_err(|e| AppError::Filesystem("Failed to open IPA".into(), e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::Misc(format!("Invalid IPA archive: {e}")))?;

    let mut info_name: Option<String> = None;
    for index in 0..archive.len() {
        let name = archive
            .by_index(index)
            .map_err(|e| AppError::Misc(format!("Failed to read IPA entry: {e}")))?
            .name()
            .to_string();

        if let Some(rest) = name.strip_prefix("Payload/") {
            let mut parts = rest.split('/');
            let app_dir = parts.next().unwrap_or_default();
            let info = parts.next().unwrap_or_default();
            if app_dir.ends_with(".app") && info == "Info.plist" && parts.next().is_none() {
                info_name = Some(name);
                break;
            }
        }
    }

    let info_name =
        info_name.ok_or_else(|| AppError::Misc("No app Info.plist found inside IPA".into()))?;
    let mut entry = archive
        .by_name(&info_name)
        .map_err(|e| AppError::Misc(format!("Failed to read Info.plist: {e}")))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| AppError::Misc(format!("Failed to read Info.plist: {e}")))?;

    let value: plist::Value = plist::from_bytes(&bytes)
        .map_err(|e| AppError::Misc(format!("Invalid Info.plist: {e}")))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| AppError::Misc("Info.plist is not a dictionary".into()))?;

    let bundle_id = dict
        .get("CFBundleIdentifier")
        .and_then(|value| value.as_string())
        .ok_or_else(|| AppError::Misc("Info.plist is missing CFBundleIdentifier".into()))?
        .to_string();
    let name = dict
        .get("CFBundleDisplayName")
        .or_else(|| dict.get("CFBundleName"))
        .and_then(|value| value.as_string())
        .unwrap_or(&bundle_id)
        .to_string();

    Ok((bundle_id, name))
}

pub fn remember_ipa(
    app: &AppHandle,
    path: &str,
    mark_refreshed: bool,
) -> Result<ManagedApp, AppError> {
    let source = PathBuf::from(path);
    if !source.is_file() {
        return Err(AppError::Filesystem(
            "IPA file not found".into(),
            path.to_string(),
        ));
    }
    if source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| !extension.eq_ignore_ascii_case("ipa"))
        .unwrap_or(true)
    {
        return Err(AppError::Misc("Not an IPA file".into()));
    }

    let (bundle_id, name) = read_ipa_metadata(&source)?;

    let library = library_dir(app)?;
    let safe_id = bundle_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
    let destination = library.join(format!("{safe_id}.ipa"));
    if source != destination {
        fs::copy(&source, &destination).map_err(|e| {
            AppError::Filesystem("Failed to copy IPA to library".into(), e.to_string())
        })?;
    }

    let mut config = load_config(app);
    let now = Utc::now().timestamp();
    let ipa_path = destination.to_string_lossy().to_string();

    if let Some(existing) = config
        .apps
        .iter_mut()
        .find(|candidate| candidate.bundle_id == bundle_id)
    {
        existing.name = name.clone();
        existing.ipa_path = ipa_path;
        existing.last_error = None;
        if mark_refreshed {
            existing.last_refreshed_at = Some(now);
            existing.last_attempt_at = Some(now);
        }
        let result = existing.clone();
        save_config(app, &config)?;
        let _ = app.emit("refresh-library-changed", &result);
        return Ok(result);
    }

    let entry = ManagedApp {
        bundle_id,
        name,
        ipa_path,
        added_at: now,
        last_refreshed_at: mark_refreshed.then_some(now),
        last_attempt_at: mark_refreshed.then_some(now),
        last_error: None,
    };
    config.apps.push(entry.clone());
    save_config(app, &config)?;
    let _ = app.emit("refresh-library-changed", &entry);
    info!("Added {} to refresh library", entry.bundle_id);
    Ok(entry)
}

fn update_app(app: &AppHandle, bundle_id: &str, update: impl FnOnce(&mut ManagedApp)) {
    let mut config = load_config(app);
    if let Some(entry) = config
        .apps
        .iter_mut()
        .find(|candidate| candidate.bundle_id == bundle_id)
    {
        update(entry);
    }
    if let Err(e) = save_config(app, &config) {
        warn!("Failed to save refresh config: {e}");
    }
}

fn is_due(entry: &ManagedApp, interval_hours: u32) -> bool {
    let interval = i64::from(interval_hours.max(1)) * 60 * 60;
    match entry.last_refreshed_at {
        Some(last) => Utc::now().timestamp() - last >= interval,
        None => true,
    }
}

fn can_retry(entry: &ManagedApp, now: i64) -> bool {
    entry
        .last_attempt_at
        .map_or(true, |last| now - last >= RETRY_AFTER_SECS)
}

fn first_saved_email(app: &AppHandle) -> Option<String> {
    let store = app.store("data.json").ok()?;
    let ids = store.get("ids")?;
    ids.as_array()?
        .iter()
        .find_map(|value| value.as_str().map(str::to_string))
}

fn read_preference(app: &AppHandle, key: &str) -> Option<String> {
    let store = app.store("preferences.json").ok()?;
    store.get(key)?.as_str().map(str::to_string)
}

fn resolve_login(app: &AppHandle) -> Result<(String, String, String), AppError> {
    let email = first_saved_email(app).ok_or_else(|| {
        AppError::Misc("No saved Apple ID. Log in from the Apple ID panel first.".into())
    })?;
    let password = Entry::new("iloader", &email)
        .map_err(|e| {
            AppError::KeyringWithMessage("Failed to create credential entry".into(), e.to_string())
        })?
        .get_password()
        .map_err(|e| {
            AppError::KeyringWithMessage(
                "No saved password. Log in again with \"save credentials\" enabled.".into(),
                e.to_string(),
            )
        })?;
    let anisette_server = read_preference(app, "anisetteServer3-1")
        .unwrap_or_else(|| DEFAULT_ANISETTE_SERVER.to_string());

    Ok((email, password, anisette_server))
}

async fn find_paired_device(app: &AppHandle) -> Result<DeviceInfo, AppError> {
    let devices = list_devices().await?;
    for device in devices.into_iter().flatten() {
        match has_stored_rppairing(device.clone(), app.clone()).await {
            Ok(true) => return Ok(device),
            Ok(false) => {}
            Err(e) => debug!("Failed to check pairing for {}: {e}", device.udid),
        }
    }

    Err(AppError::Misc(
        "No paired device connected. Pair a device from the device panel first.".into(),
    ))
}

struct RefreshSideloader {
    app: AppHandle,
    restore: bool,
    sideloader: Option<IloaderSideloader>,
    generation: u64,
}

impl RefreshSideloader {
    async fn acquire(
        app: &AppHandle,
        window: &Window,
        config: &RefreshConfig,
    ) -> Result<Self, AppError> {
        let (existing, generation) = {
            let state = app.state::<SideloaderMutex>();
            let mut guard = state.lock().unwrap();
            let generation = crate::account::session_generation();
            (guard.take(), generation)
        };

        if let Some(sideloader) = existing {
            return Ok(Self {
                app: app.clone(),
                restore: true,
                sideloader: Some(sideloader),
                generation,
            });
        }

        let (email, password, anisette_server) = resolve_login(app)?;
        let sideloader = login_refresh(
            app,
            window.clone(),
            &email,
            &password,
            anisette_server,
            config.auto_revoke_certs,
        )
        .await?;

        Ok(Self {
            app: app.clone(),
            restore: false,
            sideloader: Some(sideloader),
            generation: crate::account::session_generation(),
        })
    }

    fn sideloader_mut(&mut self) -> &mut IloaderSideloader {
        self.sideloader
            .as_mut()
            .expect("sideloader should be present")
    }
}

impl Drop for RefreshSideloader {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        if let Some(sideloader) = self.sideloader.take() {
            let state = self.app.state::<SideloaderMutex>();
            let mut guard = state.lock().unwrap();
            if guard.is_none() && crate::account::session_generation() == self.generation {
                *guard = Some(sideloader);
            }
        }
    }
}

async fn run_refresh(
    app: &AppHandle,
    window: &Window,
    bundle_ids: Vec<String>,
) -> Result<(), AppError> {
    if REFRESH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err(AppError::Misc("A refresh is already running".into()));
    }

    let result = run_refresh_inner(app, window, bundle_ids).await;
    REFRESH_RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn run_refresh_inner(
    app: &AppHandle,
    window: &Window,
    bundle_ids: Vec<String>,
) -> Result<(), AppError> {
    let config = load_config(app);
    let now = Utc::now().timestamp();

    let targets: Vec<ManagedApp> = config
        .apps
        .iter()
        .filter(|entry| bundle_ids.contains(&entry.bundle_id))
        .cloned()
        .collect();

    if targets.is_empty() {
        return Ok(());
    }

    let _lock = SideloadLock::acquire()?;
    let device = find_paired_device(app).await?;
    let mut usbmuxd = get_usbmuxd().await?;
    let pairing = pairing_file(app, &device, &mut usbmuxd, CancellationToken::new()).await?;
    let device = DeviceInfoWithPairing {
        info: device,
        pairing,
    };

    let mut refresh_handle = RefreshSideloader::acquire(app, window, &config).await?;

    for target in targets {
        update_app(app, &target.bundle_id, |entry| {
            entry.last_attempt_at = Some(now);
        });

        app.emit(
            "refresh-progress",
            RefreshProgress {
                bundle_id: &target.bundle_id,
                name: &target.name,
                status: "started",
                message: None,
            },
        )
        .ok();

        let path = PathBuf::from(&target.ipa_path);
        if !path.is_file() {
            let message = "IPA file is missing".to_string();
            update_app(app, &target.bundle_id, |entry| {
                entry.last_error = Some(message.clone());
            });
            app.emit(
                "refresh-progress",
                RefreshProgress {
                    bundle_id: &target.bundle_id,
                    name: &target.name,
                    status: "error",
                    message: Some(message.clone()),
                },
            )
            .ok();
            continue;
        }

        match sideload_with_device(refresh_handle.sideloader_mut(), &device, path).await {
            Ok(_) => {
                let finished_at = Utc::now().timestamp();
                update_app(app, &target.bundle_id, |entry| {
                    entry.last_refreshed_at = Some(finished_at);
                    entry.last_attempt_at = Some(finished_at);
                    entry.last_error = None;
                });
                app.emit(
                    "refresh-progress",
                    RefreshProgress {
                        bundle_id: &target.bundle_id,
                        name: &target.name,
                        status: "success",
                        message: None,
                    },
                )
                .ok();
            }
            Err(error) => {
                let message = error.to_string();
                update_app(app, &target.bundle_id, |entry| {
                    entry.last_attempt_at = Some(Utc::now().timestamp());
                    entry.last_error = Some(message.clone());
                });
                app.emit(
                    "refresh-progress",
                    RefreshProgress {
                        bundle_id: &target.bundle_id,
                        name: &target.name,
                        status: "error",
                        message: Some(message.clone()),
                    },
                )
                .ok();
            }
        }
    }

    Ok(())
}

#[tauri::command]
pub fn refresh_get_config(app: AppHandle) -> RefreshConfig {
    load_config(&app)
}

#[tauri::command]
pub fn refresh_update_settings(
    app: AppHandle,
    settings: RefreshSettings,
) -> Result<RefreshConfig, AppError> {
    let mut config = load_config(&app);
    config.auto_refresh = settings.auto_refresh;
    config.interval_hours = settings.interval_hours.clamp(1, 24 * 30);
    config.auto_revoke_certs = settings.auto_revoke_certs;
    save_config(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub fn refresh_add_ipa(app: AppHandle, path: String) -> Result<ManagedApp, AppError> {
    remember_ipa(&app, &path, false)
}

#[tauri::command]
pub fn refresh_remove_app(app: AppHandle, bundle_id: String) -> Result<RefreshConfig, AppError> {
    let mut config = load_config(&app);
    config.apps.retain(|entry| entry.bundle_id != bundle_id);
    save_config(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub async fn refresh_now(
    app: AppHandle,
    window: Window,
    bundle_ids: Vec<String>,
) -> Result<(), AppError> {
    run_refresh(&app, &window, bundle_ids).await
}

pub fn start_auto_refresh(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            if REFRESH_RUNNING.load(Ordering::SeqCst) {
                continue;
            }

            let config = load_config(&app);
            if !config.auto_refresh || config.apps.is_empty() {
                continue;
            }

            let now = Utc::now().timestamp();
            let due: Vec<String> = config
                .apps
                .iter()
                .filter(|entry| is_due(entry, config.interval_hours) && can_retry(entry, now))
                .map(|entry| entry.bundle_id.clone())
                .collect();
            if due.is_empty() {
                continue;
            }

            let devices = match list_devices().await {
                Ok(devices) => devices,
                Err(error) => {
                    debug!("Auto refresh: unable to list devices: {error}");
                    continue;
                }
            };

            let mut paired = false;
            for device in devices.into_iter().flatten() {
                if has_stored_rppairing(device, app.clone())
                    .await
                    .unwrap_or(false)
                {
                    paired = true;
                    break;
                }
            }
            if !paired {
                continue;
            }

            for bundle_id in &due {
                update_app(&app, bundle_id, |entry| {
                    entry.last_attempt_at = Some(now);
                });
            }

            let Some(webview) = app.get_webview_window("main") else {
                continue;
            };
            let window = webview.as_ref().window();

            info!("Auto refresh triggered for {} app(s)", due.len());
            if let Err(error) = run_refresh(&app, &window, due).await {
                warn!("Auto refresh failed: {error}");
            }
        }
    });
}

#[tauri::command]
pub fn autostart_get(app: AppHandle) -> Result<bool, AppError> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| AppError::Misc(format!("Failed to read autostart state: {e}")))
}

#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), AppError> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|e| AppError::Misc(format!("Failed to update autostart: {e}")))
}

pub async fn trigger_refresh(app: AppHandle) {
    let Some(webview) = app.get_webview_window("main") else {
        return;
    };
    let window = webview.as_ref().window();
    let bundle_ids: Vec<String> = load_config(&app)
        .apps
        .into_iter()
        .map(|entry| entry.bundle_id)
        .collect();
    if bundle_ids.is_empty() {
        return;
    }
    if let Err(error) = run_refresh(&app, &window, bundle_ids).await {
        warn!("Refresh triggered from tray failed: {error}");
    }
}
