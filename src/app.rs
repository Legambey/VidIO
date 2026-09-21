//! Boucle applicative : fenêtre, cadencement, entrées clavier.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::{self, AudioControls, AudioDeviceInfo, Passthrough};
use crate::camera::{self, Capture, ControlDesc, FormatCaps, FormatRequest, PixelFormat};
use crate::config::{AudioSettings, ColorSettings, Config, CrtSettings, Profile};
use crate::device::VideoDevice;
use crate::ui::{self, AudioTelemetry, Choice, CrtPreset, Osd, PanelState, Selection, Telemetry};
use crate::video::renderer::{self, Renderer};

/// Réveil de la boucle : une trame est prête.
///
/// C'est le point clé du cadencement. On ne dessine pas au rythme de l'écran en
/// espérant qu'une trame soit là, on dessine quand une trame arrive. Sur un
/// moniteur temps réel, attendre le prochain balayage pour découvrir qu'on a une
/// image en attente, c'est une trame de retard offerte.
#[derive(Debug, Clone, Copy)]
pub enum Wake {
    Frame,
}

/// Une combinaison exploitable, telle que proposée dans le panneau.
struct FormatOption {
    width: u32,
    height: u32,
    fps: u32,
    pixfmt: PixelFormat,
    label: String,
}

struct EguiLayer {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

pub struct App {
    config: Config,
    device: VideoDevice,
    request: FormatRequest,
    profile: Profile,

    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    capture: Option<Capture>,
    egui: Option<EguiLayer>,

    audio: Option<Passthrough>,
    audio_controls: Arc<AudioControls>,
    audio_wanted: bool,

    proxy: EventLoopProxy<Wake>,

    /// Périphériques proposés dans le panneau, relus à son ouverture.
    devices: Vec<VideoDevice>,
    /// Capacités du périphérique courant, relevées à l'ouverture.
    ///
    /// Elles sont retenues plutôt que redemandées : sous Windows, une source
    /// Media Foundation déjà en train de diffuser ne se laisse pas rouvrir pour
    /// être interrogée, et le panneau se retrouverait sans liste de formats.
    /// Sur Linux ça évite simplement d'ouvrir un second descripteur à chaque
    /// ouverture du panneau.
    caps: Vec<FormatCaps>,
    formats: Vec<FormatOption>,
    audio_inputs: Vec<AudioDeviceInfo>,
    audio_outputs: Vec<AudioDeviceInfo>,

    panel_open: bool,
    osd: Option<Osd>,
    hw_controls: Vec<ControlDesc>,
    crt_preset: CrtPreset,
    fullscreen: bool,

    // Cadence et latence mesurées sur les trames réellement présentées.
    frame_times: Vec<Instant>,
    last_latency_ms: f32,
    last_scale: f32,
    dirty: bool,
    announced: bool,
    /// Dernière erreur d'upload signalée, pour ne pas la répéter à chaque trame.
    upload_error: Option<String>,
}

impl App {
    pub fn new(
        config: Config,
        device: VideoDevice,
        request: FormatRequest,
        audio: bool,
        proxy: EventLoopProxy<Wake>,
    ) -> Self {
        let profile = config.profile(&device.key).cloned().unwrap_or_default();
        let audio_controls = Arc::new(AudioControls::default());
        audio_controls.set_gain(profile.audio.gain);
        audio_controls.set_muted(profile.audio.muted);

        Self {
            config,
            device,
            request,
            profile,
            window: None,
            renderer: None,
            capture: None,
            egui: None,
            audio: None,
            audio_controls,
            audio_wanted: audio,
            proxy,
            devices: Vec::new(),
            caps: Vec::new(),
            formats: Vec::new(),
            audio_inputs: Vec::new(),
            audio_outputs: Vec::new(),
            panel_open: false,
            osd: None,
            hw_controls: Vec::new(),
            crt_preset: CrtPreset::Off,
            fullscreen: false,
            frame_times: Vec::new(),
            last_latency_ms: 0.0,
            last_scale: 1.0,
            dirty: false,
            announced: false,
            upload_error: None,
        }
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("VidIO")
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.request.width.max(640),
                self.request.height.max(480),
            ));
        let window = Arc::new(event_loop.create_window(attrs).context("création de la fenêtre")?);

