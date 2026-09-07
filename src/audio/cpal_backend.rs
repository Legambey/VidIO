//! Transit audio via cpal.
//!
//! cpal reste le bon choix ici : il est le seul à couvrir ALSA/PipeWire, WASAPI
//! et CoreAudio derrière la même API, et sa version 0.18 expose enfin un
//! `DeviceId` persistant — on peut donc mémoriser « cette entrée-là » sans se
//! rabattre sur un nom qui change au gré des réinstallations.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use super::alsa_catalog;
use super::{AudioControls, AudioDeviceInfo, AudioStats, DriftController, Interpolator, map_channels};

/// Applique une fonction générique sur le type d'échantillon d'un flux.
macro_rules! dispatch_format {
    ($fmt:expr, $f:ident, $($arg:tt)*) => {
        match $fmt {
            SampleFormat::F32 => $f::<f32>($($arg)*),
            SampleFormat::I16 => $f::<i16>($($arg)*),
            SampleFormat::U16 => $f::<u16>($($arg)*),
            SampleFormat::I32 => $f::<i32>($($arg)*),
            // Les codecs internes ouverts en direct (`hw:`) sortent souvent en
            // 24 bits : sans ces deux lignes, choisir la sortie analogique de
            // la carte mère revient à ne pas avoir de son du tout.
            SampleFormat::I24 => $f::<cpal::I24>($($arg)*),
            SampleFormat::U24 => $f::<cpal::U24>($($arg)*),
            SampleFormat::I8 => $f::<i8>($($arg)*),
            SampleFormat::U8 => $f::<u8>($($arg)*),
            other => anyhow::bail!("format d'échantillon non géré : {other:?}"),
        }
    };
}

/// Une entrée de la liste, avant regroupement.
struct Candidate {
    id: String,
    label: String,
    /// Rang de la route ALSA : plus il est bas, plus on préfère cette route
    /// pour joindre le même matériel.
    rank: u8,
    /// Ordre d'affichage : serveurs d'abord, puis matériel par carte/device.
    order: (u8, u32, u32),
}

/// Analyse `alsa:plughw:CARD=0,DEV=3` en (plugin, carte, device).
///
/// Renvoie `None` pour tout ce qui n'est pas une route ALSA vers une carte :
/// les serveurs (`alsa:pipewire`), les hôtes des autres plateformes, et donc
/// tout ce qu'il ne faut pas regrouper.
fn parse_alsa_route(id: &str) -> Option<(&str, &str, Option<u32>)> {
    let (plugin, args) = id.strip_prefix("alsa:")?.split_once(':')?;
    let mut card = None;
    let mut dev = None;
    for arg in args.split(',') {
        match arg.split_once('=') {
            Some(("CARD", value)) => card = Some(value),
            Some(("DEV", value)) => dev = value.parse().ok(),
            _ => {}
        }
    }
    Some((plugin, card?, dev))
}

/// Préférence entre routes menant au même PCM.
///
/// `hw:` passe sans conversion ni mixage : c'est la plus courte, donc celle
/// qu'on garde. `plughw:` ajoute la conversion logicielle d'ALSA, `default:`
/// et `sysdefault:` y ajoutent le mixage — autant d'étapes que le transit fait
/// déjà lui-même, et qui coûteraient de la latence en double.
fn route_rank(plugin: &str) -> u8 {
    match plugin {
        "hw" => 0,
        "plughw" => 1,
        "sysdefault" => 2,
        "default" => 3,
        _ => 4,
    }
}

/// Clé de regroupement : les routes qui la partagent visent le même PCM.
///
/// `default:CARD=x` n'indique pas de device ; il mène au premier, donc à la
/// même chose que `hw:CARD=x,DEV=0`.
fn group_key(id: &str, catalog: &alsa_catalog::Catalog) -> String {
    match parse_alsa_route(id) {
        Some((_, card, dev)) => {
            let card = catalog
                .card_index(card)
                .map(|index| index.to_string())
                .unwrap_or_else(|| card.to_string());
            format!("carte/{card}/{}", dev.unwrap_or(0))
        }
        None => id.to_string(),
    }
}

fn candidate(id: String, desc: String, catalog: &alsa_catalog::Catalog) -> Candidate {
    let Some((plugin, card, dev)) = parse_alsa_route(&id) else {
        return Candidate { id, label: desc, rank: 0, order: (0, 0, 0) };
    };
    let rank = route_rank(plugin);
    let dev = dev.unwrap_or(0);
    let index = catalog.card_index(card);
    // cpal ne reprend que la première ligne du descriptif ALSA, souvent
    // « carte, » avec un nom de PCM vide ; le catalogue le complète.
    let label = index
        .and_then(|index| catalog.label(index, dev))
        .unwrap_or_else(|| desc.trim().trim_end_matches(',').trim().to_string());
    Candidate { id, label, rank, order: (1, index.unwrap_or(u32::MAX), dev) }
}

