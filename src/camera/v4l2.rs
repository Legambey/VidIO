//! Backend Linux : accès direct à V4L2.
//!
//! On parle aux ioctls sans passer par `libv4lconvert` : aucune conversion de
//! format n'est faite dans notre dos, ce qui est justement le but — le YUYV
//! part tel quel sur le GPU, et on sait toujours ce qu'on manipule.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use v4l::buffer::Type as BufType;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::{CaptureStream, Stream as StreamTrait};
use v4l::video::Capture as CaptureTrait;
use v4l::video::capture::Parameters;
use v4l::{Device, Format, FourCC, control};

use super::{
    Camera, CameraControls, Capture, ColorSpec, ControlDesc, ControlKind, FormatCaps,
    FormatRequest, Frame, FrameFormat, FrameSink, PixelFormat, YuvMatrix,
};
use crate::video::decode::MjpegDecoder;
use crate::device::{DeviceKey, VideoDevice};

/// Énumère les nœuds capables de capture vidéo en streaming.
///
/// Une carte d'acquisition expose souvent plusieurs nœuds : un pour les trames,
/// les autres pour les métadonnées. On ne garde que ceux qui savent capturer.
pub fn enumerate() -> Result<Vec<VideoDevice>> {
    let by_id = by_id_map();
    let mut out = Vec::new();

    for node in v4l::context::enum_devices() {
        let path = node.path().to_path_buf();
        let Ok(dev) = Device::with_path(&path) else {
            continue; // nœud occupé ou permissions insuffisantes
        };
        let Ok(caps) = dev.query_caps() else { continue };

        let flags = caps.capabilities;
        let can_capture = flags.contains(v4l::capability::Flags::VIDEO_CAPTURE)
            && flags.contains(v4l::capability::Flags::STREAMING);
        if !can_capture {
            continue;
        }

        let by_id_name = by_id.get(&path).cloned();
        let key = match &by_id_name {
            Some(name) => DeviceKey::from_by_id(name),
            None if !caps.bus.is_empty() => DeviceKey::from_card_bus(&caps.card, &caps.bus),
            None => DeviceKey::from_path(&path),
        };

        out.push(VideoDevice {
            index: node.index(),
            path,
            card: caps.card,
            driver: caps.driver,
            bus: caps.bus,
            by_id: by_id_name,
            key,
        });
    }

    Ok(out)
}

/// Associe chaque nœud à son nom `by-id` (qui contient le numéro de série).
fn by_id_map() -> HashMap<PathBuf, String> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/dev/v4l/by-id") else {
        return map;
    };
    for entry in entries.flatten() {
        let Ok(target) = fs::canonicalize(entry.path()) else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        map.insert(target, name);
    }
    map
}

/// Retrouve un périphérique par sa clé, son chemin ou son index.
pub fn find(spec: &str) -> Result<VideoDevice> {
    let devices = enumerate()?;
    devices
        .iter()
        .find(|d| d.key.as_str() == spec)
        .or_else(|| devices.iter().find(|d| d.path.to_string_lossy() == spec))
        .or_else(|| {
            spec.parse::<usize>()
                .ok()
                .and_then(|i| devices.iter().find(|d| d.index == i))
        })
        .or_else(|| devices.iter().find(|d| d.card.contains(spec)))
        .cloned()
        .ok_or_else(|| anyhow!("aucun périphérique ne correspond à « {spec} »"))
}

/// Traduit la colorimétrie annoncée par le pilote.
///
/// `Default` est fréquent — beaucoup de pilotes UVC ne renseignent rien — et on
/// retombe alors sur la déduction par la résolution.
fn color_spec(fmt: &Format) -> ColorSpec {
    use v4l::format::{Colorspace, Quantization};

    let matrix = match fmt.colorspace {
        Colorspace::Rec709 | Colorspace::SRGB => Some(YuvMatrix::Bt709),
        Colorspace::SMPTE170M | Colorspace::NTSC | Colorspace::EBUTech3212 => {
            Some(YuvMatrix::Bt601)
        }
        // Le JPEG impose YCbCr en plage complète avec la matrice BT.601.
        Colorspace::JPEG => Some(YuvMatrix::Bt601),
        _ => None,
    };

    let full_range = match fmt.quantization {
        Quantization::FullRange => true,
        Quantization::LimitedRange => false,
        Quantization::Default => matches!(fmt.colorspace, Colorspace::JPEG),
    };

    match matrix {
        Some(matrix) => ColorSpec { matrix, full_range },
        None => ColorSpec { full_range, ..ColorSpec::guess_from_height(fmt.height) },
    }
}

pub struct V4l2Camera {
    info: VideoDevice,
    dev: Arc<Device>,
}

