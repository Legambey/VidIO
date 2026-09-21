//! Identité stable des périphériques.
//!
//! Le point important : `/dev/video0` n'est pas une identité. L'ordre des nœuds
//! dépend de l'ordre d'énumération au boot, et une carte d'acquisition expose
//! souvent plusieurs nœuds (capture + metadata). On construit donc une clé
//! stable qui sert d'index dans la config utilisateur.

use std::fmt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

/// Clé stable d'un périphérique, utilisée comme identifiant de profil TOML.
///
/// Ordre de préférence, sous Linux :
/// 1. le nom `by-id` amputé du suffixe `-video-indexN` — il contient le numéro
///    de série, donc il survit à un changement de port USB ;
/// 2. `carte@bus` — survit à une réinstallation mais pas à un changement de
///    port, et ne distingue pas deux exemplaires identiques du même modèle ;
/// 3. le chemin du nœud, en dernier recours.
///
/// Sous Windows, le lien symbolique du périphérique joue le rôle du `by-id` :
/// il porte le même contenu, sous une autre syntaxe.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceKey(String);

impl DeviceKey {
    #[cfg(any(target_os = "linux", test))]
    pub fn from_by_id(by_id: &str) -> Self {
        // usb-DJJHFA1BIEX0EJ_HP_HD_Camera_01.00.00-video-index0
        //   -> usb-DJJHFA1BIEX0EJ_HP_HD_Camera_01.00.00
        let trimmed = by_id
            .rsplit_once("-video-index")
            .map(|(head, _)| head)
            .unwrap_or(by_id);
        DeviceKey(trimmed.to_string())
    }

    /// Clé tirée du lien symbolique d'un périphérique Windows.
    ///
    /// `\\?\usb#vid_534d&pid_2109&mi_00#7&1f4a2bd5&0&0000#{e5323777-…}\global`
    ///   -> `usb#vid_534d&pid_2109&mi_00#7&1f4a2bd5&0&0000`
    ///
    /// Le préfixe d'espace de noms et le GUID de classe d'interface ne
    /// distinguent rien : ils sont les mêmes pour toutes les caméras. Ce qui
    /// reste identifie l'appareil comme le fait `/dev/v4l/by-id` sur Linux —
    /// pour un appareil qui déclare un numéro de série, le segment du milieu
    /// *est* ce numéro et survit à un changement de port ; sans numéro de
    /// série, Windows y met le chemin du port, et la clé change de prise en
    /// prise. Même compromis qu'avec le repli `carte@bus`.
    #[cfg(any(windows, test))]
    pub fn from_symlink(link: &str) -> Self {
        let trimmed = link.trim_start_matches(r"\\?\");
        let trimmed = trimmed.split_once("#{").map(|(head, _)| head).unwrap_or(trimmed);
        DeviceKey(trimmed.to_ascii_lowercase())
    }

    #[cfg(target_os = "linux")]
    pub fn from_card_bus(card: &str, bus: &str) -> Self {
        DeviceKey(format!("{card}@{bus}"))
    }

    #[cfg(target_os = "linux")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_key_drops_what_identifies_nothing() {
        // Le GUID de classe d'interface et le préfixe d'espace de noms sont les
        // mêmes pour toutes les caméras du système.
        let key = DeviceKey::from_symlink(
            r"\\?\USB#VID_534D&PID_2109&MI_00#7&1f4a2bd5&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global",
        );
        assert_eq!(key.as_str(), "usb#vid_534d&pid_2109&mi_00#7&1f4a2bd5&0&0000");
    }

    #[test]
    fn symlink_key_ignores_the_casing_windows_chose_today() {
        let a = DeviceKey::from_symlink(r"\\?\USB#VID_534D&PID_2109#ABC#{guid}\global");
        let b = DeviceKey::from_symlink(r"\\?\usb#vid_534d&pid_2109#abc#{guid}\global");
        assert_eq!(a, b);
    }
}