/// Réduit la liste de cpal à un choix par matériel réel.
fn dedupe(raw: Vec<(String, String)>, default_id: Option<&str>) -> Vec<AudioDeviceInfo> {
    let catalog = alsa_catalog::load();
    let default_key = default_id.map(|id| group_key(id, &catalog));

    let mut kept: Vec<Candidate> = Vec::new();
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    for (id, desc) in raw {
        // Le PCM « null » jette ce qu'on lui donne : il n'a rien à faire dans
        // une liste de matériel.
        if id == "alsa:null" {
            continue;
        }
        let key = group_key(&id, &catalog);
        let candidate = candidate(id, desc, &catalog);
        match index_by_key.get(&key) {
            Some(&at) => {
                if candidate.rank < kept[at].rank {
                    kept[at] = candidate;
                }
            }
            None => {
                index_by_key.insert(key, kept.len());
                kept.push(candidate);
            }
        }
    }

    kept.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.label.cmp(&b.label)));
    kept.into_iter()
        .map(|c| {
            // Le périphérique par défaut est souvent désigné par une route
            // qu'on vient d'écarter : c'est son matériel qui porte la mention.
            let is_default = default_key
                .as_deref()
                .is_some_and(|key| key == group_key(&c.id, &catalog));
            AudioDeviceInfo { name: c.label, id: c.id, is_default }
        })
        .collect()
}

/// Libellé d'un périphérique ouvert, avec la règle qui sert à la liste : ce
/// que l'utilisateur lit dans l'overlay doit être ce qu'il a choisi.
fn device_label(dev: &Device) -> String {
    let desc = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    match dev.id() {
        Ok(id) => candidate(id.to_string(), desc, &alsa_catalog::load()).label,
        Err(_) => desc,
    }
}

fn enumerate(input: bool) -> Result<Vec<AudioDeviceInfo>> {
    let host = cpal::default_host();
    let default_id = if input { host.default_input_device() } else { host.default_output_device() }
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());

    let raw = host_devices(input)?
        .iter()
        .filter_map(|d| Some((d.id().ok()?.to_string(), d.description().ok()?.name().to_string())))
        .collect();
    Ok(dedupe(raw, default_id.as_deref()))
}

pub fn enumerate_inputs() -> Result<Vec<AudioDeviceInfo>> {
    enumerate(true)
}

pub fn enumerate_outputs() -> Result<Vec<AudioDeviceInfo>> {
    enumerate(false)
}

/// Un périphérique s'ouvre-t-il vraiment ?
///
/// La seule façon de le savoir est de lui demander sa configuration : ALSA ne
/// signale un PCM occupé qu'à l'ouverture.
fn usable(dev: &Device, input: bool) -> bool {
    if input { dev.default_input_config().is_ok() } else { dev.default_output_config().is_ok() }
}

/// Le périphérique désigné par le système, ou le premier qui s'ouvre.
///
/// Le `default` d'ALSA ne mène pas forcément quelque part. Sans le paquet qui
/// le redirige vers le serveur de son (`pipewire-alsa` et son
/// `99-pipewire-default.conf`), il pointe sur la carte que le serveur tient
/// déjà ouverte, et s'y ouvrir renvoie « périphérique temporairement occupé ».
/// Prendre alors le premier périphérique utilisable de la liste — le serveur
/// de son d'abord, le matériel ensuite — vaut mieux que de déclarer l'audio
/// indisponible sur une machine dont le son marche par ailleurs.
fn open_default(input: bool) -> Result<Device> {
    let host = cpal::default_host();
    let system = if input { host.default_input_device() } else { host.default_output_device() };
    if let Some(dev) = system {
        if usable(&dev, input) {
            return Ok(dev);
        }
        log::warn!(
            "le périphérique audio système par défaut ne s'ouvre pas — on prend le premier disponible"
        );
    }

    let devices = host_devices(input)?;
    for known in enumerate(input)? {
        if let Some(dev) = devices.iter().find(|d| d.id().is_ok_and(|i| i.to_string() == known.id))
            && usable(dev, input)
        {
            return Ok(dev.clone());
        }
    }
    Err(anyhow!("aucun périphérique audio utilisable"))
}

