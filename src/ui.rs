//! Overlay egui : panneau de réglages et affichage à l'écran.

use std::time::Instant;

use crate::camera::{ControlDesc, ControlKind};
use crate::config::{ColorSettings, CrtSettings, General};

/// Préréglages du filtre CRT, parcourus par la touche `S`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrtPreset {
    Off,
    Leger,
    Moniteur,
    Arcade,
}

impl CrtPreset {
    pub const ALL: [CrtPreset; 4] =
        [CrtPreset::Off, CrtPreset::Leger, CrtPreset::Moniteur, CrtPreset::Arcade];

    pub fn label(&self) -> &'static str {
        match self {
            CrtPreset::Off => "aucun",
            CrtPreset::Leger => "léger",
            CrtPreset::Moniteur => "moniteur",
            CrtPreset::Arcade => "arcade",
        }
    }

    pub fn settings(&self) -> CrtSettings {
        match self {
            CrtPreset::Off => CrtSettings { enabled: false, ..Default::default() },
            CrtPreset::Leger => CrtSettings {
                enabled: true,
                scanline: 0.2,
                mask: 0.1,
                curvature: 0.0,
                halation: 0.05,
            },
            CrtPreset::Moniteur => CrtSettings {
                enabled: true,
                scanline: 0.4,
                mask: 0.25,
                curvature: 0.03,
                halation: 0.15,
            },
            CrtPreset::Arcade => CrtSettings {
                enabled: true,
                scanline: 0.6,
                mask: 0.45,
                curvature: 0.12,
                halation: 0.3,
            },
        }
    }
}

/// Message éphémère affiché au centre bas de l'écran.
pub struct Osd {
    text: String,
    shown_at: Instant,
}

impl Osd {
    const DURATION: f32 = 2.0;

    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), shown_at: Instant::now() }
    }

    /// Opacité restante, `None` quand le message a expiré.
    fn opacity(&self) -> Option<f32> {
        let age = self.shown_at.elapsed().as_secs_f32();
        if age > Self::DURATION {
            return None;
        }
        // Pleine opacité, puis fondu sur la dernière demi-seconde.
        Some(((Self::DURATION - age) / 0.5).min(1.0))
    }
}

/// Compteurs affichés dans le panneau.
pub struct Telemetry {
    pub format: String,
    pub displayed_fps: f32,
    pub captured: u64,
    pub dropped: u64,
    pub errors: u64,
    /// Trames jamais livrées par le pilote (bande passante USB insuffisante).
    pub missed: u64,
    /// Retard entre le dépilement de la trame et sa présentation.
    pub latency_ms: f32,
    pub scale: f32,
    pub present_mode: String,
    pub adapter: String,
    pub audio: Option<AudioTelemetry>,
}

pub struct AudioTelemetry {
    pub buffer_ms: f32,
    pub drift_ppm: i32,
    pub underruns: u64,
    /// Décrochages signalés par ALSA : le flux n'a pas été servi à temps.
    /// Distinct des manques, qui sont ceux de notre propre tampon.
    pub xruns: u64,
}

/// Une entrée de liste déroulante : une étiquette lisible et rien d'autre.
/// L'overlay ne renvoie que des indices, c'est l'appelant qui sait ce qu'ils
/// désignent — lui seul peut ouvrir un périphérique.
pub struct Choice {
    pub label: String,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into() }
    }
}

/// Listes proposées dans le panneau, avec la position courante.
pub struct Selection<'a> {
    pub devices: &'a [Choice],
    pub device: Option<usize>,
    pub formats: &'a [Choice],
    pub format: Option<usize>,
    /// L'indice 0 vaut toujours « périphérique système par défaut ».
    pub audio_inputs: &'a [Choice],
    pub audio_input: usize,
    pub audio_outputs: &'a [Choice],
    pub audio_output: usize,
}

/// Ce que l'overlay peut modifier. Les changements sont appliqués par
/// l'appelant, qui seul sait joindre le matériel.
#[derive(Default)]
pub struct Actions {
    pub set_hw_control: Option<(u32, i64)>,
    pub save: bool,
    pub reset_color: bool,
    pub reset_crt: bool,
    pub reset_hw: bool,
    pub reset_audio: bool,
    /// Remet tout le profil du périphérique courant à ses valeurs d'origine.
    pub reset_all: bool,
    pub toggle_fullscreen: bool,
    pub select_device: Option<usize>,
    pub select_format: Option<usize>,
    pub select_audio_input: Option<usize>,
    pub select_audio_output: Option<usize>,
    pub refresh_devices: bool,
}

pub struct PanelState<'a> {
    pub color: &'a mut ColorSettings,
    pub crt: &'a mut CrtSettings,
    pub general: &'a mut General,
    pub gain: &'a mut f32,
    pub muted: &'a mut bool,
    pub hw_controls: &'a mut [ControlDesc],
    pub telemetry: &'a Telemetry,
    pub selection: Selection<'a>,
}

