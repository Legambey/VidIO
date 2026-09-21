//! Inventaire des PCM ALSA réels, lu dans `/proc/asound`.
//!
//! ALSA n'expose pas des périphériques mais des *noms* de PCM, et cpal en
//! ajoute encore : il fusionne les « hints » (`default:CARD=x`,
//! `sysdefault:CARD=x`, `front:CARD=x,DEV=0`…) avec une sonde matérielle qui
//! recrée `hw:` et `plughw:` pour chaque carte. Une même entrée physique
//! ressort donc cinq fois, sous cinq noms différents mais avec le même
//! descriptif — d'où une liste illisible dans l'overlay.
//!
//! Pour regrouper ces routes il faut savoir laquelle mène où, ce que la liste
//! de cpal ne dit pas : les hints désignent la carte par son identifiant
//! (`CARD=sofhdadsp`) et la sonde par son index (`CARD=0`). `/proc/asound`
//! donne la correspondance, et au passage le nom du sous-périphérique
//! (« HDA Analog », « DMIC », « HDMI1 ») que cpal laisse vide sur cette
//! machine.

use std::collections::HashMap;

/// Sur les plateformes sans ALSA, l'inventaire reste vide : `card_index` et
/// `label` renvoient alors `None`, et l'appelant retombe sur le descriptif que
/// cpal donne lui-même — lequel, sous WASAPI, est déjà le nom lisible du
/// point de terminaison.
#[derive(Default)]
pub struct Catalog {
    /// Identifiant de carte (`sofhdadsp`) → index (`0`).
    card_index: HashMap<String, u32>,
    /// Index de carte → nom lisible (`sof-hda-dsp`).
    card_name: HashMap<u32, String>,
    /// (carte, device) → nom du PCM (`HDA Analog`).
    pcm_name: HashMap<(u32, u32), String>,
}

impl Catalog {
    /// Index de la carte désignée par un jeton `CARD=…`, qui est tantôt un
    /// index, tantôt un identifiant.
    pub fn card_index(&self, token: &str) -> Option<u32> {
        if let Ok(index) = token.parse::<u32>() {
            return self.card_name.contains_key(&index).then_some(index);
        }
        self.card_index.get(token).copied().or_else(|| {
            self.card_index
                .iter()
                .find(|(id, _)| id.eq_ignore_ascii_case(token))
                .map(|(_, &index)| index)
        })
    }

    /// Libellé d'un sous-périphérique : « carte — PCM », ou juste la carte
    /// quand ALSA ne nomme pas le PCM.
    pub fn label(&self, card: u32, dev: u32) -> Option<String> {
        let card_name = self.card_name.get(&card)?;
        match self.pcm_name.get(&(card, dev)) {
            Some(pcm) => Some(format!("{card_name} — {pcm}")),
            None => Some(card_name.clone()),
        }
    }
}

#[cfg(target_os = "linux")]
pub fn load() -> Catalog {
    let mut catalog = Catalog::default();
    if let Ok(text) = std::fs::read_to_string("/proc/asound/cards") {
        parse_cards(&text, &mut catalog);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/asound/pcm") {
        parse_pcm(&text, &mut catalog);
    }
    catalog
}

#[cfg(not(target_os = "linux"))]
pub fn load() -> Catalog {
    Catalog::default()
}

/// ` 0 [sofhdadsp      ]: sof-hda-dsp - sof-hda-dsp`
/// suivi d'une ligne indentée de nom long, qu'on ignore.
#[cfg(any(target_os = "linux", test))]
fn parse_cards(text: &str, catalog: &mut Catalog) {
    for line in text.lines() {
        let line = line.trim_start();
        let Some((index, rest)) = line.split_once(' ') else { continue };
        let Ok(index) = index.parse::<u32>() else { continue };
        let Some((id, rest)) = rest.trim_start().strip_prefix('[').and_then(|r| r.split_once(']'))
        else {
            continue;
        };
        let rest = rest.trim_start_matches(':').trim();
        // « pilote - nom » : le nom seul suffit, le pilote est redondant.
        let name = rest.split_once(" - ").map(|(_, name)| name).unwrap_or(rest).trim();
        let name = if name.is_empty() { id.trim() } else { name };
        catalog.card_index.insert(id.trim().to_string(), index);
        catalog.card_name.insert(index, name.to_string());
    }
}

/// `00-06: DMIC (*) :  : capture 1`
#[cfg(any(target_os = "linux", test))]
fn parse_pcm(text: &str, catalog: &mut Catalog) {
    for line in text.lines() {
        let Some((addr, rest)) = line.trim().split_once(':') else { continue };
        let Some((card, dev)) = addr.split_once('-') else { continue };
        let (Ok(card), Ok(dev)) = (card.parse::<u32>(), dev.parse::<u32>()) else { continue };
        let name = rest.split(" : ").next().unwrap_or("").replace("(*)", "");
        let name = name.trim();
        if !name.is_empty() {
            catalog.pcm_name.insert((card, dev), name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [sofhdadsp      ]: sof-hda-dsp - sof-hda-dsp\n\
                          \x20                     HP-HPEliteBook840G8NotebookPC-SBKPF-880D\n\
                          1 [Video          ]: USB-Audio - USB3.0 Video\n\
                          \x20                     MACROSILICON USB3.0 Video at usb-0000:00:14.0-2\n";

    const PCM: &str = "00-00: HDA Analog (*) :  : playback 1 : capture 1\n\
                       00-03: HDMI1 (*) : HDMI 1 : playback 1\n\
                       00-06: DMIC (*) :  : capture 1\n\
                       01-00: USB Audio : USB Audio : capture 1\n";

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        parse_cards(CARDS, &mut catalog);
        parse_pcm(PCM, &mut catalog);
        catalog
    }

    #[test]
    fn resolves_card_by_id_or_index() {
        let catalog = catalog();
        assert_eq!(catalog.card_index("sofhdadsp"), Some(0));
        assert_eq!(catalog.card_index("Video"), Some(1));
        assert_eq!(catalog.card_index("1"), Some(1));
        assert_eq!(catalog.card_index("absente"), None);
        // Un index hors inventaire ne doit pas être pris pour argent comptant.
        assert_eq!(catalog.card_index("7"), None);
    }

    #[test]
    fn labels_join_card_and_pcm() {
        let catalog = catalog();
        assert_eq!(catalog.label(0, 6).as_deref(), Some("sof-hda-dsp — DMIC"));
        assert_eq!(catalog.label(1, 0).as_deref(), Some("USB3.0 Video — USB Audio"));
        // PCM inconnu : la carte seule, plutôt que rien.
        assert_eq!(catalog.label(0, 9).as_deref(), Some("sof-hda-dsp"));
        assert_eq!(catalog.label(9, 0), None);
    }
}
