//! Capture vidéo : types communs et plomberie entre le thread de capture et le
//! consommateur (rendu ou CLI).
//!
//! Le backend concret est derrière le trait [`Camera`]. Sur Linux c'est
//! [`v4l2`], en accès direct aux ioctls V4L2 — pas de couche de conversion
//! intermédiaire, et l'intégralité des contrôles matériels de l'appareil.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

#[cfg(target_os = "linux")]
pub mod v4l2;

/// Format de pixel tel qu'il sort de l'appareil, sans conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// YUV 4:2:2 entrelacé, 2 octets par pixel. Uploadable tel quel sur le GPU.
    Yuyv,
    /// Même chose, ordre des composantes inversé.
    Uyvy,
    /// JPEG par trame : compressé, à décoder côté CPU.
    Mjpeg,
    /// YUV 4:2:0 semi-planaire.
    Nv12,
    /// Luminance seule, 8 bits par pixel. Sortie des capteurs infrarouges et
    /// de certaines cartes d'acquisition en mode monochrome.
    Grey,
    Rgb24,
    Bgr24,
    /// RGBA 8 bits par composante. Ne sort pas d'un appareil : c'est ce que
    /// produit l'étage de décodage pour les formats compressés.
    Rgba8,
    Unknown([u8; 4]),
}

impl PixelFormat {
    pub fn from_fourcc(cc: [u8; 4]) -> Self {
        match &cc {
            b"YUYV" | b"YUY2" => Self::Yuyv,
            b"UYVY" => Self::Uyvy,
            b"MJPG" | b"JPEG" => Self::Mjpeg,
            b"NV12" => Self::Nv12,
            b"GREY" | b"Y8  " => Self::Grey,
            b"RGB3" => Self::Rgb24,
            b"BGR3" => Self::Bgr24,
            b"AB24" => Self::Rgba8,
            _ => Self::Unknown(cc),
        }
    }

    pub fn fourcc(&self) -> [u8; 4] {
        match self {
            Self::Yuyv => *b"YUYV",
            Self::Uyvy => *b"UYVY",
            Self::Mjpeg => *b"MJPG",
            Self::Nv12 => *b"NV12",
            Self::Grey => *b"GREY",
            Self::Rgb24 => *b"RGB3",
            Self::Bgr24 => *b"BGR3",
            Self::Rgba8 => *b"AB24",
            Self::Unknown(cc) => *cc,
        }
    }

    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.fourcc()).into_owned()
    }

    /// Un format compressé impose un décodage CPU avant l'envoi au GPU.
    pub fn is_compressed(&self) -> bool {
        matches!(self, Self::Mjpeg)
    }

    /// Le format sous lequel une trame sera publiée, une fois passée par le
    /// thread de capture : un flux compressé y est décodé en RGBA. C'est
    /// celui-là que le rendu voit, et donc celui qui décide de la texture.
    pub fn published(self) -> Self {
        if self.is_compressed() { Self::Rgba8 } else { self }
    }

    /// Préférence de négociation, du meilleur au moins bon. Les formats non
    /// compressés gagnent : zéro décodage, donc zéro latence ajoutée et un CPU
    /// qui reste froid. Le MJPEG ne sert que quand l'USB ne peut pas suivre.
    fn rank(&self) -> u8 {
        match self {
            Self::Yuyv | Self::Uyvy => 0,
            Self::Nv12 | Self::Grey => 1,
            Self::Rgb24 | Self::Bgr24 | Self::Rgba8 => 2,
            Self::Mjpeg => 3,
            Self::Unknown(_) => 4,
        }
    }
}

/// Matrice de conversion YUV → RGB.
///
/// Se tromper ici est très visible : les verts virent et les peaux rosissent.
/// La convention est BT.601 pour la définition standard et BT.709 à partir du
/// 720p — mais une carte d'acquisition qui numérise du composite peut très bien
/// remonter du 601 en haute définition, d'où la lecture de ce que dit le pilote
/// plutôt qu'une déduction à partir de la résolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

/// Convention colorimétrique du flux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorSpec {
    pub matrix: YuvMatrix,
    /// `true` = 0..255 utilisés, `false` = plage réduite 16..235 (le cas des
    /// signaux vidéo). Étirer une plage réduite qu'on a prise pour complète
    /// donne des noirs délavés.
    pub full_range: bool,
}

