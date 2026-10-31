//! Device appearance stays separate from provider and Codex configuration.
mod launcher;
mod pixels;
use base64::{Engine, engine::general_purpose::STANDARD};
use emp_state::{atomic_write_private_state, read_file_limited};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) const DEFAULT_PNG: &[u8] =
    include_bytes!("../../../../assets/branding/easy-multi-provider-icon-1024.png");

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Selection {
    preset: String,
    monochrome: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    png: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_png: Option<String>,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            preset: "emp".into(),
            monochrome: false,
            png: None,
            original_png: None,
        }
    }
}

pub(crate) struct Image {
    selection: Selection,
    pub(crate) png: Vec<u8>,
    pub(crate) source_png: Vec<u8>,
    pub(crate) rgba: Vec<u8>,
    pub(crate) size: u32,
    pub(crate) version: String,
}

impl Image {
    fn decode(selection: Selection) -> Result<Self, &'static str> {
        if selection.preset.is_empty() || selection.preset.len() > 40 {
            return Err("app_icon_invalid");
        }
        let png = match &selection.png {
            None if selection.preset == "emp" && !selection.monochrome => DEFAULT_PNG.to_vec(),
            Some(data) if data.len() <= 1_500_000 => {
                STANDARD.decode(data).map_err(|_| "app_icon_invalid")?
            }
            _ => return Err("app_icon_invalid"),
        };
        let (rgba, size) = pixels::decode(&png).map_err(|_| "app_icon_invalid")?;
        let source_png = match &selection.original_png {
            Some(source) if source.len() <= 1_500_000 => {
                let bytes = STANDARD.decode(source).map_err(|_| "app_icon_invalid")?;
                pixels::decode(&bytes).map_err(|_| "app_icon_invalid")?;
                bytes
            }
            Some(_) => return Err("app_icon_invalid"),
            None => png.clone(),
        };
        let version = format!("{:x}", Sha256::digest(&png));
        Ok(Self {
            selection,
            png,
            source_png,
            rgba,
            size,
            version,
        })
    }

    pub(crate) fn metadata(&self) -> Value {
        json!({"preset":self.selection.preset,"monochrome":self.selection.monochrome,
            "version":self.version,"image_url":format!("/assets/app-icon.png?v={}",self.version),
            "source_url":format!("/assets/app-icon-source.png?v={}",self.version)})
    }

    pub(crate) fn tray_rgba(&self) -> Vec<u8> {
        #[cfg(target_os = "macos")]
        if self.selection.png.is_none() {
            let source =
                include_bytes!("../../../../assets/branding/native/easy-multi-provider-tray.png");
            let (rgba, size) = pixels::decode(source).expect("packaged tray icon");
            return pixels::resize(&rgba, size, 48);
        }
        pixels::resize(&self.rgba, self.size, 48)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn templated(&self) -> bool {
        self.selection.monochrome || self.selection.png.is_none()
    }
}

pub(crate) struct AppIcon {
    path: PathBuf,
    image: Mutex<Arc<Image>>,
    pub(crate) revision: AtomicU64,
}

impl AppIcon {
    pub(crate) fn new(root: &Path) -> Self {
        let path = root.join("appearance.json");
        let saved = read_file_limited(&path, 3_200_000)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Selection>(&bytes).ok())
            .and_then(|selection| Image::decode(selection).ok());
        let image = saved
            .unwrap_or_else(|| Image::decode(Selection::default()).expect("packaged EMP icon"));
        Self {
            path,
            image: Mutex::new(Arc::new(image)),
            revision: AtomicU64::new(0),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<Image> {
        Arc::clone(&self.image.lock().expect("app icon"))
    }

    pub(crate) fn save(&self, selection: Selection) -> Result<Value, &'static str> {
        let image = Image::decode(selection)?;
        let bytes = serde_json::to_vec(&image.selection).expect("appearance JSON");
        atomic_write_private_state(&self.path, &bytes).map_err(|_| "app_icon_save_failed")?;
        let root = self
            .path
            .parent()
            .expect("appearance directory")
            .join("app-icons");
        let old = self.snapshot();
        let launcher_updated = launcher::apply(&root, &image).is_ok();
        let metadata = image.metadata();
        *self.image.lock().expect("app icon") = Arc::new(image);
        self.revision.fetch_add(1, Ordering::Release);
        if launcher_updated && old.version != metadata["version"].as_str().expect("icon version") {
            for extension in ["png", "ico"] {
                let _ = std::fs::remove_file(root.join(format!("{}.{}", old.version, extension)));
            }
        }
        Ok(json!({"app_icon":metadata,"launcher_updated":launcher_updated}))
    }
}

#[cfg(test)]
mod tests;
