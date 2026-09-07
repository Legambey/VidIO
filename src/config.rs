//! Configuration persistante, indexée par périphérique.
//!
//! Un fichier unique `~/.config/vidio/config.toml`. Les réglages globaux vivent
//! dans `[general]`, et chaque périphérique de capture a son profil sous
//! `[profiles."<clé stable>"]`. Brancher une autre carte ne touche pas au
//! profil de la première.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::device::DeviceKey;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    /// Profils par périphérique vidéo, clé = [`DeviceKey`].
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Démarrer en plein écran.
    pub fullscreen: bool,
    /// Cantonner l'agrandissement à un facteur entier (pixels carrés nets).
    pub integer_scale: bool,
    /// Mode de présentation souhaité. `Immediate` = latence minimale au prix du
    /// tearing ; on retombe automatiquement sur ce qui est disponible.
    pub present_mode: PresentMode,
    /// Dernier périphérique utilisé, pour rouvrir le même au lancement.
    pub last_device: Option<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            fullscreen: false,
            integer_scale: true,
            present_mode: PresentMode::Immediate,
            last_device: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentMode {
    Immediate,
    Mailbox,
    Fifo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub video: VideoSettings,
    /// Contrôles matériels V4L2, par nom ("brightness", "contrast", ...).
    /// Appliqués côté périphérique : gratuits en CPU et en latence.
    pub hw_controls: BTreeMap<String, i64>,
    /// Corrections appliquées dans le shader, en complément du matériel.
    pub color: ColorSettings,
    pub crt: CrtSettings,
    pub audio: AudioSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// FourCC souhaité ("YUYV", "MJPG"). `None` = on laisse la négociation
    /// choisir, en préférant le format non compressé s'il tient la cadence.
    pub fourcc: Option<String>,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self { width: 1280, height: 720, fps: 60, fourcc: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorSettings {
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub gamma: f32,
}

impl Default for ColorSettings {
    fn default() -> Self {
        Self { brightness: 0.0, contrast: 1.0, saturation: 1.0, gamma: 1.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrtSettings {
    pub enabled: bool,
    /// 0.0 = pas de scanlines, 1.0 = lignes paires éteintes.
    pub scanline: f32,
    /// Masque d'ouverture (aperture grille).
    pub mask: f32,
    /// Courbure de la dalle.
    pub curvature: f32,
    /// Diffusion lumineuse des zones claires.
    pub halation: f32,
}

impl Default for CrtSettings {
    fn default() -> Self {
        Self { enabled: false, scanline: 0.35, mask: 0.3, curvature: 0.06, halation: 0.15 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioSettings {
    /// Nom du périphérique d'entrée (celui de la carte d'acquisition).
    pub input: Option<String>,
    /// Nom de la sortie ; `None` = sortie par défaut du système.
    pub output: Option<String>,
    pub gain: f32,
    pub muted: bool,
    /// Latence visée du tampon de transit, en millisecondes.
    pub target_latency_ms: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { input: None, output: None, gain: 1.0, muted: false, target_latency_ms: 20.0 }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("dossier de configuration introuvable")?;
        Ok(dir.join("vidio").join("config.toml"))
    }

    /// Charge la config, ou renvoie les valeurs par défaut si le fichier
    /// n'existe pas encore. Un fichier illisible est une erreur : on préfère
    /// s'arrêter plutôt qu'écraser silencieusement les réglages de l'utilisateur.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("config illisible : {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("lecture de {}", path.display())),
        }
    }

    /// Écriture atomique : on écrit à côté puis on renomme, pour qu'un crash en
    /// cours de sauvegarde ne laisse pas un fichier tronqué.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("création de {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("sérialisation de la config")?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("écriture de {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("remplacement de {}", path.display()))?;
        Ok(())
    }

    /// Profil d'un périphérique, créé à la volée s'il n'existe pas encore.
    pub fn profile_mut(&mut self, key: &DeviceKey) -> &mut Profile {
        self.profiles.entry(key.as_str().to_string()).or_default()
    }

    pub fn profile(&self, key: &DeviceKey) -> Option<&Profile> {
        self.profiles.get(key.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_profiles() {
        let mut cfg = Config::default();
        let key = DeviceKey::from_by_id("usb-ABC_Capture_1.0-video-index0");
        let profile = cfg.profile_mut(&key);
        profile.video.fps = 60;
        profile.hw_controls.insert("brightness".into(), -8);
        profile.crt.enabled = true;

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();

        let p = back.profile(&key).expect("profil conservé");
        assert_eq!(p.video.fps, 60);
        assert_eq!(p.hw_controls.get("brightness"), Some(&-8));
        assert!(p.crt.enabled);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = Config::load_from(Path::new("/nonexistent/vidio/config.toml")).unwrap();
        assert!(cfg.profiles.is_empty());
        assert!(cfg.general.integer_scale);
    }

    #[test]
    fn by_id_key_drops_node_index() {
        let a = DeviceKey::from_by_id("usb-XYZ_Cam_1.0-video-index0");
        let b = DeviceKey::from_by_id("usb-XYZ_Cam_1.0-video-index1");
        assert_eq!(a, b, "deux nœuds du même appareil partagent le profil");
    }
}