impl Default for ColorSpec {
    fn default() -> Self {
        Self { matrix: YuvMatrix::Bt709, full_range: false }
    }
}

impl ColorSpec {
    /// Repli quand le pilote ne dit rien : la résolution décide.
    pub fn guess_from_height(height: u32) -> Self {
        Self {
            matrix: if height >= 720 { YuvMatrix::Bt709 } else { YuvMatrix::Bt601 },
            full_range: false,
        }
    }
}

/// Format effectivement négocié avec l'appareil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    pub pixfmt: PixelFormat,
    /// Octets par ligne tels que rapportés par le pilote (0 si compressé).
    pub stride: u32,
    pub color: ColorSpec,
}

/// Ce qu'un appareil sait faire : une combinaison format / taille / cadences.
#[derive(Debug, Clone)]
pub struct FormatCaps {
    pub pixfmt: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Cadences discrètes disponibles, ordre décroissant.
    pub fps: Vec<u32>,
}

/// Ce qu'on demande à l'appareil. Rien n'est garanti : le pilote peut renvoyer
/// un format voisin, d'où le [`FrameFormat`] retourné par la négociation.
#[derive(Debug, Clone)]
pub struct FormatRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixfmt: Option<PixelFormat>,
    /// Nombre de tampons côté pilote. Peu de tampons = moins de latence de file
    /// d'attente ; en dessous de 3 le pilote risque de manquer de tampon libre
    /// et de perdre des trames.
    pub buffers: u32,
}

impl Default for FormatRequest {
    fn default() -> Self {
        Self { width: 1280, height: 720, fps: 60, pixfmt: None, buffers: 3 }
    }
}

impl FormatRequest {
    /// Choisit la meilleure capacité disponible pour cette demande.
    ///
    /// Ordre des critères : la résolution exacte d'abord (on ne veut pas d'un
    /// rééchantillonnage surprise), puis la cadence, puis le format de pixel.
    pub fn pick<'a>(&self, caps: &'a [FormatCaps]) -> Option<&'a FormatCaps> {
        caps.iter()
            .filter(|c| self.pixfmt.is_none_or(|p| p == c.pixfmt))
            .min_by_key(|c| {
                let size_err = (c.width as i64 - self.width as i64).abs()
                    + (c.height as i64 - self.height as i64).abs();
                let best_fps = c.fps.iter().copied().max().unwrap_or(0);
                let fps_err = (best_fps as i64 - self.fps as i64).abs();
                (size_err, fps_err, c.pixfmt.rank())
            })
    }
}

/// Type d'un contrôle matériel, pour savoir quel widget afficher.
#[derive(Debug, Clone)]
pub enum ControlKind {
    Integer { min: i64, max: i64, step: i64 },
    Boolean,
    Menu { items: Vec<(u32, String)> },
    Button,
}

/// Un contrôle exposé par l'appareil (luminosité, exposition, balance...).
///
/// Ces réglages sont appliqués par le capteur ou le pilote : ils ne coûtent ni
/// CPU ni latence, contrairement aux corrections faites dans le shader. Quand
/// l'appareil sait faire, c'est toujours lui qu'il faut préférer.
#[derive(Debug, Clone)]
pub struct ControlDesc {
    pub id: u32,
    pub name: String,
    pub kind: ControlKind,
    pub default: i64,
    pub current: i64,
    /// Modifiable uniquement quand un réglage automatique est désactivé
    /// (typiquement l'exposition manuelle).
    pub inactive: bool,
    pub read_only: bool,
}

/// Accès aux contrôles matériels, utilisable pendant la diffusion.
///
/// Le descripteur de fichier V4L2 supporte `S_CTRL` depuis un autre thread que
/// celui qui dépile les tampons : l'overlay peut donc régler la luminosité du
/// capteur sans interrompre le flux ni passer par une file de commandes.
pub trait CameraControls: Send + Sync {
    fn controls(&self) -> Result<Vec<ControlDesc>>;
    fn set_control(&self, id: u32, value: i64) -> Result<()>;
}

/// Une trame capturée. `data` est réutilisée entre les trames via le pool.
#[derive(Debug)]
pub struct Frame {
    pub data: Vec<u8>,
    pub format: FrameFormat,
    /// Instant du retrait de la file du pilote, pour mesurer la latence bout
    /// en bout jusqu'à la présentation.
    pub captured_at: Instant,
}

