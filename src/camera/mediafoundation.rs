//! Backend Windows : Media Foundation.
//!
//! Même parti pris que le backend Linux : on parle au pilote sans laisser
//! s'intercaler une couche de conversion. C'est le rôle de
//! `MF_READWRITE_DISABLE_CONVERTERS` sur le lecteur de source — sans lui, Media
//! Foundation « rend service » en insérant un décodeur et un convertisseur de
//! couleurs, et livre du RGB32 converti sur le CPU. Le flux arrive donc ici tel
//! que la carte l'émet : le YUY2 part sur le GPU sans être touché, le MJPEG est
//! décodé par nous, sur le thread de capture.
//!
//! Les contrôles matériels passent par les deux interfaces DirectShow que toute
//! source de capture expose encore (`IAMVideoProcAmp`, `IAMCameraControl`) :
//! c'est l'équivalent exact des contrôles V4L2, appliqué par le capteur, donc
//! sans coût en CPU ni en latence.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::DirectShow::{
    CameraControlProperty, CameraControl_Exposure, CameraControl_Flags_Auto,
    CameraControl_Flags_Manual, CameraControl_Focus, CameraControl_Iris, CameraControl_Pan,
    CameraControl_Roll, CameraControl_Tilt, CameraControl_Zoom, IAMCameraControl, IAMVideoProcAmp,
    VideoProcAmpProperty, VideoProcAmp_BacklightCompensation, VideoProcAmp_Brightness,
    VideoProcAmp_ColorEnable, VideoProcAmp_Contrast, VideoProcAmp_Flags_Auto,
    VideoProcAmp_Flags_Manual, VideoProcAmp_Gain, VideoProcAmp_Gamma, VideoProcAmp_Hue,
    VideoProcAmp_Saturation, VideoProcAmp_Sharpness, VideoProcAmp_WhiteBalance,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFActivate, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MFCreateAttributes, MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFSampleExtension_Discontinuity,
    MFStartup, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_NO_MORE_TYPES,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_READWRITE_DISABLE_CONVERTERS,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_STREAMTICK, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{GUID, Interface, PCWSTR, PWSTR};

use super::{
    Camera, CameraControls, Capture, ColorSpec, ControlDesc, ControlKind, FormatCaps,
    FormatRequest, Frame, FrameFormat, FrameSink, PixelFormat, YuvMatrix,
};
use crate::device::{DeviceKey, VideoDevice};
use crate::video::decode::MjpegDecoder;

/// Le seul flux qui nous intéresse. Une carte d'acquisition expose aussi son
/// entrée audio, mais elle passe par cpal comme n'importe quelle autre.
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Enveloppe `Send` / `Sync` pour un pointeur d'interface COM.
///
/// Les interfaces générées par la crate `windows` enveloppent un pointeur brut :
/// elles sont donc `!Send`. Les objets de Media Foundation, eux, sont libres de
/// tout appartement — l'architecture appelle elle-même ses clients depuis ses
/// propres threads de travail — et les faire traverser un thread est licite.
/// C'est le pendant exact du descripteur V4L2 partagé par le backend Linux.
#[repr(transparent)]
struct Handle<T>(T);

unsafe impl<T> Send for Handle<T> {}
unsafe impl<T> Sync for Handle<T> {}

impl<T> std::ops::Deref for Handle<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// La source ouverte, éteinte quand plus personne ne la tient.
///
/// `Shutdown` n'est pas une politesse : tant qu'il n'a pas été appelé, la carte
/// reste réservée et la rouvrir échoue. Le compte de références COM finirait par
/// y arriver, mais pas forcément avant que l'utilisateur ne réessaie.
struct Source(Handle<IMFMediaSource>);

impl Source {
    fn shutdown(&self) {
        unsafe {
            let _ = self.0.Shutdown();
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::ops::Deref for Source {
    type Target = IMFMediaSource;
    fn deref(&self) -> &IMFMediaSource {
        &self.0
    }
}

/// Inscrit le thread courant auprès de COM.
///
/// `RPC_E_CHANGED_MODE` est attendu sur le thread principal : winit y appelle
/// `OleInitialize` pour le glisser-déposer, ce qui le place en appartement
/// cloisonné. L'échec est sans conséquence ici — les objets de Media Foundation
/// sont libres d'appartement, et le thread reste bien inscrit.
fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Démarre Media Foundation, une fois pour le processus.
fn mf_init() -> Result<()> {
    com_init();
    static STARTED: OnceLock<std::result::Result<(), windows::core::HRESULT>> = OnceLock::new();
    match STARTED.get_or_init(|| unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| e.code())
    }) {
        Ok(()) => Ok(()),
        Err(code) => bail!("Media Foundation indisponible (MFStartup : {code})"),
    }
}

fn create_attributes(slots: u32) -> Result<IMFAttributes> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, slots) }.context("allocation d'attributs MF")?;
    attrs.ok_or_else(|| anyhow!("MFCreateAttributes n'a rien renvoyé"))
}