impl V4l2Camera {
    pub fn open(info: VideoDevice) -> Result<Self> {
        let dev = Device::with_path(&info.path)
            .with_context(|| format!("ouverture de {}", info.path.display()))?;
        Ok(Self { info, dev: Arc::new(dev) })
    }
}

/// Poignée de contrôles : ne détient que le descripteur, donc clonable et
/// partageable avec le thread de capture.
struct V4l2Controls {
    dev: Arc<Device>,
}

/// Types de contrôle V4L2 qu'on sait présenter (cf. `V4L2_CTRL_TYPE_*`).
/// Tout ce qui vaut 0x0100 et plus est composé (matrices, rectangles) et n'a
/// pas de widget évident : on le laisse de côté.
mod ctrl_type {
    pub const INTEGER: u32 = 1;
    pub const BOOLEAN: u32 = 2;
    pub const MENU: u32 = 3;
    pub const BUTTON: u32 = 4;
    pub const INTEGER64: u32 = 5;
    pub const CTRL_CLASS: u32 = 6;
    pub const BITMASK: u32 = 8;
    pub const INTEGER_MENU: u32 = 9;
}

impl V4l2Controls {
    /// Énumère les contrôles via `VIDIOC_QUERY_EXT_CTRL`.
    ///
    /// On refait ce que `Device::query_controls` fait déjà, pour une raison
    /// précise : cette méthode convertit le type de contrôle avec un `unwrap`
    /// et panique donc sur tout type qu'elle ne connaît pas — dont
    /// `V4L2_CTRL_TYPE_RECT`, que la moindre webcam UVC récente expose pour sa
    /// région d'intérêt. Ici on ignore ce qu'on ne sait pas afficher.
    fn enumerate_raw(&self) -> Result<Vec<(v4l::v4l_sys::v4l2_query_ext_ctrl, String)>> {
        use v4l::v4l_sys::{V4L2_CTRL_FLAG_NEXT_COMPOUND, V4L2_CTRL_FLAG_NEXT_CTRL};

        let fd = self.dev.handle().fd();
        let mut out = Vec::new();
        let mut query: v4l::v4l_sys::v4l2_query_ext_ctrl = unsafe { std::mem::zeroed() };

        loop {
            query.id |= V4L2_CTRL_FLAG_NEXT_CTRL | V4L2_CTRL_FLAG_NEXT_COMPOUND;
            let res = unsafe {
                v4l::v4l2::ioctl(
                    fd,
                    v4l::v4l2::vidioc::VIDIOC_QUERY_EXT_CTRL,
                    &mut query as *mut _ as *mut std::os::raw::c_void,
                )
            };

            match res {
                Ok(()) => {
                    let name = unsafe { std::ffi::CStr::from_ptr(query.name.as_ptr()) }
                        .to_string_lossy()
                        .into_owned();
                    out.push((query, name));
                }
                // EINVAL marque simplement la fin de la liste.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput && !out.is_empty() => break,
                Err(e) => return Err(e).context("VIDIOC_QUERY_EXT_CTRL"),
            }
        }

        Ok(out)
    }

    /// Énumère les entrées d'un contrôle de type menu.
    ///
    /// Le pilote a le droit de renvoyer EINVAL pour un index intermédiaire non
    /// supporté : on saute l'index au lieu d'abandonner le contrôle.
    fn menu_items(&self, query: &v4l::v4l_sys::v4l2_query_ext_ctrl) -> Vec<(u32, String)> {
        let fd = self.dev.handle().fd();
        let step = query.step.max(1) as usize;
        let mut items = Vec::new();

        for index in (query.minimum..=query.maximum).step_by(step) {
            let mut menu: v4l::v4l_sys::v4l2_querymenu = unsafe { std::mem::zeroed() };
            menu.id = query.id;
            menu.index = index as u32;

            let ok = unsafe {
                v4l::v4l2::ioctl(
                    fd,
                    v4l::v4l2::vidioc::VIDIOC_QUERYMENU,
                    &mut menu as *mut _ as *mut std::os::raw::c_void,
                )
            };
            if ok.is_err() {
                continue;
            }

            // `v4l2_querymenu` est `repr(packed)` : on copie l'union avant de
            // la lire, une référence sur un champ non aligné serait invalide.
            let payload = menu.__bindgen_anon_1;
            let label = if query.type_ == ctrl_type::INTEGER_MENU {
                unsafe { payload.value }.to_string()
            } else {
                let raw = unsafe { payload.name };
                String::from_utf8_lossy(&raw)
                    .trim_end_matches('\0')
                    .to_string()
            };
            items.push((menu.index, label));
        }

        items
    }
}

