//! Identité stable des périphériques.
//!
//! Le point important : `/dev/video0` n'est pas une identité. L'ordre des nœuds
//! dépend de l'ordre d'énumération au boot, et une carte d'acquisition expose
//! souvent plusieurs nœuds (capture + metadata). On construit donc une clé
//! stable qui sert d'index dans la config utilisateur.

use std::fmt;
use std::path::{Path, PathBuf};

/// Clé stable d'un périphérique, utilisée comme identifiant de profil TOML.
///
/// Ordre de préférence :
/// 1. le nom `by-id` amputé du suffixe `-video-indexN` — il contient le numéro
///    de série, donc il survit à un changement de port USB ;
/// 2. `carte@bus` — survit à une réinstallation mais pas à un changement de
///    port, et ne distingue pas deux exemplaires identiques du même modèle ;
/// 3. le chemin du nœud, en dernier recours.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceKey(String);

impl DeviceKey {
    pub fn from_by_id(by_id: &str) -> Self {
        // usb-DJJHFA1BIEX0EJ_HP_HD_Camera_01.00.00-video-index0
        //   -> usb-DJJHFA1BIEX0EJ_HP_HD_Camera_01.00.00
        let trimmed = by_id
            .rsplit_once("-video-index")
            .map(|(head, _)| head)
            .unwrap_or(by_id);
        DeviceKey(trimmed.to_string())
    }

    pub fn from_card_bus(card: &str, bus: &str) -> Self {
        DeviceKey(format!("{card}@{bus}"))
    }

    pub fn from_path(path: &Path) -> Self {
        DeviceKey(path.display().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Un nœud de capture vidéo utilisable (V4L2_CAP_VIDEO_CAPTURE + STREAMING).
#[derive(Debug, Clone)]
pub struct VideoDevice {
    pub path: PathBuf,
    pub index: usize,
    /// Nom lisible remonté par le pilote ("HP HD Camera").
    pub card: String,
    pub driver: String,
    /// Chemin bus ("usb-0000:00:14.0-2").
    pub bus: String,
    pub by_id: Option<String>,
    pub key: DeviceKey,
}

impl VideoDevice {
    /// Étiquette courte pour l'affichage (menus, listes).
    pub fn label(&self) -> String {
        format!("{} ({})", self.card, self.bus)
    }
}