        if self.config.general.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            self.fullscreen = true;
        }

        let renderer = Renderer::new(Arc::clone(&window), self.config.general.present_mode)?;

        let ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            renderer.device(),
            renderer.surface_format(),
            egui_wgpu::RendererOptions::default(),
        );

        // Le thread de capture réveille la boucle à chaque trame.
        let proxy = self.proxy.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send_event(Wake::Frame);
        });

        let camera = camera::open(&self.device)?;
        self.caps = camera.caps().unwrap_or_default();
        let capture = camera.start(self.request.clone(), Some(notify))?;

        // Le profil enregistré s'applique au capteur au démarrage : c'est nous
        // qui rétablissons l'état, le périphérique ne se souvient de rien.
        self.hw_controls = capture.controls().unwrap_or_default();
        self.apply_hw_profile(&capture);

        self.restart_audio();

        self.crt_preset = CrtPreset::ALL
            .iter()
            .copied()
            .find(|p| p.settings() == self.profile.crt)
            .unwrap_or(if self.profile.crt.enabled {
                CrtPreset::Moniteur
            } else {
                CrtPreset::Off
            });

        // Le périphérique réellement ouvert devient celui qu'on rouvrira sans
        // argument. Sans ça, il n'était retenu que si un autre réglage avait
        // par ailleurs marqué la configuration comme modifiée.
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.capture = Some(capture);
        self.egui = Some(EguiLayer { ctx, state: egui_state, renderer: egui_renderer });

        self.remember_device();
        self.refresh_devices();
        Ok(())
    }

    /// Relit la liste des périphériques et les formats du périphérique courant.
    fn refresh_devices(&mut self) {
        self.devices = camera::enumerate().unwrap_or_default();
        self.audio_inputs = audio::enumerate_inputs().unwrap_or_default();
        self.audio_outputs = audio::enumerate_outputs().unwrap_or_default();
        self.formats = self.enumerate_formats();
    }

    /// Aplatit les capacités du périphérique en une liste de choix triée du
    /// plus ambitieux au plus modeste.
    fn enumerate_formats(&self) -> Vec<FormatOption> {
        let mut options = format_options(&self.caps);
        // Proposer un format que le GPU ne peut pas recevoir, c'est promettre
        // un écran noir : la carte annonce jusqu'au 4K, les textures d'un GPU
        // ont une taille maximale, et rien ne les met en rapport ailleurs.
        if let Some(renderer) = &self.renderer {
            let limit = renderer.max_texture_dimension();
            options.retain(|o| {
                let (w, h) = renderer::texture_size(o.pixfmt.published(), o.width, o.height);
                w <= limit && h <= limit
            });
        }
        options
    }

    /// Indice du format actuellement diffusé, s'il figure dans la liste.
    fn current_format_index(&self) -> Option<usize> {
        let capture = self.capture.as_ref()?;
        let live = capture.format();
        self.formats.iter().position(|o| {
            o.width == live.width
                && o.height == live.height
                && o.fps == self.request.fps
                // Un flux compressé est publié décodé : on compare à la demande.
                && Some(o.pixfmt) == self.request.pixfmt.or(Some(o.pixfmt))
        })
    }

    /// Ferme la capture en cours et en ouvre une autre.
    ///
    /// Le profil du périphérique quitté est rangé avant de partir, celui du
    /// nouveau est chargé et appliqué à son capteur.
    fn reopen(&mut self, device: VideoDevice, request: FormatRequest) {
        self.stash_profile();

        // La capture s'arrête à la destruction : on la lâche avant d'ouvrir la
        // suivante, sinon deux flux se disputeraient le même nœud.
        self.capture = None;

        let switching = device.key != self.device.key;
        self.device = device;
        self.request = request;

        if switching {
            self.profile = self.config.profile(&self.device.key).cloned().unwrap_or_default();
            self.audio_controls.set_gain(self.profile.audio.gain);
            self.audio_controls.set_muted(self.profile.audio.muted);
        }

        let proxy = self.proxy.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send_event(Wake::Frame);
        });

        let opened = camera::open(&self.device).and_then(|camera| {
            let caps = camera.caps().unwrap_or_default();
            camera.start(self.request.clone(), Some(notify)).map(|capture| (capture, caps))
        });

        match opened {
            Ok((capture, caps)) => {
                self.caps = caps;
                self.hw_controls = capture.controls().unwrap_or_default();
                self.apply_hw_profile(&capture);
                self.capture = Some(capture);
                self.announced = false;
                self.upload_error = None;
                self.formats = self.enumerate_formats();
                self.remember_device();
                self.notify(self.device.card.clone());
            }
            Err(e) => {
                // Sans capture on garde une fenêtre noire mais utilisable :
                // l'utilisateur peut en choisir une autre dans le panneau.
                self.caps.clear();
                self.formats.clear();
                log::error!("ouverture de {} : {e:#}", self.device.label());
                self.notify(format!("échec : {}", self.device.card));
            }
        }

        if switching {
            self.restart_audio();
        }
    }

    /// Réapplique les contrôles matériels mémorisés pour ce périphérique.
    fn apply_hw_profile(&mut self, capture: &Capture) {
        for (name, value) in &self.profile.hw_controls {
            if let Some(ctrl) = self.hw_controls.iter_mut().find(|c| &c.name == name)
                && capture.set_control(ctrl.id, *value).is_ok()
            {
                ctrl.current = *value;
            }
        }
    }

    fn remember_device(&mut self) {
        // On l'écrit tout de suite plutôt qu'à la fermeture : un plantage ou un
        // `kill` ne doit pas faire oublier quelle carte on utilisait.
        let key = self.device.key.as_str().to_string();
        if self.config.general.last_device.as_deref() != Some(key.as_str()) {
            self.config.general.last_device = Some(key);
            if let Err(e) = self.config.save() {
                log::warn!("mémorisation du périphérique : {e:#}");
            }
        }
    }

    /// (Re)démarre le transit audio d'après le profil courant.
    ///
    /// Si un périphérique mémorisé a disparu — carte débranchée, PCM occupé —
    /// on n'abandonne que celui-là. Lâcher les deux d'un coup, comme on le
    /// faisait, faisait retomber sur « par défaut » un choix d'entrée
    /// parfaitement valable dès que la sortie, elle, ne s'ouvrait pas.
    ///
    /// Renvoie `false` quand un des deux choix a dû être abandonné : l'appelant
    /// n'annonce alors pas le périphérique comme s'il avait été retenu.
    fn restart_audio(&mut self) -> bool {
        self.audio = None;
        if !self.audio_wanted {
            return true;
        }

        let latency = self.profile.audio.target_latency_ms;
        let input = self.profile.audio.input.clone();
        let output = self.profile.audio.output.clone();
        let controls = Arc::clone(&self.audio_controls);

        // Du plus fidèle au moins exigeant. Les combinaisons déjà tentées sont
        // sautées : sans choix mémorisé, il n'y a qu'un essai à faire.
        let attempts = [
            (input.as_deref(), output.as_deref()),
            (input.as_deref(), None),
            (None, output.as_deref()),
            (None, None),
        ];
        let mut tried: Vec<(Option<&str>, Option<&str>)> = Vec::new();
        let mut started = None;
        let mut failure = None;
        for attempt in attempts {
            if tried.contains(&attempt) {
                continue;
            }
            tried.push(attempt);
            match Passthrough::start(attempt.0, attempt.1, latency, Arc::clone(&controls)) {
                Ok(pass) => {
                    started = Some((pass, attempt));
                    break;
                }
                Err(e) => {
                    log::warn!("transit audio refusé : {e:#}");
                    failure.get_or_insert(e);
                }
            }
        }

        let Some((pass, (kept_input, kept_output))) = started else {
            // Pas de son n'est pas une raison de ne pas afficher l'image.
            if let Some(e) = failure {
                log::warn!("transit audio indisponible : {e:#}");
            }
            return false;
        };

        let mut dropped = Vec::new();
        if input.is_some() && kept_input.is_none() {
            self.profile.audio.input = None;
            self.dirty = true;
            dropped.push("entrée");
        }
        if output.is_some() && kept_output.is_none() {
            self.profile.audio.output = None;
            self.dirty = true;
            dropped.push("sortie");
        }
        // Le journal ne disait pas quels périphériques avaient été retenus :
        // impossible, en relisant une session, de savoir si un décrochage
        // venait de la carte ou du serveur de son.
        log::info!("transit audio : {} → {}", pass.input_name, pass.output_name);
        self.audio = Some(pass);

        if dropped.is_empty() {
            return true;
        }
        self.notify(format!(
            "{} audio indisponible — retour au réglage système",
            dropped.join(" et ")
        ));
        false
    }

    /// Range le profil courant dans la configuration en mémoire.
    fn stash_profile(&mut self) {
        let key = self.device.key.clone();
        *self.config.profile_mut(&key) = self.profile.clone();
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = &self.window else { return };
        self.fullscreen = !self.fullscreen;
        window.set_fullscreen(
            self.fullscreen.then(|| Fullscreen::Borderless(None)),
        );
        self.config.general.fullscreen = self.fullscreen;
        self.dirty = true;
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.osd = Some(Osd::new(text));
    }

    /// Applique un pas relatif à un réglage et l'annonce à l'écran.
    fn nudge(&mut self, label: &str, delta: f32, range: (f32, f32), field: fn(&mut ColorSettings) -> &mut f32) {
        let value = field(&mut self.profile.color);
        *value = (*value + delta).clamp(range.0, range.1);
        let shown = *value;
        self.dirty = true;
        self.notify(format!("{label} {shown:.2}"));
    }

    fn handle_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        match key {
            Key::Named(NamedKey::Tab) | Key::Named(NamedKey::F1) => {
                self.panel_open = !self.panel_open;
                if self.panel_open {
                    // On relit tout à l'ouverture : les contrôles sont globaux
                    // au périphérique et une carte a pu être branchée depuis.
                    if let Some(capture) = &self.capture
                        && let Ok(controls) = capture.controls()
                    {
                        self.hw_controls = controls;
                    }
                    self.refresh_devices();
                }
            }
            Key::Named(NamedKey::Escape) => {
                if self.panel_open {
                    self.panel_open = false;
                } else if self.fullscreen {
                    self.toggle_fullscreen();
                } else {
                    event_loop.exit();
                }
            }
            Key::Named(NamedKey::ArrowUp) => self.change_gain(0.05),
            Key::Named(NamedKey::ArrowDown) => self.change_gain(-0.05),
            Key::Character(c) => match c.as_str() {
                "f" | "F" => self.toggle_fullscreen(),
                "[" => self.nudge("luminosité", -0.02, (-0.5, 0.5), |c| &mut c.brightness),
                "]" => self.nudge("luminosité", 0.02, (-0.5, 0.5), |c| &mut c.brightness),
                ";" => self.nudge("contraste", -0.05, (0.0, 3.0), |c| &mut c.contrast),
                "'" => self.nudge("contraste", 0.05, (0.0, 3.0), |c| &mut c.contrast),
                "-" => self.nudge("saturation", -0.05, (0.0, 3.0), |c| &mut c.saturation),
                "=" => self.nudge("saturation", 0.05, (0.0, 3.0), |c| &mut c.saturation),
                "," => self.nudge("gamma", -0.05, (0.3, 3.0), |c| &mut c.gamma),
                "." => self.nudge("gamma", 0.05, (0.3, 3.0), |c| &mut c.gamma),
                "c" | "C" => {
                    self.profile.crt.enabled = !self.profile.crt.enabled;
                    let on = self.profile.crt.enabled;
                    self.dirty = true;
                    self.notify(if on { "filtre CRT activé" } else { "filtre CRT désactivé" });
                }
                "s" | "S" => self.cycle_preset(),
                "i" | "I" => {
                    self.config.general.integer_scale = !self.config.general.integer_scale;
                    let on = self.config.general.integer_scale;
                    self.dirty = true;
                    self.notify(if on {
                        "agrandissement entier"
                    } else {
                        "agrandissement proportionnel"
                    });
                }
                "m" | "M" => {
                    let muted = self.audio_controls.toggle_mute();
                    self.profile.audio.muted = muted;
                    self.dirty = true;
                    self.notify(if muted { "son coupé" } else { "son rétabli" });
                }
                "r" | "R" => {
                    self.profile.color = ColorSettings::default();
                    self.dirty = true;
                    self.notify("image réinitialisée");
                }
                "q" | "Q" => event_loop.exit(),
                _ => {}
            },
            _ => {}
        }
    }

    /// Applique ce que l'overlay a demandé, une fois l'emprunt d'egui relâché.
    ///
    /// Rien n'est fait pendant le dessin : ouvrir un périphérique ou remettre
    /// un profil à zéro passe par des méthodes qui empruntent tout `self`,
    /// que le panneau tient encore par morceaux.
    fn apply_actions(&mut self, actions: ui::Actions, gain: f32, muted: bool) {
        if gain != self.audio_controls.gain() {
            self.audio_controls.set_gain(gain);
            self.profile.audio.gain = gain;
            self.dirty = true;
        }
        if muted != self.audio_controls.muted() {
            self.audio_controls.set_muted(muted);
            self.profile.audio.muted = muted;
            self.dirty = true;
        }
        if let Some((id, value)) = actions.set_hw_control
            && let Some(capture) = &self.capture
        {
            if let Err(e) = capture.set_control(id, value) {
                log::warn!("contrôle matériel refusé : {e:#}");
            } else if let Some((name, is_switch)) = self
                .hw_controls
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.name.clone(), matches!(c.kind, camera::ControlKind::Boolean)))
            {
                self.profile.hw_controls.insert(name, value);
                self.dirty = true;

                // Un interrupteur — « exposition automatique », « balance des
                // blancs automatique » — décide si le réglage qu'il pilote
                // accepte encore une valeur. Sans relecture, le curseur voisin
                // resterait grisé (ou actif) à tort jusqu'à la prochaine
                // ouverture du panneau. Les curseurs, eux, ne changent l'état de
                // personne : les relire à chaque pixel de glissement coûterait
                // une énumération complète des contrôles par trame.
                if is_switch
                    && let Ok(controls) = capture.controls()
                {
                    self.hw_controls = controls;
                }
            }
        }
        if actions.reset_color || actions.reset_all {
            self.profile.color = ColorSettings::default();
            self.dirty = true;
        }
        if actions.reset_crt || actions.reset_all {
            self.profile.crt = CrtSettings::default();
            self.crt_preset = CrtPreset::Off;
            self.dirty = true;
        }
        if actions.reset_hw || actions.reset_all {
            self.reset_hw_controls();
        }
        if actions.reset_audio || actions.reset_all {
            self.profile.audio = AudioSettings::default();
            self.audio_controls.set_gain(self.profile.audio.gain);
            self.audio_controls.set_muted(self.profile.audio.muted);
            self.dirty = true;
            self.restart_audio();
        }
        if actions.reset_all {
            self.notify("profil réinitialisé");
        } else if actions.reset_color {
            self.notify("image réinitialisée");
        } else if actions.reset_crt {
            self.notify("filtre CRT réinitialisé");
        } else if actions.reset_hw {
            self.notify("contrôles du capteur réinitialisés");
        } else if actions.reset_audio {
            self.notify("audio réinitialisé");
        }

        if actions.toggle_fullscreen {
            self.toggle_fullscreen();
        }
        if actions.refresh_devices {
            self.refresh_devices();
            self.notify(format!("{} périphérique(s) trouvé(s)", self.devices.len()));
        }
        if let Some(index) = actions.select_device
            && let Some(device) = self.devices.get(index).cloned()
        {
            let profile = self.config.profile(&device.key).cloned().unwrap_or_default();
            let request = FormatRequest {
                width: profile.video.width,
                height: profile.video.height,
                fps: profile.video.fps,
                pixfmt: profile.video.fourcc.as_deref().map(fourcc_of),
                buffers: self.request.buffers,
            };
            self.reopen(device, request);
        }
        if let Some(index) = actions.select_format
            && let Some(option) = self.formats.get(index)
        {
            let request = FormatRequest {
                width: option.width,
                height: option.height,
                fps: option.fps,
                pixfmt: Some(option.pixfmt),
                buffers: self.request.buffers,
            };
            // Le format choisi devient celui du profil : c'est lui qu'on
            // rouvrira la prochaine fois sur ce périphérique.
            self.profile.video.width = option.width;
            self.profile.video.height = option.height;
            self.profile.video.fps = option.fps;
            self.profile.video.fourcc = Some(option.pixfmt.name());
            self.dirty = true;
            let device = self.device.clone();
            self.reopen(device, request);
        }
        if let Some(index) = actions.select_audio_input {
            self.profile.audio.input =
                index.checked_sub(1).and_then(|i| self.audio_inputs.get(i)).map(|d| d.id.clone());
            self.dirty = true;
            if self.restart_audio() {
                self.notify(match &self.audio {
                    Some(p) => format!("entrée : {}", p.input_name),
                    None => "entrée audio indisponible".into(),
                });
            }
        }
        if let Some(index) = actions.select_audio_output {
            self.profile.audio.output =
                index.checked_sub(1).and_then(|i| self.audio_outputs.get(i)).map(|d| d.id.clone());
            self.dirty = true;
            if self.restart_audio() {
                self.notify(match &self.audio {
                    Some(p) => format!("sortie : {}", p.output_name),
                    None => "sortie audio indisponible".into(),
                });
            }
        }
        if actions.save {
            self.save();
        }
    }

    /// Remet les contrôles du capteur aux valeurs par défaut du pilote, et
    /// oublie ce que le profil en avait mémorisé — sans quoi la prochaine
    /// ouverture les rétablirait.
    fn reset_hw_controls(&mut self) {
        let Some(capture) = &self.capture else { return };
        for ctrl in self.hw_controls.iter_mut() {
            if ctrl.read_only || ctrl.current == ctrl.default {
                continue;
            }
            match capture.set_control(ctrl.id, ctrl.default) {
                Ok(()) => ctrl.current = ctrl.default,
                Err(e) => log::warn!("remise à zéro de « {} » refusée : {e:#}", ctrl.name),
            }
        }
        self.profile.hw_controls.clear();
        self.dirty = true;
    }

    fn change_gain(&mut self, delta: f32) {
        let gain = (self.audio_controls.gain() + delta).clamp(0.0, 2.0);
        self.audio_controls.set_gain(gain);
        self.profile.audio.gain = gain;
        self.dirty = true;
        self.notify(format!("volume {:.0} %", gain * 100.0));
    }

    fn cycle_preset(&mut self) {
        let index = CrtPreset::ALL.iter().position(|p| *p == self.crt_preset).unwrap_or(0);
        self.crt_preset = CrtPreset::ALL[(index + 1) % CrtPreset::ALL.len()];
        self.profile.crt = self.crt_preset.settings();
        self.dirty = true;
        self.notify(format!("filtre : {}", self.crt_preset.label()));
    }

    /// Cadence des trames effectivement présentées, sur la dernière seconde.
    fn displayed_fps(&mut self) -> f32 {
        let now = Instant::now();
        self.frame_times.retain(|t| now.duration_since(*t) < Duration::from_secs(1));
        self.frame_times.len() as f32
    }

    fn telemetry(&mut self) -> Telemetry {
        let fps = self.displayed_fps();
        let (format, captured, dropped, errors, missed) = match &self.capture {
            Some(c) => {
                let f = c.format();
                let s = c.stats();
                (
                    format!("{}x{} {}", f.width, f.height, f.pixfmt.name()),
                    s.captured.load(Ordering::Relaxed),
                    s.dropped.load(Ordering::Relaxed),
                    s.errors.load(Ordering::Relaxed),
                    s.missed.load(Ordering::Relaxed),
                )
            }
            None => ("—".into(), 0, 0, 0, 0),
        };

        let audio = self.audio.as_ref().map(|p| AudioTelemetry {
            buffer_ms: p.latency_ms(),
            drift_ppm: p.stats.drift_ppm(),
            underruns: p.stats.underruns.load(Ordering::Relaxed),
            xruns: p.stats.xruns.load(Ordering::Relaxed),
        });

        let (present_mode, adapter) = match &self.renderer {
            Some(r) => (format!("{:?}", r.present_mode), r.adapter_name.clone()),
            None => ("—".into(), "—".into()),
        };

        Telemetry {
            format,
            displayed_fps: fps,
            captured,
            dropped,
            errors,
            missed,
            latency_ms: self.last_latency_ms,
            scale: self.last_scale,
            present_mode,
            adapter,
            audio,
        }
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };

        // Trame la plus récente. S'il n'y en a pas de nouvelle, on redessine
        // quand même : l'overlay, lui, a pu changer.
        if let (Some(capture), Some(renderer)) = (&self.capture, self.renderer.as_mut())
            && let Some(frame) = capture.try_take()
        {
            self.last_latency_ms = frame.captured_at.elapsed().as_secs_f32() * 1000.0;
            match renderer.upload(&frame) {
                Ok(()) => self.upload_error = None,
                Err(e) => {
                    // Une trame qui échoue échoue en général à 30 par seconde :
                    // on ne le signale qu'au changement.
                    let message = format!("{e:#}");
                    if self.upload_error.as_deref() != Some(message.as_str()) {
                        log::warn!("upload de trame : {message}");
                        // `notify` emprunterait tout `self`, que la capture et
                        // le rendu tiennent encore ; l'OSD est relevé juste
                        // après, il recevra le message.
                        self.osd = Some(Osd::new(message.clone()));
                        self.upload_error = Some(message);
                    }
                }
            }
            self.frame_times.push(Instant::now());
            capture.recycle(frame);
        }

        let telemetry = self.telemetry();
        let osd = self.osd.take();

        // Les listes du panneau, préparées avant l'emprunt de l'overlay.
        // L'indice 0 des listes audio est toujours « par défaut système » :
        // c'est le repli quand rien n'est mémorisé ou que le matériel a changé.
        let device_choices: Vec<Choice> =
            self.devices.iter().map(|d| Choice::new(d.label())).collect();
        let device_index = self.devices.iter().position(|d| d.key == self.device.key);
        let format_choices: Vec<Choice> =
            self.formats.iter().map(|f| Choice::new(f.label.clone())).collect();
        let format_index = self.current_format_index();

        let mut input_choices = vec![Choice::new("par défaut")];
        input_choices.extend(self.audio_inputs.iter().map(|d| Choice::new(&d.name)));
        let input_index = self
            .profile
            .audio
            .input
            .as_deref()
            .and_then(|id| self.audio_inputs.iter().position(|d| d.id == id).map(|i| i + 1))
            .unwrap_or(0);

        let mut output_choices = vec![Choice::new("par défaut")];
        output_choices.extend(self.audio_outputs.iter().map(|d| Choice::new(&d.name)));
        let output_index = self
            .profile
            .audio
            .output
            .as_deref()
            .and_then(|id| self.audio_outputs.iter().position(|d| d.id == id).map(|i| i + 1))
            .unwrap_or(0);

        // On construit l'interface avant de toucher au moteur de rendu : les
        // deux empruntent des champs distincts, mais l'ordre garde le code lisible.
        // Les actions de l'overlay passent par des méthodes qui empruntent tout
        // `self` : on les remonte du bloc et on les applique une fois l'emprunt
        // d'egui relâché.
        let (paint_jobs, textures_delta, screen, actions, gain, muted) = {
            let Some(egui) = self.egui.as_mut() else { return };
            let raw_input = egui.state.take_egui_input(&window);

            let mut gain = self.audio_controls.gain();
            let mut muted = self.audio_controls.muted();
            let mut actions = ui::Actions::default();

            let output = egui.ctx.run_ui(raw_input, |root| {
                actions = ui::draw(
                    root,
                    self.panel_open,
                    osd.as_ref(),
                    PanelState {
                        color: &mut self.profile.color,
                        crt: &mut self.profile.crt,
                        general: &mut self.config.general,
                        gain: &mut gain,
                        muted: &mut muted,
                        hw_controls: &mut self.hw_controls,
                        telemetry: &telemetry,
                        selection: Selection {
                            devices: &device_choices,
                            device: device_index,
                            formats: &format_choices,
                            format: format_index,
                            audio_inputs: &input_choices,
                            audio_input: input_index,
                            audio_outputs: &output_choices,
                            audio_output: output_index,
                        },
                    },
                );
            });

            egui.state.handle_platform_output(&window, output.platform_output);
            let jobs = egui.ctx.tessellate(output.shapes, output.pixels_per_point);
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [window.inner_size().width, window.inner_size().height],
                pixels_per_point: output.pixels_per_point,
            };
            (jobs, output.textures_delta, screen, actions, gain, muted)
        };

        // L'OSD n'est remis en place que s'il n'a pas expiré, pour ne pas le
        // faire réapparaître à chaque redessin.
        self.osd = osd;
        self.apply_actions(actions, gain, muted);

        let Some(renderer) = self.renderer.as_mut() else { return };
        let Some(egui) = self.egui.as_mut() else { return };

        // Les textures d'egui sont appliquées ici, hors de la passe de rendu.
        // Un `TexturesDelta` non consommé panique à la destruction, et le rendu
        // a plusieurs sorties légitimes — surface périmée, fenêtre masquée — qui
        // laisseraient le delta sur le carreau.
        let mut textures_delta = textures_delta;
        for (id, deltas) in &textures_delta.set {
            for delta in deltas {
                egui.renderer.update_texture(renderer.device(), renderer.queue(), *id, delta);
            }
        }
        let freed = std::mem::take(&mut textures_delta.free);
        textures_delta.clear();

        let color = self.profile.color.clone();
        let crt = self.profile.crt.clone();
        let integer = self.config.general.integer_scale;

        let result = renderer.render(&color, &crt, integer, |device, queue, encoder, view| {
            egui.renderer.update_buffers(device, queue, encoder, &paint_jobs, &screen);

            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("vidio-overlay"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // On conserve la vidéo déjà tracée.
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            egui.renderer.render(&mut pass, &paint_jobs, &screen);
        });

        for id in &freed {
            egui.renderer.free_texture(id);
        }

        match result {
            Ok(viewport) => {
                self.last_scale = viewport.scale;
                if !self.announced && viewport.width > 0 {
                    self.announced = true;
                    if let Some(capture) = &self.capture {
                        let f = capture.format();
                        log::info!(
                            "première image affichée : {}x{} {} → {}x{} (×{:.2}), latence {:.1} ms",
                            f.width,
                            f.height,
                            f.pixfmt.name(),
                            viewport.width,
                            viewport.height,
                            viewport.scale,
                            self.last_latency_ms,
                        );
                    }
                }
            }
            Err(e) => log::error!("rendu : {e:#}"),
        }
    }

    fn save(&mut self) {
        self.config.general.last_device = Some(self.device.key.as_str().to_string());
        self.stash_profile();
        match self.config.save() {
            Ok(()) => {
                self.dirty = false;
                self.notify("configuration enregistrée");
            }
            Err(e) => log::error!("enregistrement : {e:#}"),
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.start(event_loop) {
            log::error!("démarrage impossible : {e:#}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // L'overlay voit les évènements en premier : quand le panneau est
        // ouvert, la souris et le clavier lui appartiennent.
        let consumed = match (self.egui.as_mut(), self.window.as_ref()) {
            (Some(egui), Some(window)) => {
                let response = egui.state.on_window_event(window, &event);
                if response.repaint {
                    window.request_redraw();
                }
                response.consumed
            }
            _ => false,
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::KeyboardInput { event, .. } if !consumed => {
                if event.state == ElementState::Pressed && !event.repeat {
                    self.handle_key(&event.logical_key, event_loop);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Bilan de session : la première chose qu'on regarde quand l'affichage
        // n'a pas la tête qu'on attendait.
        if let Some(capture) = &self.capture {
            let s = capture.stats();
            log::info!(
                "session : {} trames capturées, {} écrasées, {} perdues par le pilote, {} erreurs, {:.1} ms de latence en fin de course",
                s.captured.load(Ordering::Relaxed),
                s.dropped.load(Ordering::Relaxed),
                s.missed.load(Ordering::Relaxed),
                s.errors.load(Ordering::Relaxed),
                self.last_latency_ms,
            );
        }

        // La capture s'arrête à la destruction ; on écrit la configuration
        // seulement si quelque chose a bougé.
        self.capture = None;
        self.audio = None;
        if self.dirty {
            self.save();
        }
    }
}

/// Aplatit les capacités en une liste de choix, de la plus ambitieuse à la
/// plus modeste. C'est l'ordre dans lequel on veut les lire : la première ligne
/// de la liste doit être celle qu'on prendrait si on ne réfléchissait pas.
fn format_options(caps: &[crate::camera::FormatCaps]) -> Vec<FormatOption> {
    let mut options: Vec<FormatOption> = caps
        .iter()
        .flat_map(|c| {
            c.fps.iter().map(move |fps| FormatOption {
                width: c.width,
                height: c.height,
                fps: *fps,
                pixfmt: c.pixfmt,
                label: format!("{}x{} {} Hz {}", c.width, c.height, fps, c.pixfmt.name()),
            })
        })
        .collect();

    options.sort_by_key(|o| {
        (
            std::cmp::Reverse(o.width * o.height),
            std::cmp::Reverse(o.fps),
            o.pixfmt.name(),
        )
    });
    options.dedup_by(|a, b| a.label == b.label);
    options
}

/// Convertit un code à quatre lettres en format de pixel.
fn fourcc_of(text: &str) -> PixelFormat {
    let mut code = [b' '; 4];
    for (dst, src) in code.iter_mut().zip(text.bytes()) {
        *dst = src;
    }
    PixelFormat::from_fourcc(code)
}

/// Lance l'application.
pub fn run(config: Config, device: VideoDevice, request: FormatRequest, audio: bool) -> Result<()> {
    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .context("création de la boucle d'évènements")?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(config, device, request, audio, proxy);
    event_loop.run_app(&mut app).context("boucle d'évènements")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::FormatCaps;

    fn caps() -> Vec<FormatCaps> {
        vec![
            FormatCaps { pixfmt: PixelFormat::Yuyv, width: 640, height: 480, fps: vec![30, 15] },
            FormatCaps { pixfmt: PixelFormat::Mjpeg, width: 1280, height: 720, fps: vec![60, 30] },
            FormatCaps { pixfmt: PixelFormat::Mjpeg, width: 640, height: 480, fps: vec![30] },
        ]
    }

    #[test]
    fn format_list_runs_from_most_to_least_ambitious() {
        let options = format_options(&caps());
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "1280x720 60 Hz MJPG",
                "1280x720 30 Hz MJPG",
                "640x480 30 Hz MJPG",
                "640x480 30 Hz YUYV",
                "640x480 15 Hz YUYV",
            ]
        );
    }

    #[test]
    fn every_frame_rate_gets_its_own_entry() {
        // Une résolution annoncée à 60 et 30 Hz doit se choisir à l'une ou à
        // l'autre : c'est le réglage qui compte le plus sur une carte
        // d'acquisition, et le masquer forcerait à passer par le TOML.
        let options = format_options(&caps());
        let hd: Vec<u32> =
            options.iter().filter(|o| o.width == 1280).map(|o| o.fps).collect();
        assert_eq!(hd, [60, 30]);
    }

    #[test]
    fn empty_capabilities_yield_no_choice() {
        assert!(format_options(&[]).is_empty());
    }
}