/// Lit une chaîne d'attributs, en libérant l'allocation faite par MF.
fn allocated_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut raw = PWSTR::null();
        let mut len = 0u32;
        attrs.GetAllocatedString(key, &mut raw, &mut len).ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const c_void));
        text
    }
}

/// Convertit une chaîne Rust en tampon terminé par un zéro, à garder vivant le
/// temps de l'appel qui l'utilise.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Un sous-type vidéo de Media Foundation est un GUID dont les quatre premiers
/// octets sont le FourCC, le reste étant une base commune. Les quelques
/// sous-types hérités de DirectShow (RGB32, RGB24...) mettent un petit entier à
/// la place ; on les écarte, faute de pouvoir les nommer — et de toute façon le
/// lecteur n'en produira aucun, les convertisseurs étant désactivés.
fn fourcc_of(subtype: &GUID) -> Option<[u8; 4]> {
    let cc = subtype.data1.to_le_bytes();
    cc.iter().all(|b| b.is_ascii_graphic() || *b == b' ').then_some(cc)
}

/// `\\?\usb#vid_534d&pid_2109&mi_00#...` -> `usb 534d:2109`
///
/// Le lien symbolique complet est illisible dans une liste ; il reste accessible
/// dans le champ `by-id`. Ce qu'on en garde ici, c'est ce qui permet de
/// reconnaître la carte : l'identifiant du constructeur et celui du produit.
fn bus_label(link: &str) -> String {
    let lower = link.to_ascii_lowercase();
    let field = |name: &str| {
        lower
            .split_once(name)
            .map(|(_, rest)| rest.chars().take(4).collect::<String>())
            .filter(|v| v.len() == 4)
    };
    let enumerator = lower
        .trim_start_matches(r"\\?\")
        .split_once('#')
        .map(|(head, _)| head.to_string())
        .unwrap_or_else(|| "?".into());
    match (field("vid_"), field("pid_")) {
        (Some(vid), Some(pid)) => format!("{enumerator} {vid}:{pid}"),
        _ => enumerator,
    }
}

/// Énumère les sources de capture vidéo du système.
pub fn enumerate() -> Result<Vec<VideoDevice>> {
    mf_init()?;

    let attrs = create_attributes(1)?;
    unsafe {
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .context("sélection des sources vidéo")?;
    }

    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut raw, &mut count) }
        .context("énumération des périphériques de capture")?;
    if raw.is_null() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    unsafe {
        // MF nous confie le tableau *et* une référence par élément. Sortir
        // chaque `Option` de la case la relâche en fin d'itération ; le tableau
        // lui-même se rend à l'allocateur de COM.
        let slots = std::slice::from_raw_parts_mut(raw, count as usize);
        for (index, slot) in slots.iter_mut().enumerate() {
            let Some(activate) = slot.take() else { continue };
            let Some(link) =
                allocated_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
            else {
                // Sans lien symbolique on ne saurait ni rouvrir l'appareil, ni
                // lui donner une identité stable.
                continue;
            };
            let card = allocated_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|| "Périphérique de capture".into());

            out.push(VideoDevice {
                path: PathBuf::from(&link),
                index,
                bus: bus_label(&link),
                card,
                driver: "Media Foundation".into(),
                key: DeviceKey::from_symlink(&link),
                by_id: Some(link),
            });
        }
        CoTaskMemFree(Some(raw as *const c_void));
    }

    Ok(out)
}

pub struct MfCamera {
    info: VideoDevice,
    source: Arc<Source>,
    reader: Handle<IMFSourceReader>,
}