impl CameraControls for V4l2Controls {
    fn controls(&self) -> Result<Vec<ControlDesc>> {
        let mut out = Vec::new();

        for (query, name) in self.enumerate_raw()? {
            let flags = query.flags;
            // Un contrôle désactivé ne répond à rien ; une classe n'est qu'un
            // séparateur logique dans la liste.
            if flags & v4l::v4l_sys::V4L2_CTRL_FLAG_DISABLED != 0
                || query.type_ == ctrl_type::CTRL_CLASS
            {
                continue;
            }

            let kind = match query.type_ {
                ctrl_type::INTEGER | ctrl_type::INTEGER64 | ctrl_type::BITMASK => {
                    ControlKind::Integer {
                        min: query.minimum,
                        max: query.maximum,
                        step: query.step.max(1) as i64,
                    }
                }
                ctrl_type::BOOLEAN => ControlKind::Boolean,
                ctrl_type::MENU | ctrl_type::INTEGER_MENU => {
                    ControlKind::Menu { items: self.menu_items(&query) }
                }
                ctrl_type::BUTTON => ControlKind::Button,
                _ => continue, // types composés et chaînes
            };

            let current = match self.dev.control(query.id).map(|c| c.value) {
                Ok(control::Value::Integer(v)) => v,
                Ok(control::Value::Boolean(b)) => b as i64,
                _ => query.default_value,
            };

            out.push(ControlDesc {
                id: query.id,
                name,
                kind,
                default: query.default_value,
                current,
                inactive: flags & v4l::v4l_sys::V4L2_CTRL_FLAG_INACTIVE != 0,
                read_only: flags & v4l::v4l_sys::V4L2_CTRL_FLAG_READ_ONLY != 0,
            });
        }

        Ok(out)
    }

    fn set_control(&self, id: u32, value: i64) -> Result<()> {
        // `Value::Integer` couvre aussi les booléens et les menus : au niveau
        // de `VIDIOC_S_CTRL` tout finit dans un `i32`.
        self.dev
            .set_control(control::Control { id, value: control::Value::Integer(value) })
            .with_context(|| format!("écriture du contrôle {id} = {value}"))?;
        Ok(())
    }
}

impl Camera for V4l2Camera {
    fn caps(&self) -> Result<Vec<FormatCaps>> {
        let mut out = Vec::new();

        for fmt in self.dev.enum_formats().context("énumération des formats")? {
            let pixfmt = PixelFormat::from_fourcc(fmt.fourcc.repr);

            for size in self.dev.enum_framesizes(fmt.fourcc)? {
                // On ignore les tailles « stepwise » : une carte d'acquisition
                // annonce des modes discrets, et proposer un continuum à
                // l'utilisateur ne l'aiderait pas.
                let v4l::framesize::FrameSizeEnum::Discrete(d) = size.size else {
                    continue;
                };

                let mut fps: Vec<u32> = self
                    .dev
                    .enum_frameintervals(fmt.fourcc, d.width, d.height)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|iv| match iv.interval {
                        v4l::frameinterval::FrameIntervalEnum::Discrete(f) if f.numerator > 0 => {
                            Some(f.denominator / f.numerator)
                        }
                        _ => None,
                    })
                    .collect();
                fps.sort_unstable_by(|a, b| b.cmp(a));
                fps.dedup();

                out.push(FormatCaps { pixfmt, width: d.width, height: d.height, fps });
            }
        }