/// Dessine l'overlay. Renvoie ce que l'utilisateur a demandé.
pub fn draw(ui: &mut egui::Ui, open: bool, osd: Option<&Osd>, state: PanelState<'_>) -> Actions {
    let mut actions = Actions::default();

    if open {
        draw_panel(ui, state, &mut actions);
    }

    if let Some(osd) = osd
        && let Some(opacity) = osd.opacity()
    {
        draw_osd(ui.ctx(), &osd.text, opacity);
    }

    actions
}

/// Liste déroulante générique : renvoie l'indice choisi, et rien si l'on
/// resélectionne l'entrée courante — inutile de rouvrir un périphérique déjà
/// ouvert.
fn combo(
    ui: &mut egui::Ui,
    label: &str,
    choices: &[Choice],
    current: Option<usize>,
    out: &mut Option<usize>,
) {
    let selected = current
        .and_then(|i| choices.get(i))
        .map(|c| c.label.as_str())
        .unwrap_or("—");

    egui::ComboBox::from_label(label)
        .selected_text(selected)
        .width(210.0)
        .show_ui(ui, |ui| {
            for (index, choice) in choices.iter().enumerate() {
                if ui.selectable_label(current == Some(index), &choice.label).clicked()
                    && current != Some(index)
                {
                    *out = Some(index);
                }
            }
        });
}

fn draw_osd(ctx: &egui::Context, text: &str, opacity: f32) {
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("vidio-osd"))
        .fixed_pos(egui::pos2(screen.center().x, screen.max.y - 90.0))
        .pivot(egui::Align2::CENTER_CENTER)
        .interactable(false)
        .show(ctx, |ui| {
            let alpha = (opacity * 255.0) as u8;
            egui::Frame::NONE
                .fill(egui::Color32::from_black_alpha((opacity * 190.0) as u8))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(text)
                            .size(22.0)
                            .color(egui::Color32::from_white_alpha(alpha)),
                    );
                });
        });
}

/// Bouton de remise aux valeurs par défaut, en fin de section.
///
/// Aligné à gauche comme le reste du contenu : collé à droite, il partait se
/// ranger sous la barre de défilement du panneau, loin des réglages qu'il
/// concerne.
fn reset_button(ui: &mut egui::Ui, flag: &mut bool) {
    ui.add_space(4.0);
    if ui.small_button("réinitialiser").clicked() {
        *flag = true;
    }
}