impl MfCamera {
    pub fn open(info: VideoDevice) -> Result<Self> {
        mf_init()?;

        let link = wide(info.by_id.as_deref().unwrap_or_default());
        let attrs = create_attributes(2)?;
        let source = unsafe {
            attrs.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )?;
            attrs.SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                PCWSTR(link.as_ptr()),
            )?;
            MFCreateDeviceSource(&attrs)
        }
        .map_err(|e| open_error(e, &info))?;

        let reader_attrs = create_attributes(1)?;
        let reader = unsafe {
            // Voir l'en-tête du module : c'est la ligne qui garantit qu'aucune
            // conversion n'a lieu dans notre dos.
            reader_attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &reader_attrs)
                .map_err(|e| anyhow!("création du lecteur de source : {e}"))?;
            reader
                .SetStreamSelection(STREAM, true)
                .map_err(|e| anyhow!("sélection du flux vidéo : {e}"))?;
            reader
        };

        Ok(Self { info, source: Arc::new(Source(Handle(source))), reader: Handle(reader) })
    }
}

/// Traduit l'échec d'ouverture le plus courant sous Windows.
///
/// Un périphérique de capture parfaitement branché et reconnu renvoie
/// `E_ACCESSDENIED` tant que « Autoriser les applications de bureau à accéder à
/// votre caméra » est décoché — c'est un réglage système, pas un problème de
/// pilote, et rien dans le message d'origine ne le laisse deviner.
fn open_error(err: windows::core::Error, info: &VideoDevice) -> anyhow::Error {
    const E_ACCESSDENIED: i32 = -2147024891; // 0x80070005
    if err.code().0 == E_ACCESSDENIED {
        return anyhow!(
            "accès à « {} » refusé par Windows — vérifier Paramètres › Confidentialité \
             et sécurité › Caméra, et notamment « Autoriser les applications de bureau \
             à accéder à votre caméra »",
            info.card
        );
    }
    anyhow!("ouverture de « {} » : {err}", info.card)
}

/// Taille d'image annoncée par un type de média.
fn frame_size(mt: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    Some(((packed >> 32) as u32, packed as u32))
}

/// Cadence annoncée, arrondie à l'entier le plus proche.
///
/// Media Foundation la donne en fraction exacte : le 59,94 Hz du NTSC est
/// 60000/1001, et l'arrondir à 60 est ce que fait déjà le backend Linux.
fn frame_rate(mt: &IMFMediaType) -> Option<u32> {
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let (num, den) = ((packed >> 32) as u32, packed as u32);
    (den > 0).then(|| (num as f64 / den as f64).round() as u32)
}

/// Colorimétrie annoncée par le pilote, avec le même repli que sous Linux.
fn color_spec(mt: &IMFMediaType, height: u32) -> ColorSpec {
    // MFVideoTransferMatrix : 1 = BT.709, 2 = BT.601, 3 = SMPTE 240M.
    let matrix = match unsafe { mt.GetUINT32(&MF_MT_YUV_MATRIX) } {
        Ok(1) => Some(YuvMatrix::Bt709),
        Ok(2) | Ok(3) => Some(YuvMatrix::Bt601),
        _ => None,
    };
    // MFNominalRange : 1 = 16..235, 2 = 0..255.
    let full_range = match unsafe { mt.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
        Ok(2) => Some(true),
        Ok(1) => Some(false),
        _ => None,
    };

    let guess = ColorSpec::guess_from_height(height);
    ColorSpec {
        matrix: matrix.unwrap_or(guess.matrix),
        full_range: full_range.unwrap_or(guess.full_range),
    }
}

/// Décrit le format qu'un type de média représente.
fn describe(mt: &IMFMediaType) -> Result<FrameFormat> {
    let subtype = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }.context("sous-type absent")?;
    let cc = fourcc_of(&subtype).ok_or_else(|| anyhow!("sous-type vidéo non nommable"))?;
    let pixfmt = PixelFormat::from_fourcc(cc);
    let (width, height) = frame_size(mt).ok_or_else(|| anyhow!("taille d'image absente"))?;

    // La foulée est signée : elle est négative pour les formats RGB stockés de
    // bas en haut. On n'en affiche aucun, mais autant ne pas propager un nombre
    // absurde jusqu'à l'upload GPU.
    let stride = unsafe { mt.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|s| (s as i32).unsigned_abs())
        .unwrap_or(0);

    Ok(FrameFormat { width, height, pixfmt, stride, color: color_spec(mt, height) })
}