fn host_devices(input: bool) -> Result<Vec<Device>> {
    let host = cpal::default_host();
    Ok(if input {
        host.input_devices().context("énumération des entrées audio")?.collect()
    } else {
        host.output_devices().context("énumération des sorties audio")?.collect()
    })
}

/// Retrouve un périphérique par identifiant persistant, à défaut par nom.
fn find_device(spec: Option<&str>, input: bool) -> Result<Device> {
    let Some(spec) = spec else {
        return open_default(input);
    };

    let devices = host_devices(input)?;

    devices
        .iter()
        .find(|d| d.id().is_ok_and(|i| i.to_string() == spec))
        .or_else(|| {
            devices
                .iter()
                .find(|d| d.description().map(|x| x.name().contains(spec)).unwrap_or(false))
        })
        .cloned()
        .ok_or_else(|| anyhow!("périphérique audio introuvable : « {spec} »"))
}

/// Transit actif entre une entrée et une sortie.
///
/// Non `Send` : les `Stream` de cpal doivent rester sur le thread qui les a
/// créés sur certaines plateformes. À garder dans la structure d'application.
pub struct Passthrough {
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub stats: Arc<AudioStats>,
    pub input_name: String,
    pub output_name: String,
    pub input_rate: u32,
    pub output_rate: u32,
    pub input_channels: u16,
    pub output_channels: u16,
}

impl Passthrough {
    /// Ouvre les deux flux et démarre le transit.
    ///
    /// `target_latency_ms` fixe le remplissage visé du tampon : c'est le
    /// compromis à régler. Trop bas, le moindre à-coup de l'ordonnanceur
    /// provoque une coupure ; ~20 ms est confortable sans être perceptible.
    pub fn start(
        input: Option<&str>,
        output: Option<&str>,
        target_latency_ms: f32,
        controls: Arc<AudioControls>,
    ) -> Result<Self> {
        let in_dev = find_device(input, true)?;
        let out_dev = find_device(output, false)?;

        let in_supported = in_dev.default_input_config().context("config d'entrée par défaut")?;
        let out_supported = out_dev.default_output_config().context("config de sortie par défaut")?;

        let in_config: StreamConfig = in_supported.config();
        let out_config: StreamConfig = out_supported.config();
        let in_channels = in_config.channels as usize;
        let out_channels = out_config.channels as usize;
        let in_rate = in_config.sample_rate;
        let out_rate = out_config.sample_rate;

        let target_frames = (target_latency_ms as f64 / 1000.0 * in_rate as f64).max(64.0);
        // Le tampon doit encaisser une salve de callbacks sans déborder :
        // huit fois la cible laisse de la marge sans ajouter de latence, le
        // remplissage moyen restant asservi sur la cible.
        let capacity = (target_frames * 8.0) as usize * in_channels;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);

        let stats = Arc::new(AudioStats::default());

        let input_stream = dispatch_format!(
            in_supported.sample_format(),
            build_input,
            &in_dev,
            in_config.clone(),
            producer,
            Arc::clone(&stats)
        )?;

        let drift = DriftController::new(in_rate, out_rate, target_frames);
        let output_stream = dispatch_format!(
            out_supported.sample_format(),
            build_output,
            &out_dev,
            out_config.clone(),
            consumer,
            in_channels,
            out_channels,
            drift,
            target_frames,
            Arc::clone(&controls),
            Arc::clone(&stats)
        )?;

        input_stream.play().context("démarrage de l'entrée audio")?;
        output_stream.play().context("démarrage de la sortie audio")?;

        Ok(Self {
            input_name: device_label(&in_dev),
            output_name: device_label(&out_dev),
            input_rate: in_rate,
            output_rate: out_rate,
            input_channels: in_config.channels,
            output_channels: out_config.channels,
            _input: input_stream,
            _output: output_stream,
            stats,
        })
    }

    pub fn latency_ms(&self) -> f32 {
        self.stats.buffer_latency_ms(self.input_rate)
    }
}

/// Rapporte les décrochages d'un flux sans noyer le journal.
///
/// ALSA les émet par rafales — un XRUN en entraîne souvent trois — et une
/// ligne par incident rend le reste du journal illisible. On compte tout, on
/// n'écrit qu'au plus une fois toutes les cinq secondes.
fn report_errors(
    label: &'static str,
    stats: Arc<AudioStats>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let mut last_log: Option<Instant> = None;
    let mut pending = 0u64;
    move |err| {
        stats.xruns.fetch_add(1, Ordering::Relaxed);
        pending += 1;
        if last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            if pending > 1 {
                log::warn!("{label} : {err} ({pending} décrochages depuis le dernier message)");
            } else {
                log::warn!("{label} : {err}");
            }
            last_log = Some(Instant::now());
            pending = 0;
        }
    }
}

