use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    device::{
        DeviceInfoMutex, DeviceInfoWithPairing, get_provider, get_provider_from_connection,
        get_usbmuxd,
    },
    error::AppError,
    operation::Operation,
    pairing::{get_sidestore_info, place_file},
};
use isideload::{
    sideload::{application::SpecialApp, sideloader::Sideloader},
    util::callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox},
};
use tauri::{AppHandle, Manager, State, Window};

static SIDELOAD_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct SideloadLock;

impl SideloadLock {
    pub fn acquire() -> Result<Self, AppError> {
        if SIDELOAD_ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(AppError::Misc("Another install is already running".into()));
        }
        Ok(Self)
    }
}

impl Drop for SideloadLock {
    fn drop(&mut self) {
        SIDELOAD_ACTIVE.store(false, Ordering::SeqCst);
    }
}

pub type IloaderSideloader =
    Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>;

pub type SideloaderMutex = Mutex<Option<IloaderSideloader>>;

pub struct SideloaderGuard<'a> {
    state: &'a SideloaderMutex,
    sideloader: Option<IloaderSideloader>,
    generation: u64,
}

impl<'a> SideloaderGuard<'a> {
    pub fn take(state: &'a SideloaderMutex) -> Result<Self, AppError> {
        let mut guard = state.lock().unwrap();
        let generation = crate::account::session_generation();
        let sideloader = guard.take().ok_or(AppError::NotLoggedIn)?;
        Ok(Self {
            state,
            sideloader: Some(sideloader),
            generation,
        })
    }

    pub fn get_mut(
        &mut self,
    ) -> &mut Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox> {
        self.sideloader
            .as_mut()
            .expect("Sideloader should be present")
    }
}

impl Drop for SideloaderGuard<'_> {
    fn drop(&mut self) {
        if self.sideloader.is_none() {
            return;
        }
        let mut guard = self.state.lock().unwrap();
        if guard.is_none() && crate::account::session_generation() == self.generation {
            *guard = self.sideloader.take();
        }
    }
}

pub async fn sideload(
    device_state: State<'_, DeviceInfoMutex>,
    sideloader_state: State<'_, SideloaderMutex>,
    app_path: String,
) -> Result<Option<SpecialApp>, AppError> {
    let device = {
        let device_lock = device_state.lock().unwrap();
        match &*device_lock {
            Some(d) => d.clone(),
            None => return Err(AppError::NoDeviceSelected),
        }
    };

    let provider = get_provider(&device.info).await?;

    let mut sideloader = SideloaderGuard::take(&sideloader_state)?;

    let special = sideloader
        .get_mut()
        .install_app(
            &provider,
            app_path.into(),
            true,
            None::<fn(f32) -> std::future::Ready<()>>,
            Some(&device.pairing),
        )
        .await?;

    Ok(special)
}

pub async fn sideload_with_device(
    sideloader: &mut IloaderSideloader,
    device: &DeviceInfoWithPairing,
    app_path: PathBuf,
) -> Result<Option<SpecialApp>, AppError> {
    let provider = get_provider(&device.info).await?;

    let special = sideloader
        .install_app(
            &provider,
            app_path,
            true,
            None::<fn(f32) -> std::future::Ready<()>>,
            Some(&device.pairing),
        )
        .await?;

    Ok(special)
}

#[tauri::command]
pub async fn sideload_operation(
    app: AppHandle,
    window: Window,
    device_state: State<'_, DeviceInfoMutex>,
    sideloader_state: State<'_, SideloaderMutex>,
    app_path: String,
) -> Result<(), AppError> {
    let _lock = SideloadLock::acquire()?;
    let op = Operation::new("sideload".to_string(), &window);
    op.start("install")?;
    op.fail_if_err(
        "install",
        sideload(device_state, sideloader_state, app_path.clone()).await,
    )?;
    op.complete("install")?;

    if let Err(e) = crate::refresh::remember_ipa(&app, &app_path, true) {
        tracing::warn!("Failed to add imported IPA to refresh library: {e}");
    }

    Ok(())
}

#[tauri::command]
pub async fn install_sidestore_operation(
    handle: AppHandle,
    window: Window,
    device_state: State<'_, DeviceInfoMutex>,
    sideloader_state: State<'_, SideloaderMutex>,
    nightly: bool,
    live_container: bool,
) -> Result<(), AppError> {
    let _lock = SideloadLock::acquire()?;
    let op = Operation::new("install_sidestore".to_string(), &window);
    op.start("download")?;
    // TODO: Cache & check version to avoid re-downloading
    let (filename, url) = if live_container {
        if nightly {
            (
                "LiveContainerSideStore-Nightly.ipa",
                "https://github.com/LiveContainer/LiveContainer/releases/download/nightly/LiveContainer+SideStore.ipa",
            )
        } else {
            (
                "LiveContainerSideStore.ipa",
                "https://github.com/LiveContainer/LiveContainer/releases/latest/download/LiveContainer+SideStore.ipa",
            )
        }
    } else if nightly {
        (
            "SideStore-Nightly.ipa",
            "https://github.com/SideStore/SideStore/releases/download/nightly/SideStore.ipa",
        )
    } else {
        (
            "SideStore.ipa",
            "https://github.com/SideStore/SideStore/releases/latest/download/SideStore.ipa",
        )
    };

    let dest = handle
        .path()
        .temp_dir()
        .map_err(|e| AppError::Filesystem("Failed to get temp dir".into(), e.to_string()))?
        .join(filename);
    op.fail_if_err("download", download(url, &dest).await)?;
    op.move_on("download", "install")?;
    let device = {
        let device_guard = device_state.lock().unwrap();
        match &*device_guard {
            Some(d) => d.clone(),
            None => return op.fail("install", AppError::NoDeviceSelected),
        }
    };
    op.fail_if_err(
        "install",
        sideload(
            device_state,
            sideloader_state,
            dest.to_string_lossy().to_string(),
        )
        .await,
    )?;
    op.move_on("install", "pairing")?;
    let sidestore_info = op.fail_if_err(
        "pairing",
        get_sidestore_info(&device.info, live_container).await,
    )?;
    if let Some(info) = sidestore_info {
        let mut usbmuxd = op.fail_if_err("pairing", get_usbmuxd().await)?;

        let provider = op.fail_if_err(
            "pairing",
            get_provider_from_connection(&device.info, &mut usbmuxd).await,
        )?;

        op.fail_if_err(
            "pairing",
            place_file(device.pairing, &provider, info.bundle_id, info.path).await,
        )?;
    } else {
        return op.fail(
            "pairing",
            AppError::HouseArrest(
                "SideStore's not found".into(),
                "The device did not report SideStore's bundle ID as installed".into(),
            ),
        );
    }

    op.complete("pairing")?;
    Ok(())
}

pub async fn download(url: impl AsRef<str>, dest: &PathBuf) -> Result<(), AppError> {
    let response = reqwest::get(url.as_ref())
        .await
        .map_err(|e| AppError::Download(e.to_string()))?;
    if !response.status().is_success() {
        return Err(AppError::Download(format!(
            "Failed to download file: HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::Download(e.to_string()))?;
    tokio::fs::write(dest, &bytes).await.map_err(|e| {
        AppError::Filesystem("Failed to write downloaded file".into(), e.to_string())
    })?;

    Ok(())
}
