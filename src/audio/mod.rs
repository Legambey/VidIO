//! Transit audio : on lit l'entrée de la carte d'acquisition et on la rejoue
//! sur la sortie, avec le moins de retard possible.
//!
//! Le piège de cet exercice n'est pas d'ouvrir deux flux, c'est que les deux
//! horloges sont indépendantes : le quartz de la carte USB et celui du DAC ne
//! comptent pas exactement à la même vitesse. Un simple tampon circulaire finit
//! donc immanquablement par déborder ou se vider — en quelques minutes pour
//! quelques dizaines de ppm d'écart. On corrige en rééchantillonnant en continu
//! d'une fraction de pour-cent, ce qui est inaudible, plutôt qu'en jetant ou en
//! dupliquant des paquets, ce qui s'entend.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

mod alsa_catalog;
pub mod cpal_backend;

pub use cpal_backend::{Passthrough, enumerate_inputs, enumerate_outputs};

/// Un périphérique audio tel qu'on le présente à l'utilisateur.
#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    /// Nom lisible.
    pub name: String,
    /// Identifiant persistant fourni par cpal, stable d'une session à l'autre.
    pub id: String,
    pub is_default: bool,
}

/// Réglages modifiables en direct depuis l'overlay, sans couper le flux.
#[derive(Debug)]
pub struct AudioControls {
    gain: AtomicU32,
    muted: AtomicBool,
}

impl Default for AudioControls {
    fn default() -> Self {
        Self { gain: AtomicU32::new(1.0f32.to_bits()), muted: AtomicBool::new(false) }
    }
}

impl AudioControls {
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn toggle_mute(&self) -> bool {
        !self.muted.fetch_xor(true, Ordering::Relaxed)
    }

    /// Gain effectif appliqué par le callback de sortie.
    fn effective_gain(&self) -> f32 {
        if self.muted() { 0.0 } else { self.gain() }
    }
}

/// Compteurs de santé du transit, affichés dans l'overlay.
#[derive(Debug, Default)]
pub struct AudioStats {
    /// Le callback de sortie a manqué d'échantillons : on a joué du silence.
    pub underruns: AtomicU64,
    /// L'entrée a produit plus vite qu'on ne consomme : échantillons perdus.
    pub overruns: AtomicU64,
    /// Remplissage courant du tampon, en trames.
    pub fill_frames: AtomicU32,
    /// Ratio de rééchantillonnage courant, ×1e6 (1_000_000 = neutre).
    pub ratio_ppm: AtomicU32,
    /// Décrochages signalés par ALSA (XRUN) : le flux n'a pas été servi à
    /// temps. À distinguer des deux compteurs ci-dessus, qui portent sur notre
    /// tampon à nous ; ceux-là viennent de la couche en dessous.
    pub xruns: AtomicU64,
}

impl AudioStats {
    /// Latence introduite par le tampon de transit, en millisecondes.
    pub fn buffer_latency_ms(&self, sample_rate: u32) -> f32 {
        if sample_rate == 0 {
            return 0.0;
        }
        self.fill_frames.load(Ordering::Relaxed) as f32 * 1000.0 / sample_rate as f32
    }

    /// Dérive mesurée entre les deux horloges, en parties par million.
    pub fn drift_ppm(&self) -> i32 {
        self.ratio_ppm.load(Ordering::Relaxed) as i32 - 1_000_000
    }
}

/// Asservissement du ratio de rééchantillonnage sur le remplissage du tampon.
///
/// C'est un correcteur proportionnel avec un lissage exponentiel : le
/// remplissage instantané est bruité (les callbacks ne sont pas régulièrement
/// espacés), et on ne veut surtout pas que la hauteur du son suive ce bruit.
/// La correction est bornée pour rester sous le seuil d'audibilité.
#[derive(Debug)]
pub struct DriftController {
    /// Trames d'entrée consommées par trame de sortie, en régime stable.
    base_ratio: f64,
    /// Remplissage visé, en trames.
    target: f64,
    /// Ratio courant, lissé.
    ratio: f64,
    /// Écart relatif maximal toléré (5e-3 = 0,5 %, soit ~9 centièmes de demi-ton).
    max_deviation: f64,
    /// Gain du correcteur.
    gain: f64,
    /// Coefficient de lissage, par mise à jour.
    smoothing: f64,
}

impl DriftController {
    pub fn new(in_rate: u32, out_rate: u32, target_frames: f64) -> Self {
        let base_ratio = in_rate as f64 / out_rate as f64;
        Self {
            base_ratio,
            target: target_frames.max(1.0),
            ratio: base_ratio,
            max_deviation: 0.005,
            gain: 0.05,
            smoothing: 0.02,
        }
    }

    /// Met à jour le ratio à partir du remplissage observé.
    ///
    /// Tampon trop plein → on consomme l'entrée plus vite (ratio > base).
    /// Tampon trop creux → on la consomme plus lentement.
    pub fn update(&mut self, fill_frames: f64) -> f64 {
        let error = (fill_frames - self.target) / self.target;
        let correction = (1.0 + self.gain * error)
            .clamp(1.0 - self.max_deviation, 1.0 + self.max_deviation);
        let goal = self.base_ratio * correction;
        self.ratio += (goal - self.ratio) * self.smoothing;
        self.ratio
    }
}