/// Annonce la taille de période réellement accordée, à la première trame.
///
/// C'est le plancher de latence du flux et la marge dont dispose le
/// planificateur avant un décrochage — deux choses que `BufferSize::Default`
/// laisse décider au pilote, et que rien n'affiche autrement.
fn announce_period(label: &'static str, announced: &mut bool, frames: usize, rate: u32) {
    if *announced {
        return;
    }
    *announced = true;
    let ms = frames as f32 * 1000.0 / rate.max(1) as f32;
    log::info!("{label} : période de {frames} trames ({ms:.1} ms)");
}

fn build_input<T>(
    dev: &Device,
    config: StreamConfig,
    mut producer: rtrb::Producer<f32>,
    stats: Arc<AudioStats>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let rate = config.sample_rate;
    let channels = config.channels as usize;
    let mut announced = false;
    let stats_for_errors = Arc::clone(&stats);
    let stream = dev
        .build_input_stream::<T, _, _>(
            config,
            move |data, _| {
                announce_period("entrée audio", &mut announced, data.len() / channels.max(1), rate);
                let mut dropped = 0u64;
                for &sample in data {
                    // Un tampon plein signifie que la sortie ne consomme pas :
                    // on jette plutôt que de bloquer le callback temps réel.
                    if producer.push(f32::from_sample(sample)).is_err() {
                        dropped += 1;
                    }
                }
                if dropped > 0 {
                    stats.overruns.fetch_add(dropped, Ordering::Relaxed);
                }
            },
            report_errors("entrée audio", stats_for_errors),
            None,
        )
        .context("construction du flux d'entrée")?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
fn build_output<T>(
    dev: &Device,
    config: StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    in_channels: usize,
    out_channels: usize,
    mut drift: DriftController,
    target_frames: f64,
    controls: Arc<AudioControls>,
    stats: Arc<AudioStats>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut interp = Interpolator::new(in_channels);
    let mut in_frame = vec![0.0f32; in_channels];
    let mut out_frame = vec![0.0f32; out_channels];
    // On ne commence à jouer qu'une fois la cible atteinte : sinon les
    // premières centaines de millisecondes ne sont qu'une suite de manques.
    let mut primed = false;

    let rate = config.sample_rate;
    let mut announced = false;
    let stats_for_errors = Arc::clone(&stats);
    let stream = dev
        .build_output_stream::<T, _, _>(
            config,
            move |data, _| {
                announce_period("sortie audio", &mut announced, data.len() / out_channels.max(1), rate);
                let fill = (consumer.slots() / in_channels) as f64;
                stats.fill_frames.store(fill as u32, Ordering::Relaxed);

                if !primed {
                    if fill < target_frames {
                        data.fill(T::from_sample(0.0f32));
                        return;
                    }
                    primed = true;
                }

                let ratio = drift.update(fill);
                stats.ratio_ppm.store((ratio * 1e6) as u32, Ordering::Relaxed);
                let gain = controls.effective_gain();

                let mut underran = false;
                for chunk in data.chunks_mut(out_channels) {
                    let ok = interp.next_frame(
                        ratio,
                        |frame| {
                            // On ne dépile que si la trame complète est
                            // disponible : dépiler à moitié désalignerait les
                            // canaux jusqu'à la fin du flux.
                            if consumer.slots() < in_channels {
                                return false;
                            }
                            for slot in frame.iter_mut() {
                                match consumer.pop() {
                                    Ok(v) => *slot = v,
                                    Err(_) => return false,
                                }
                            }
                            true
                        },
                        &mut in_frame,
                    );

                    if !ok {
                        underran = true;
                        for sample in chunk.iter_mut() {
                            *sample = T::from_sample(0.0f32);
                        }
                        continue;
                    }

                    map_channels(&in_frame, &mut out_frame, gain);
                    for (dst, src) in chunk.iter_mut().zip(out_frame.iter()) {
                        *dst = T::from_sample(*src);
                    }
                }

                if underran {
                    stats.underruns.fetch_add(1, Ordering::Relaxed);
                    interp.reset();
                    primed = false;
                }
            },
            report_errors("sortie audio", stats_for_errors),
            None,
        )
        .context("construction du flux de sortie")?;
    Ok(stream)
}