impl Camera for MfCamera {
    fn caps(&self) -> Result<Vec<FormatCaps>> {
        // Media Foundation énumère une entrée par combinaison format / taille /
        // cadence ; on les regroupe comme le fait V4L2, une entrée par mode avec
        // la liste de ses cadences.
        let mut grouped: BTreeMap<([u8; 4], u32, u32), Vec<u32>> = BTreeMap::new();

        for index in 0.. {
            let mt = match unsafe { self.reader.GetNativeMediaType(STREAM, index) } {
                Ok(mt) => mt,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(anyhow!("énumération des formats : {e}")),
            };

            let major = unsafe { mt.GetGUID(&MF_MT_MAJOR_TYPE) };
            if major.ok() != Some(MFMediaType_Video) {
                continue;
            }
            let Ok(subtype) = (unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }) else { continue };
            let Some(cc) = fourcc_of(&subtype) else { continue };
            let Some((width, height)) = frame_size(&mt) else { continue };

            let entry = grouped.entry((cc, width, height)).or_default();
            if let Some(fps) = frame_rate(&mt) {
                entry.push(fps);
            }
        }

        Ok(grouped
            .into_iter()
            .map(|((cc, width, height), mut fps)| {
                fps.sort_unstable_by(|a, b| b.cmp(a));
                fps.dedup();
                FormatCaps { pixfmt: PixelFormat::from_fourcc(cc), width, height, fps }
            })
            .collect())
    }

    fn control_handle(&self) -> Arc<dyn CameraControls> {
        Arc::new(MfControls { source: Arc::clone(&self.source) })
    }

    fn negotiate(&mut self, req: &FormatRequest) -> Result<FrameFormat> {
        let caps = self.caps()?;
        let chosen = req.pick(&caps).ok_or_else(|| super::no_format_error(req, &caps))?;

        // Un format retenu qui n'est pas celui demandé vient forcément d'un
        // profil : le dire, sinon l'image change sans explication.
        if let Some(want) = req.pixfmt
            && want != chosen.pixfmt
        {
            log::warn!(
                "{} indisponible sur cet appareil : {} retenu à la place",
                want.name(),
                chosen.pixfmt.name()
            );
        }
        let target_fps = chosen
            .fps
            .iter()
            .copied()
            .min_by_key(|f| (*f as i64 - req.fps as i64).abs())
            .unwrap_or(req.fps);

        // Contrairement à V4L2, on ne décrit pas le format voulu : on désigne
        // l'un des types que le pilote a lui-même énumérés. Il faut donc le
        // retrouver, et parmi ceux qui conviennent prendre la cadence la plus
        // proche de celle demandée.
        let mut best: Option<(IMFMediaType, i64)> = None;
        for index in 0.. {
            let mt = match unsafe { self.reader.GetNativeMediaType(STREAM, index) } {
                Ok(mt) => mt,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(anyhow!("énumération des formats : {e}")),
            };

            let Ok(subtype) = (unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }) else { continue };
            if fourcc_of(&subtype).map(PixelFormat::from_fourcc) != Some(chosen.pixfmt) {
                continue;
            }
            if frame_size(&mt) != Some((chosen.width, chosen.height)) {
                continue;
            }

            let error = (frame_rate(&mt).unwrap_or(0) as i64 - target_fps as i64).abs();
            if best.as_ref().is_none_or(|(_, best_error)| error < *best_error) {
                best = Some((mt, error));
            }
        }

        let (mt, _) = best.ok_or_else(|| {
            anyhow!(
                "le pilote n'expose plus {}x{} {}",
                chosen.width,
                chosen.height,
                chosen.pixfmt.name()
            )
        })?;

        unsafe { self.reader.SetCurrentMediaType(STREAM, None, &mt) }
            .map_err(|e| anyhow!("format refusé par le pilote : {e}"))?;

        // On relit ce que le lecteur a retenu : c'est lui qui fait foi, en
        // particulier pour la foulée et la colorimétrie.
        let got = unsafe { self.reader.GetCurrentMediaType(STREAM) }
            .map_err(|e| anyhow!("relecture du format négocié : {e}"))?;
        describe(&got)
    }

    fn start(
        self: Box<Self>,
        req: FormatRequest,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Capture> {
        // `buffers` ne sert pas ici : le nombre de tampons en vol appartient au
        // lecteur de source, qui n'en accepte pas la consigne. La politique
        // « la dernière trame gagne » est de toute façon appliquée en aval.
        let _ = req.buffers;

        let mut me = *self;
        let capture_format = me.negotiate(&req)?;

        // Même règle que sous Linux : un flux compressé est décodé sur le thread
        // de capture, qui a tout le temps d'une trame pour le faire, et publié
        // en RGBA pleine largeur.
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

        let controls = me.control_handle();
        let (sink, mut capture) = super::channel(published_format, controls, notify);

        let source = Arc::clone(&me.source);
        let unblock = Arc::clone(&me.source);
        let reader = me.reader;
        let label = me.info.card.clone();

        let thread = std::thread::Builder::new()
            .name("vidio-capture".into())
            .spawn(move || {
                com_init();
                // La source doit rester vivante aussi longtemps que le lecteur
                // s'en sert ; elle n'est éteinte qu'à la fin du flux.
                let _source = source;
                if let Err(e) = pump(&reader, capture_format, published_format, &sink) {
                    log::error!("capture {label} interrompue : {e:#}");
                    sink.note_error();
                }
            })
            .context("démarrage du thread de capture")?;

        capture.thread = Some(thread);
        // `ReadSample` bloque sans plafond : sans ça, fermer la fenêtre pendant
        // que la carte ne délivre plus rien — console éteinte, câble débranché —
        // attendrait indéfiniment. Éteindre la source fait échouer l'attente.
        capture.unblock = Some(Box::new(move || unblock.shutdown()));
        Ok(capture)
    }
}