        Ok(out)
    }

    fn control_handle(&self) -> Arc<dyn CameraControls> {
        Arc::new(V4l2Controls { dev: Arc::clone(&self.dev) })
    }

    fn negotiate(&mut self, req: &FormatRequest) -> Result<FrameFormat> {
        let caps = self.caps()?;
        let chosen = req
            .pick(&caps)
            .ok_or_else(|| anyhow!("l'appareil n'annonce aucun format exploitable"))?;

        let fourcc = FourCC::new(&chosen.pixfmt.fourcc());
        let want = Format::new(chosen.width, chosen.height, fourcc);
        let got = self.dev.set_format(&want).context("VIDIOC_S_FMT")?;

        // La cadence se règle séparément, et le pilote peut refuser en silence :
        // on relit ce qu'il a retenu plutôt que de supposer.
        let target_fps = chosen
            .fps
            .iter()
            .copied()
            .min_by_key(|f| (*f as i64 - req.fps as i64).abs())
            .unwrap_or(req.fps);
        if target_fps > 0 {
            let _ = self.dev.set_params(&Parameters::with_fps(target_fps));
        }

        Ok(FrameFormat {
            width: got.width,
            height: got.height,
            pixfmt: PixelFormat::from_fourcc(got.fourcc.repr),
            stride: got.stride,
            color: color_spec(&got),
        })
    }

    fn start(
        mut self: Box<Self>,
        req: FormatRequest,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Capture> {
        let capture_format = self.negotiate(&req)?;

        // Ce que voit le consommateur n'est pas toujours ce que sort l'appareil :
        // un flux compressé est décodé sur le thread de capture, qui a tout le
        // temps d'une trame pour le faire, plutôt que sur le thread de rendu où
        // ces millisecondes se paieraient en retard à l'affichage.
        let published_format = if capture_format.pixfmt.published() != capture_format.pixfmt {
            FrameFormat {
                pixfmt: capture_format.pixfmt.published(),
                stride: capture_format.width * 4,
                color: ColorSpec { full_range: true, ..capture_format.color },
                ..capture_format
            }
        } else {
            capture_format
        };

        let controls = self.control_handle();
        let (sink, mut capture) = super::channel(published_format, controls, notify);

        let dev = Arc::clone(&self.dev);
        let buffers = req.buffers.max(2);
        let label = self.info.path.display().to_string();

        let thread = std::thread::Builder::new()
            .name("vidio-capture".into())
            .spawn(move || {
                if let Err(e) = pump(&dev, buffers, capture_format, published_format, &sink) {
                    log::error!("capture {label} interrompue : {e:#}");
                    sink.note_error();
                }
            })
            .context("démarrage du thread de capture")?;

        capture.thread = Some(thread);
        Ok(capture)
    }
}

/// Boucle de capture. Tourne jusqu'à ce que le [`Capture`] soit lâché.
fn pump(
    dev: &Device,
    buffers: u32,
    capture_format: FrameFormat,
    published_format: FrameFormat,
    sink: &FrameSink,
) -> Result<()> {
    let mut decoder = capture_format.pixfmt.is_compressed().then(MjpegDecoder::new);
    // Le pilote numérote les trames : un trou signifie qu'il en a produit une
    // qu'on n'a jamais reçue, faute de tampon libre ou de bande passante USB.
    let mut last_sequence: Option<u32> = None;
    let mut stream = MmapStream::with_buffers(dev, BufType::VideoCapture, buffers)
        .context("allocation des tampons mmap")?;
    // Sans plafond, un débranchement à chaud bloquerait le thread pour toujours.
    stream.set_timeout(Duration::from_millis(500));
    // Surtout ne pas appeler `start()` ici : le premier `next()` enfile tous les
    // tampons puis démarre lui-même. Déclencher STREAMON avant lui le fait
    // basculer dans sa branche « déjà actif », où il ne réenfile que le dernier
    // tampon rendu — on tournerait alors avec un seul tampon en vol, à perdre
    // toutes les trames produites pendant qu'on traite la précédente.

    while sink.should_run() {
        let (buf, meta) = match stream.next() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                sink.note_error();
                continue;
            }
            Err(e) => return Err(e).context("dépilement d'un tampon"),
        };

        if meta.flags.contains(v4l::buffer::Flags::ERROR) {
            sink.note_error();
            continue;
        }

        let len = (meta.bytesused as usize).min(buf.len());
        if len == 0 {
            sink.note_error();
            continue;
        }

        let captured_at = Instant::now();

        if let Some(previous) = last_sequence {
            let gap = meta.sequence.saturating_sub(previous).saturating_sub(1);
            if gap > 0 {
                sink.note_missed(gap as u64);
            }
        }
        last_sequence = Some(meta.sequence);

        let data = if let Some(decoder) = decoder.as_mut() {
            let mut rgba = sink.take_buffer(published_format.stride as usize * published_format.height as usize);
            match decoder.decode_into(&buf[..len], &mut rgba) {
                Ok(_) => rgba,
                Err(e) => {
                    // Une trame JPEG corrompue arrive de temps en temps sur USB.
                    // On la saute : la suivante est déjà en route.
                    log::debug!("trame JPEG rejetée : {e:#}");
                    sink.note_error();
                    continue;
                }
            }
        } else {
            // Une copie depuis le tampon mmap. On pourrait l'éviter en gardant
            // le tampon dépilé jusqu'à la fin de l'upload GPU, mais avec 3
            // tampons on affamerait le pilote ; à 720p60 en YUYV cette copie
            // coûte ~110 Mo/s, un bruit de fond devant le reste du pipeline.
            let mut data = sink.take_buffer(len);
            data.extend_from_slice(&buf[..len]);
            data
        };

        sink.publish(Frame { data, format: published_format, captured_at });
    }

    stream.stop().context("VIDIOC_STREAMOFF")?;
    Ok(())
}