/// Compteurs partagés, lus par l'overlay.
#[derive(Debug, Default)]
pub struct Stats {
    pub captured: AtomicU64,
    /// Trames écrasées avant d'avoir été consommées : le rendu ne suit pas.
    pub dropped: AtomicU64,
    /// Erreurs de capture non fatales (EIO transitoire, trame corrompue).
    pub errors: AtomicU64,
    /// Trames que le pilote a comptées mais jamais livrées, détectées par les
    /// trous dans la numérotation. Sur USB c'est le symptôme d'une bande
    /// passante insuffisante : baisser la résolution ou changer de port.
    pub missed: AtomicU64,
}

struct Shared {
    latest: Mutex<Option<Frame>>,
    /// Tampons libres, recyclés pour ne pas allouer 60 fois par seconde.
    pool: Mutex<Vec<Vec<u8>>>,
    ready: Condvar,
    stats: Stats,
    running: AtomicBool,
}

/// Extrémité producteur : le thread de capture y dépose ses trames.
///
/// Politique volontairement « la dernière gagne » : la capture n'attend jamais
/// le consommateur. Si le rendu décroche, la trame en attente est écrasée par
/// la plus récente. Sur un moniteur temps réel c'est ce qu'on veut — accumuler
/// un retard qu'on ne rattrapera jamais est pire que sauter une image.
pub struct FrameSink {
    shared: Arc<Shared>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl FrameSink {
    /// Emprunte un tampon au pool (ou en alloue un) pour la trame suivante.
    pub fn take_buffer(&self, capacity: usize) -> Vec<u8> {
        let mut buf = self.shared.pool.lock().unwrap().pop().unwrap_or_default();
        buf.clear();
        buf.reserve(capacity);
        buf
    }

    /// Publie une trame. L'éventuelle trame non consommée retourne au pool.
    pub fn publish(&self, frame: Frame) {
        {
            let mut slot = self.shared.latest.lock().unwrap();
            if let Some(stale) = slot.replace(frame) {
                self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
                self.recycle_buffer(stale.data);
            }
        }
        self.shared.stats.captured.fetch_add(1, Ordering::Relaxed);
        self.shared.ready.notify_all();
        if let Some(notify) = &self.notify {
            notify();
        }
    }

    pub fn note_error(&self) {
        self.shared.stats.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Signale des trames que le pilote n'a jamais livrées.
    pub fn note_missed(&self, count: u64) {
        self.shared.stats.missed.fetch_add(count, Ordering::Relaxed);
    }

    pub fn should_run(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    fn recycle_buffer(&self, buf: Vec<u8>) {
        let mut pool = self.shared.pool.lock().unwrap();
        // Au-delà de quelques tampons, on a un problème ailleurs : on laisse
        // le surplus être libéré plutôt que de gonfler indéfiniment.
        if pool.len() < 4 {
            pool.push(buf);
        }
    }
}

/// Extrémité consommateur, plus le contrôle du thread de capture.
pub struct Capture {
    shared: Arc<Shared>,
    format: FrameFormat,
    controls: Arc<dyn CameraControls>,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Format réellement négocié — pas forcément celui demandé.
    pub fn format(&self) -> FrameFormat {
        self.format
    }

    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// Contrôles matériels, interrogeables et modifiables en cours de flux.
    pub fn controls(&self) -> Result<Vec<ControlDesc>> {
        self.controls.controls()
    }

    pub fn set_control(&self, id: u32, value: i64) -> Result<()> {
        self.controls.set_control(id, value)
    }

    /// Récupère la dernière trame si elle est nouvelle, sans bloquer.
    pub fn try_take(&self) -> Option<Frame> {
        self.shared.latest.lock().unwrap().take()
    }

    /// Attend une trame, avec plafond. `None` en cas d'expiration.
    pub fn wait_take(&self, timeout: Duration) -> Option<Frame> {
        let slot = self.shared.latest.lock().unwrap();
        let (mut slot, _) = self
            .shared
            .ready
            .wait_timeout_while(slot, timeout, |s| s.is_none())
            .unwrap();
        slot.take()
    }

    /// Rend le tampon d'une trame consommée au pool.
    pub fn recycle(&self, frame: Frame) {
        let mut pool = self.shared.pool.lock().unwrap();
        if pool.len() < 4 {
            let mut buf = frame.data;
            buf.clear();
            pool.push(buf);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Le thread teste `should_run` entre deux trames ; il sort au pire
            // après le délai d'attente d'une trame.
            let _ = t.join();
        }
    }
}

/// Crée la paire producteur / consommateur.
pub fn channel(
    format: FrameFormat,
    controls: Arc<dyn CameraControls>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (FrameSink, Capture) {
    let shared = Arc::new(Shared {
        latest: Mutex::new(None),
        pool: Mutex::new(Vec::new()),
        ready: Condvar::new(),
        stats: Stats::default(),
        running: AtomicBool::new(true),
    });
    let sink = FrameSink { shared: Arc::clone(&shared), notify };
    let capture = Capture { shared, format, controls, thread: None };
    (sink, capture)
}

/// Un appareil ouvert, pas encore en train de diffuser.
pub trait Camera: Send {
    /// Combinaisons format / taille / cadence supportées.
    fn caps(&self) -> Result<Vec<FormatCaps>>;

    /// Poignée vers les contrôles matériels, clonable et partageable.
    fn control_handle(&self) -> Arc<dyn CameraControls>;

    /// Fixe le format sans démarrer le flux ; renvoie ce que le pilote a accepté.
    fn negotiate(&mut self, req: &FormatRequest) -> Result<FrameFormat>;

    /// Démarre la diffusion sur un thread dédié. L'appareil est consommé : le
    /// thread en devient propriétaire, ce qui évite tout partage de descripteur.
    fn start(
        self: Box<Self>,
        req: FormatRequest,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Capture>;
}

/// Énumère les périphériques de capture du système.
pub fn enumerate() -> Result<Vec<crate::device::VideoDevice>> {
    #[cfg(target_os = "linux")]
    {
        v4l2::enumerate()
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("aucun backend de capture pour cette plateforme")
    }
}

/// Ouvre un périphérique par sa clé stable.
pub fn open(device: &crate::device::VideoDevice) -> Result<Box<dyn Camera>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(v4l2::V4l2Camera::open(device.clone())?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        anyhow::bail!("aucun backend de capture pour cette plateforme")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Vec<FormatCaps> {
        vec![
            FormatCaps { pixfmt: PixelFormat::Mjpeg, width: 1280, height: 720, fps: vec![60, 30] },
            FormatCaps { pixfmt: PixelFormat::Yuyv, width: 1280, height: 720, fps: vec![30] },
            FormatCaps { pixfmt: PixelFormat::Yuyv, width: 640, height: 480, fps: vec![60, 30] },
        ]
    }

    #[test]
    fn resolution_beats_pixel_format() {
        let req = FormatRequest { width: 1280, height: 720, fps: 30, ..Default::default() };
        let caps = caps();
        let picked = req.pick(&caps).unwrap();
        assert_eq!((picked.width, picked.height), (1280, 720));
        // À résolution et cadence égales, le non compressé l'emporte.
        assert_eq!(picked.pixfmt, PixelFormat::Yuyv);
    }

    #[test]
    fn explicit_pixel_format_is_honoured() {
        let req = FormatRequest {
            width: 1280,
            height: 720,
            fps: 60,
            pixfmt: Some(PixelFormat::Mjpeg),
            ..Default::default()
        };
        let caps = caps();
        assert_eq!(req.pick(&caps).unwrap().pixfmt, PixelFormat::Mjpeg);
    }

    #[test]
    fn latest_frame_wins_and_buffer_is_recycled() {
        struct NoControls;
        impl CameraControls for NoControls {
            fn controls(&self) -> Result<Vec<ControlDesc>> {
                Ok(Vec::new())
            }
            fn set_control(&self, _id: u32, _value: i64) -> Result<()> {
                Ok(())
            }
        }

        let fmt = FrameFormat {
            width: 2,
            height: 1,
            pixfmt: PixelFormat::Yuyv,
            stride: 4,
            color: ColorSpec::default(),
        };
        let (sink, cap) = channel(fmt, Arc::new(NoControls), None);

        for seq in 0..3u8 {
            let mut data = sink.take_buffer(4);
            data.extend_from_slice(&[seq; 4]);
            sink.publish(Frame { data, format: fmt, captured_at: Instant::now() });
        }

        let frame = cap.try_take().expect("une trame disponible");
        assert_eq!(frame.data[0], 2, "on reçoit la plus récente");
        assert_eq!(cap.stats().dropped.load(Ordering::Relaxed), 2);
        assert!(cap.try_take().is_none(), "la trame n'est servie qu'une fois");
    }
}