/// Boucle de capture. Tourne jusqu'à ce que le [`Capture`] soit lâché.
fn pump(
    reader: &IMFSourceReader,
    capture_format: FrameFormat,
    published_format: FrameFormat,
    sink: &FrameSink,
) -> Result<()> {
    const ENDOFSTREAM: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;
    const ERROR: u32 = MF_SOURCE_READERF_ERROR.0 as u32;
    const STREAMTICK: u32 = MF_SOURCE_READERF_STREAMTICK.0 as u32;
    const TYPECHANGED: u32 = MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32;

    let mut decoder = capture_format.pixfmt.is_compressed().then(MjpegDecoder::new);

    while sink.should_run() {
        let mut flags = 0u32;
        let mut sample = None;
        unsafe {
            reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
        }
        .map_err(|e| anyhow!("lecture d'une trame : {e}"))?;

        if flags & ENDOFSTREAM != 0 {
            log::info!("fin de flux signalée par le pilote");
            break;
        }
        if flags & ERROR != 0 {
            sink.note_error();
            continue;
        }
        if flags & TYPECHANGED != 0 {
            // Le pilote a changé de format sous nos pieds : la trame suivante
            // n'aurait plus la disposition que le rendu attend. Mieux vaut
            // s'arrêter en le disant que peindre du bruit.
            let live = unsafe { reader.GetCurrentMediaType(STREAM) }
                .map_err(|e| anyhow!("relecture du format : {e}"))
                .and_then(|mt| describe(&mt))?;
            if live != capture_format {
                bail!(
                    "le pilote est passé en {}x{} {} — rouvrir le périphérique",
                    live.width,
                    live.height,
                    live.pixfmt.name()
                );
            }
        }

        // Le lecteur rend la main sans trame quand il ne fait que signaler un
        // trou dans le flux. Sur USB, c'est le symptôme d'une bande passante
        // insuffisante — exactement ce que compte `missed` sous Linux.
        let Some(sample) = sample else {
            if flags & STREAMTICK != 0 {
                sink.note_missed(1);
            }
            continue;
        };

        if unsafe { sample.GetUINT32(&MFSampleExtension_Discontinuity) } == Ok(1) {
            sink.note_missed(1);
        }

        let captured_at = Instant::now();

        // `ConvertToContiguousBuffer` ne copie que si la trame était éclatée en
        // plusieurs tampons, ce qui n'arrive pas pour de la capture.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| anyhow!("tampon de trame illisible : {e}"))?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }
            .map_err(|e| anyhow!("verrouillage du tampon : {e}"))?;

        let taken = if len == 0 || ptr.is_null() {
            None
        } else {
            let raw = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            match decoder.as_mut() {
                Some(decoder) => {
                    let mut rgba = sink.take_buffer(
                        published_format.stride as usize * published_format.height as usize,
                    );
                    match decoder.decode_into(raw, &mut rgba) {
                        Ok(_) => Some(rgba),
                        Err(e) => {
                            // Une trame JPEG corrompue arrive de temps en temps
                            // sur USB. On la saute : la suivante est en route.
                            log::debug!("trame JPEG rejetée : {e:#}");
                            None
                        }
                    }
                }
                None => {
                    // Une copie hors du tampon de MF, qu'il faut lui rendre
                    // aussitôt. À 720p60 en YUY2 elle coûte ~110 Mo/s, un bruit
                    // de fond devant le reste du pipeline.
                    let mut data = sink.take_buffer(raw.len());
                    data.extend_from_slice(raw);
                    Some(data)
                }
            }
        };

        unsafe {
            let _ = buffer.Unlock();
        }

        match taken {
            Some(data) => {
                sink.publish(Frame { data, format: published_format, captured_at })
            }
            None => sink.note_error(),
        }
    }

    Ok(())
}