fn draw_panel(root: &mut egui::Ui, state: PanelState<'_>, actions: &mut Actions) {
    let PanelState { color, crt, general, gain, muted, hw_controls, telemetry, selection } = state;

    egui::Panel::left("vidio-panel")
        .resizable(false)
        .exact_size(330.0)
        .show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("VidIO");
                ui.separator();

                combo(
                    ui,
                    "source",
                    selection.devices,
                    selection.device,
                    &mut actions.select_device,
                );
                combo(
                    ui,
                    "format",
                    selection.formats,
                    selection.format,
                    &mut actions.select_format,
                );
                if ui.small_button("rechercher les périphériques").clicked() {
                    actions.refresh_devices = true;
                }

                ui.add_space(6.0);
                ui.label(&telemetry.format);
                ui.label(format!(
                    "{:.1} img/s affichées · {:.1} ms de latence",
                    telemetry.displayed_fps, telemetry.latency_ms
                ));
                ui.label(format!(
                    "{} capturées · {} écrasées · {} erreurs",
                    telemetry.captured, telemetry.dropped, telemetry.errors
                ));
                if telemetry.missed > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} trames perdues par le pilote — bande passante USB",
                            telemetry.missed
                        ))
                        .color(egui::Color32::from_rgb(230, 160, 60)),
                    );
                }
                ui.label(format!(
                    "×{:.2} · {} · {}",
                    telemetry.scale, telemetry.present_mode, telemetry.adapter
                ));

                ui.add_space(10.0);
                ui.collapsing("Image", |ui| {
                    ui.add(
                        egui::Slider::new(&mut color.brightness, -0.5..=0.5).text("luminosité"),
                    );
                    ui.add(egui::Slider::new(&mut color.contrast, 0.0..=3.0).text("contraste"));
                    ui.add(
                        egui::Slider::new(&mut color.saturation, 0.0..=3.0).text("saturation"),
                    );
                    ui.add(egui::Slider::new(&mut color.gamma, 0.3..=3.0).text("gamma"));
                    reset_button(ui, &mut actions.reset_color);
                });

                ui.collapsing("Filtre CRT", |ui| {
                    ui.checkbox(&mut crt.enabled, "activé");
                    ui.add_enabled_ui(crt.enabled, |ui| {
                        ui.add(
                            egui::Slider::new(&mut crt.scanline, 0.0..=1.0).text("lignes"),
                        );
                        ui.add(egui::Slider::new(&mut crt.mask, 0.0..=1.0).text("masque"));
                        ui.add(
                            egui::Slider::new(&mut crt.curvature, 0.0..=0.3).text("courbure"),
                        );
                        ui.add(
                            egui::Slider::new(&mut crt.halation, 0.0..=1.0).text("halation"),
                        );
                    });
                    reset_button(ui, &mut actions.reset_crt);
                });

                ui.collapsing("Affichage", |ui| {
                    ui.checkbox(&mut general.integer_scale, "agrandissement entier");
                    if ui.button("plein écran (F)").clicked() {
                        actions.toggle_fullscreen = true;
                    }
                });

                ui.collapsing("Audio", |ui| {
                    combo(
                        ui,
                        "entrée",
                        selection.audio_inputs,
                        Some(selection.audio_input),
                        &mut actions.select_audio_input,
                    );
                    combo(
                        ui,
                        "sortie",
                        selection.audio_outputs,
                        Some(selection.audio_output),
                        &mut actions.select_audio_output,
                    );
                    ui.add_space(4.0);
                    if let Some(audio) = &telemetry.audio {
                        ui.label(format!(
                            "tampon {:.1} ms · dérive {:+} ppm · {} manques",
                            audio.buffer_ms, audio.drift_ppm, audio.underruns
                        ));
                        if audio.xruns > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} décrochages ALSA — le flux n'est pas servi à temps",
                                    audio.xruns
                                ))
                                .color(egui::Color32::from_rgb(230, 160, 60)),
                            );
                        }
                    } else {
                        ui.label("aucun transit actif");
                    }
                    ui.add(egui::Slider::new(gain, 0.0..=2.0).text("volume"));
                    ui.checkbox(muted, "muet");
                    reset_button(ui, &mut actions.reset_audio);
                });

                // Les contrôles matériels agissent sur le capteur lui-même : ils
                // ne coûtent rien en latence, mais leur effet est global au
                // périphérique et survit à la fermeture de VidIO.
                ui.collapsing("Matériel (capteur)", |ui| {
                    if hw_controls.is_empty() {
                        ui.label("aucun contrôle exposé");
                    }
                    for ctrl in hw_controls.iter_mut() {
                        let enabled = !ctrl.read_only && !ctrl.inactive;
                        ui.add_enabled_ui(enabled, |ui| match &ctrl.kind {
                            ControlKind::Integer { min, max, .. } => {
                                let mut value = ctrl.current;
                                if ui
                                    .add(egui::Slider::new(&mut value, *min..=*max).text(&ctrl.name))
                                    .changed()
                                {
                                    ctrl.current = value;
                                    actions.set_hw_control = Some((ctrl.id, value));
                                }
                            }
                            ControlKind::Boolean => {
                                let mut value = ctrl.current != 0;
                                if ui.checkbox(&mut value, &ctrl.name).changed() {
                                    ctrl.current = value as i64;
                                    actions.set_hw_control = Some((ctrl.id, ctrl.current));
                                }
                            }
                            ControlKind::Menu { items } => {
                                let current = items
                                    .iter()
                                    .find(|(i, _)| *i as i64 == ctrl.current)
                                    .map(|(_, n)| n.as_str())
                                    .unwrap_or("?");
                                egui::ComboBox::from_label(&ctrl.name)
                                    .selected_text(current)
                                    .show_ui(ui, |ui| {
                                        for (index, name) in items {
                                            if ui
                                                .selectable_label(
                                                    *index as i64 == ctrl.current,
                                                    name,
                                                )
                                                .clicked()
                                            {
                                                ctrl.current = *index as i64;
                                                actions.set_hw_control =
                                                    Some((ctrl.id, ctrl.current));
                                            }
                                        }
                                    });
                            }
                            ControlKind::Button => {
                                if ui.button(&ctrl.name).clicked() {
                                    actions.set_hw_control = Some((ctrl.id, 1));
                                }
                            }
                        });
                    }
                    if !hw_controls.is_empty() {
                        reset_button(ui, &mut actions.reset_hw);
                    }
                });

                ui.add_space(10.0);
                ui.separator();
                if ui.button("enregistrer la configuration").clicked() {
                    actions.save = true;
                }
                ui.add_space(4.0);
                // Une remise à zéro complète efface un réglage patiemment
                // trouvé : elle demande une confirmation, là où celles d'une
                // seule section se refont d'un geste.
                ui.menu_button("tout réinitialiser…", |ui| {
                    ui.label("image, filtre CRT, audio et contrôles du capteur");
                    if ui.button("confirmer").clicked() {
                        actions.reset_all = true;
                        ui.close();
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Tab panneau · F plein écran · [ ] luminosité · ; ' contraste\n\
                         , . gamma · - = saturation · C filtre · S préréglage\n\
                         I agrandissement · ↑↓ volume · M muet · R remise à zéro",
                    )
                    .size(11.0)
                    .weak(),
                );
                ui.add_space(8.0);
            });
        });
}