/// Interpolateur linéaire pour un flux entrelacé.
///
/// Volontairement simple : à des ratios qui restent à 0,5 % de 1,0, le repliement
/// introduit est à −80 dB et très au-dessus de la bande utile. Un polyphase
/// serait du gaspillage ici.
#[derive(Debug)]
pub struct Interpolator {
    channels: usize,
    prev: Vec<f32>,
    next: Vec<f32>,
    /// Position fractionnaire entre `prev` et `next`.
    phase: f64,
    primed: bool,
}

impl Interpolator {
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            prev: vec![0.0; channels],
            next: vec![0.0; channels],
            phase: 0.0,
            primed: false,
        }
    }

    /// Produit une trame de sortie dans `out`, en tirant des trames d'entrée via
    /// `pull` autant que le ratio l'exige. Renvoie `false` en cas de manque
    /// d'échantillons : l'appelant doit alors sortir du silence.
    pub fn next_frame(
        &mut self,
        ratio: f64,
        mut pull: impl FnMut(&mut [f32]) -> bool,
        out: &mut [f32],
    ) -> bool {
        if !self.primed {
            if !pull(&mut self.prev) || !pull(&mut self.next) {
                return false;
            }
            self.primed = true;
        }

        while self.phase >= 1.0 {
            std::mem::swap(&mut self.prev, &mut self.next);
            if !pull(&mut self.next) {
                self.primed = false;
                return false;
            }
            self.phase -= 1.0;
        }

        let t = self.phase as f32;
        for ch in 0..self.channels.min(out.len()) {
            out[ch] = self.prev[ch] + (self.next[ch] - self.prev[ch]) * t;
        }
        self.phase += ratio;
        true
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.primed = false;
    }
}

/// Répartit `src` (canaux d'entrée) sur `dst` (canaux de sortie).
///
/// Le cas qui compte vraiment : une carte d'acquisition qui remonte du mono doit
/// s'entendre des deux côtés, pas seulement à gauche.
pub fn map_channels(src: &[f32], dst: &mut [f32], gain: f32) {
    match (src.len(), dst.len()) {
        (0, _) => dst.fill(0.0),
        (1, _) => dst.fill(src[0] * gain),
        (_, 1) => {
            let sum: f32 = src.iter().sum();
            dst[0] = sum / src.len() as f32 * gain;
        }
        (s, d) => {
            for i in 0..d {
                dst[i] = if i < s { src[i] * gain } else { 0.0 };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_controller_pushes_fill_towards_target() {
        let mut ctl = DriftController::new(48_000, 48_000, 960.0);
        assert!((ctl.update(960.0) - 1.0).abs() < 1e-9, "au repos, aucune correction");

        // Tampon qui déborde : on doit consommer plus vite.
        for _ in 0..500 {
            ctl.update(1400.0);
        }
        assert!(ctl.update(1400.0) > 1.0, "ratio = {}", ctl.update(1400.0));

        // Tampon qui se vide : on ralentit.
        let mut ctl = DriftController::new(48_000, 48_000, 960.0);
        for _ in 0..500 {
            ctl.update(400.0);
        }
        assert!(ctl.update(400.0) < 1.0, "ratio = {}", ctl.update(400.0));
    }

    #[test]
    fn drift_correction_stays_inaudible() {
        let mut ctl = DriftController::new(48_000, 48_000, 960.0);
        for _ in 0..10_000 {
            ctl.update(1_000_000.0); // erreur absurde
        }
        let ratio = ctl.update(1_000_000.0);
        assert!(ratio <= 1.0 + 0.005 + 1e-9, "correction non bornée : {ratio}");
    }

    #[test]
    fn different_rates_give_the_right_base_ratio() {
        let mut ctl = DriftController::new(48_000, 44_100, 960.0);
        assert!((ctl.update(960.0) - 48_000.0 / 44_100.0).abs() < 1e-9);
    }

    #[test]
    fn interpolator_reproduces_a_ramp() {
        let mut interp = Interpolator::new(1);
        let mut src = (0..64).map(|i| i as f32).collect::<Vec<_>>().into_iter();
        let mut out = [0.0f32];

        // Ratio 1.0 : la sortie doit suivre l'entrée sans déformation.
        let mut got = Vec::new();
        for _ in 0..32 {
            let ok = interp.next_frame(
                1.0,
                |frame| match src.next() {
                    Some(v) => {
                        frame[0] = v;
                        true
                    }
                    None => false,
                },
                &mut out,
            );
            assert!(ok);
            got.push(out[0]);
        }
        assert_eq!(got[0], 0.0);
        assert_eq!(got[1], 1.0);
        assert_eq!(got[10], 10.0);
    }

    #[test]
    fn interpolator_signals_starvation() {
        let mut interp = Interpolator::new(1);
        let mut out = [0.0f32];
        assert!(!interp.next_frame(1.0, |_| false, &mut out));
    }

    #[test]
    fn mono_input_reaches_both_speakers() {
        let mut dst = [0.0f32; 2];
        map_channels(&[0.5], &mut dst, 1.0);
        assert_eq!(dst, [0.5, 0.5]);
    }

    #[test]
    fn stereo_downmix_averages() {
        let mut dst = [0.0f32; 1];
        map_channels(&[1.0, 0.0], &mut dst, 1.0);
        assert_eq!(dst[0], 0.5);
    }

    #[test]
    fn mute_zeroes_the_output() {
        let ctl = AudioControls::default();
        ctl.set_gain(0.8);
        assert_eq!(ctl.effective_gain(), 0.8);
        ctl.set_muted(true);
        assert_eq!(ctl.effective_gain(), 0.0);
    }
}