// --- contrôles matériels ---------------------------------------------------

/// Les deux interfaces ne se distinguent que par l'ensemble de propriétés
/// qu'elles portent ; on encode laquelle dans l'identifiant du contrôle.
const ID_PROC_AMP: u32 = 0x0100_0000;
const ID_CAMERA: u32 = 0x0200_0000;
/// Le contrôle ne porte pas la valeur mais le drapeau « automatique » associé.
const ID_AUTO: u32 = 0x0001_0000;

/// Réglages d'image, nommés comme leurs équivalents V4L2 : un profil reste ainsi
/// lisible quel que soit le système qui l'a écrit.
const PROC_AMP: &[(VideoProcAmpProperty, &str)] = &[
    (VideoProcAmp_Brightness, "Brightness"),
    (VideoProcAmp_Contrast, "Contrast"),
    (VideoProcAmp_Hue, "Hue"),
    (VideoProcAmp_Saturation, "Saturation"),
    (VideoProcAmp_Sharpness, "Sharpness"),
    (VideoProcAmp_Gamma, "Gamma"),
    (VideoProcAmp_ColorEnable, "Color Enable"),
    (VideoProcAmp_WhiteBalance, "White Balance Temperature"),
    (VideoProcAmp_BacklightCompensation, "Backlight Compensation"),
    (VideoProcAmp_Gain, "Gain"),
];

/// Réglages d'optique. Rares sur une carte d'acquisition, courants sur une
/// webcam — et c'est là que se trouve l'exposition.
const CAMERA: &[(CameraControlProperty, &str)] = &[
    (CameraControl_Pan, "Pan, Absolute"),
    (CameraControl_Tilt, "Tilt, Absolute"),
    (CameraControl_Roll, "Roll, Absolute"),
    (CameraControl_Zoom, "Zoom, Absolute"),
    (CameraControl_Exposure, "Exposure Time, Absolute"),
    (CameraControl_Iris, "Iris, Absolute"),
    (CameraControl_Focus, "Focus, Absolute"),
];

struct MfControls {
    source: Arc<Source>,
}

/// Interroge une propriété et en fait un ou deux contrôles.
///
/// Les deux interfaces DirectShow ont des méthodes de signature identique mais
/// aucun trait commun : une macro évite d'écrire la même chose deux fois.
macro_rules! collect_controls {
    ($out:expr, $iface:expr, $base:expr, $table:expr, $auto:expr, $manual:expr) => {
        for (property, name) in $table {
            let property = property.0;
            let (mut min, mut max, mut step, mut default, mut caps) = (0, 0, 0, 0, 0);
            // Une propriété non supportée renvoie une erreur ; c'est la seule
            // façon de savoir ce que l'appareil sait vraiment faire.
            if unsafe {
                $iface.GetRange(
                    property,
                    &mut min,
                    &mut max,
                    &mut step,
                    &mut default,
                    &mut caps,
                )
            }
            .is_err()
            {
                continue;
            }

            let (mut value, mut flags) = (default, 0);
            if unsafe { $iface.Get(property, &mut value, &mut flags) }.is_err() {
                value = default;
            }
            let automatic = flags & $auto.0 != 0;

            let kind = if min == 0 && max == 1 && step == 1 {
                ControlKind::Boolean
            } else {
                ControlKind::Integer {
                    min: min as i64,
                    max: max as i64,
                    step: (step.max(1)) as i64,
                }
            };

            $out.push(ControlDesc {
                id: $base | property as u32,
                name: (*name).to_string(),
                kind,
                default: default as i64,
                current: value as i64,
                // Une valeur réglée à la main n'a aucun effet tant que le pilote
                // pilote la propriété tout seul : c'est exactement ce que le
                // drapeau `inactive` de V4L2 signale.
                inactive: automatic,
                read_only: caps & $manual.0 == 0,
            });

            // Le mode automatique devient une case à cocher à part, comme
            // « Auto Exposure » côté V4L2. Windows ne dit pas lequel des deux
            // modes est celui d'usine ; on retient l'automatique, qui est le
            // comportement de sortie de carton de la quasi-totalité des
            // appareils UVC.
            if caps & $auto.0 != 0 {
                $out.push(ControlDesc {
                    id: $base | ID_AUTO | property as u32,
                    name: format!("{name} (auto)"),
                    kind: ControlKind::Boolean,
                    default: 1,
                    current: automatic as i64,
                    inactive: false,
                    read_only: false,
                });
            }
        }
    };
}

