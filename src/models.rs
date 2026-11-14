//! The model registry: which ONNX depth models exist and where they live on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const BUILTIN_REGISTRY: &str = include_str!("../models/models.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// Larger = nearer (relative inverse depth).
    Disparity,
    /// Larger = farther.
    Depth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub file: String,
    #[serde(default)]
    pub notes: String,
    pub output: OutputKind,
    #[serde(default = "default_patch")]
    pub patch: u32,
    #[serde(default = "default_mean")]
    pub mean: [f32; 3],
    #[serde(default = "default_std")]
    pub std: [f32; 3],
    #[serde(default)]
    pub fetch: Option<String>,
}

fn default_patch() -> u32 {
    14
}
fn default_mean() -> [f32; 3] {
    [0.485, 0.456, 0.406]
}
fn default_std() -> [f32; 3] {
    [0.229, 0.224, 0.225]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpscalerInfo {
    pub id: String,
    pub name: String,
    pub file: String,
    pub scale: u32,
    #[serde(default = "one")]
    pub multiple: u32,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub fetch: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Deserialize)]
struct RegistryFile {
    model: Vec<ModelInfo>,
    #[serde(default)]
    upscaler: Vec<UpscalerInfo>,
}

#[derive(Clone, Debug)]
pub struct Registry {
    pub dir: PathBuf,
    pub models: Vec<ModelInfo>,
    pub upscalers: Vec<UpscalerInfo>,
}

impl Registry {
    /// Find the models folder and read its `models.toml` (falls back to the built-in list).
    pub fn discover() -> Self {
        let dir = find_models_dir();
        let file = std::fs::read_to_string(dir.join("models.toml"))
            .ok()
            .and_then(|s| parse(&s).ok())
            .unwrap_or_else(|| parse(BUILTIN_REGISTRY).expect("built-in registry is valid"));
        Self {
            dir,
            models: file.model,
            upscalers: file.upscaler,
        }
    }

    pub fn path(&self, m: &ModelInfo) -> PathBuf {
        self.dir.join(&m.file)
    }

    pub fn upscaler_path(&self, u: &UpscalerInfo) -> PathBuf {
        self.dir.join(&u.file)
    }

    pub fn upscaler_installed(&self, u: &UpscalerInfo) -> bool {
        self.upscaler_path(u).is_file()
    }

    /// Resolve an upscaler choice ("" = best installed: the first installed entry).
    pub fn resolve_upscaler(&self, id: &str) -> Result<&UpscalerInfo> {
        let u = if id.is_empty() {
            self.upscalers
                .iter()
                .find(|u| self.upscaler_installed(u))
                .context("no AI upscaler is installed.\nRun once: scripts/make-upscalers.sh")?
        } else {
            self.upscalers
                .iter()
                .find(|u| u.id == id)
                .with_context(|| format!("unknown upscaler '{id}' (see `stereowright models`)"))?
        };
        anyhow::ensure!(
            self.upscaler_installed(u),
            "upscaler '{}' is not built yet (expected {}).\nRun once: scripts/make-upscalers.sh {}",
            u.id,
            self.upscaler_path(u).display(),
            u.fetch.as_deref().unwrap_or("")
        );
        Ok(u)
    }

    pub fn is_installed(&self, m: &ModelInfo) -> bool {
        self.path(m).is_file()
    }

    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The best installed model (the last installed entry in the registry).
    pub fn best_installed(&self) -> Option<&ModelInfo> {
        self.models.iter().rev().find(|m| self.is_installed(m))
    }

    /// Resolve a user choice ("" = automatic) to an installed model.
    pub fn resolve(&self, id: &str) -> Result<&ModelInfo> {
        if id.is_empty() {
            return self.best_installed().with_context(|| {
                format!(
                    "no depth model is installed in {}.\nRun once: scripts/fetch-models.sh",
                    self.dir.display()
                )
            });
        }
        let m = self
            .get(id)
            .with_context(|| format!("unknown model '{id}' (see `stereowright models`)"))?;
        if !self.is_installed(m) {
            anyhow::bail!(
                "model '{}' is not downloaded (expected {}).\nRun once: scripts/fetch-models.sh {}",
                m.id,
                self.path(m).display(),
                m.fetch.as_deref().unwrap_or("")
            );
        }
        Ok(m)
    }
}

fn parse(s: &str) -> Result<RegistryFile> {
    Ok(toml::from_str::<RegistryFile>(s)?)
}

fn has_models(dir: &Path) -> bool {
    dir.join("models.toml").is_file()
        || std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "onnx"))
            })
            .unwrap_or(false)
}

/// Search order: $STEREOWRIGHT_MODELS, next to the executable (and the project root when
/// running from target/), the current directory, then ~/.local/share/stereowright/models.
fn find_models_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("STEREOWRIGHT_MODELS") {
        return PathBuf::from(p);
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        for anc in exe.ancestors().skip(1).take(4) {
            candidates.push(anc.join("models"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("models"));
    }
    let data = data_dir().join("models");
    candidates.push(data.clone());
    candidates
        .into_iter()
        .find(|d| has_models(d))
        .unwrap_or(data)
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("stereowright")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses() {
        let reg = parse(BUILTIN_REGISTRY).unwrap();
        assert!(reg.model.len() >= 3);
        assert!(reg.model.iter().all(|m| m.patch == 14));
        assert!(reg.upscaler.len() >= 3);
        assert!(reg.upscaler.iter().all(|u| u.scale >= 2));
    }
}