impl CameraControls for MfControls {
    fn controls(&self) -> Result<Vec<ControlDesc>> {
        let mut out = Vec::new();

        if let Ok(amp) = self.source.cast::<IAMVideoProcAmp>() {
            collect_controls!(
                out,
                amp,
                ID_PROC_AMP,
                PROC_AMP,
                VideoProcAmp_Flags_Auto,
                VideoProcAmp_Flags_Manual
            );
        }
        if let Ok(cam) = self.source.cast::<IAMCameraControl>() {
            collect_controls!(
                out,
                cam,
                ID_CAMERA,
                CAMERA,
                CameraControl_Flags_Auto,
                CameraControl_Flags_Manual
            );
        }

        Ok(out)
    }

    fn set_control(&self, id: u32, value: i64) -> Result<()> {
        let property = (id & 0xFFFF) as i32;
        let toggles_auto = id & ID_AUTO != 0;

        // Basculer le mode automatique se fait par le même `Set` que la valeur :
        // on relit donc celle en cours pour ne pas la remplacer au passage.
        macro_rules! apply {
            ($iface:expr, $auto:expr, $manual:expr) => {{
                if toggles_auto {
                    let (mut current, mut flags) = (0, 0);
                    unsafe { $iface.Get(property, &mut current, &mut flags) }
                        .map_err(|e| anyhow!("lecture du contrôle {property} : {e}"))?;
                    let mode = if value != 0 { $auto } else { $manual };
                    unsafe { $iface.Set(property, current, mode.0) }
                } else {
                    unsafe { $iface.Set(property, value as i32, $manual.0) }
                }
                .map_err(|e| anyhow!("écriture du contrôle {property} = {value} : {e}"))
            }};
        }

        if id & ID_PROC_AMP != 0 {
            let amp = self
                .source
                .cast::<IAMVideoProcAmp>()
                .map_err(|e| anyhow!("réglages d'image indisponibles : {e}"))?;
            return apply!(amp, VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual);
        }
        if id & ID_CAMERA != 0 {
            let cam = self
                .source
                .cast::<IAMCameraControl>()
                .map_err(|e| anyhow!("réglages d'optique indisponibles : {e}"))?;
            return apply!(cam, CameraControl_Flags_Auto, CameraControl_Flags_Manual);
        }

        bail!("contrôle {id} inconnu")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str =
        r"\\?\usb#vid_534d&pid_2109&mi_00#7&1f4a2bd5&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";

    #[test]
    fn bus_label_keeps_what_identifies_the_card() {
        assert_eq!(bus_label(LINK), "usb 534d:2109");
        // Une source qui n'est pas branchée en USB (carte interne, passerelle
        // logicielle) n'a ni VID ni PID : on garde au moins l'énumérateur.
        assert_eq!(bus_label(r"\\?\root#media#0000#{e5323777}\global"), "root");
    }

    #[test]
    fn only_fourcc_subtypes_are_kept() {
        // MFVideoFormat_YUY2 : le FourCC occupe les quatre premiers octets.
        let yuy2 = GUID::from_u128(0x32595559_0000_0010_8000_00aa00389b71);
        assert_eq!(fourcc_of(&yuy2), Some(*b"YUY2"));
        assert_eq!(PixelFormat::from_fourcc(fourcc_of(&yuy2).unwrap()), PixelFormat::Yuyv);

        // MFVideoFormat_RGB32 : un petit entier hérité de DirectShow, qui ne
        // donnerait qu'un nom illisible.
        let rgb32 = GUID::from_u128(0x00000016_0000_0010_8000_00aa00389b71);
        assert_eq!(fourcc_of(&rgb32), None);
    }
}
