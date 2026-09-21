//! VidIO — moniteur vidéo/audio basse latence pour cartes d'acquisition USB.
//!
//! Cette étape pose le socle : identification des périphériques, configuration
//! persistante, capture V4L2 et transit audio. Les sous-commandes permettent de
//! vérifier le matériel avant que le rendu GPU n'entre en jeu.

mod app;
mod audio;
mod camera;
mod config;
mod device;
mod ui;
mod video;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::audio::{AudioControls, Passthrough};
use crate::camera::{FormatRequest, PixelFormat};
use crate::config::Config;
use crate::video::decode::MjpegDecoder;

#[derive(Parser)]
#[command(name = "vidio", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ouvre la fenêtre de visualisation. C'est la commande par défaut.
    Run {
        /// Clé, chemin, index ou fragment de nom. À défaut : le dernier utilisé.
        device: Option<String>,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long)]
        fourcc: Option<String>,
        /// N'ouvre pas le transit audio.
        #[arg(long)]
        no_audio: bool,
    },
    /// Liste les périphériques vidéo et audio détectés.
    List,
    /// Détaille les formats et cadences d'un périphérique vidéo.
    Formats {
        /// Clé, chemin, index ou fragment de nom.
        device: String,
    },
    /// Liste les contrôles matériels d'un périphérique.
    Controls { device: String },
    /// Écrit un contrôle matériel et l'enregistre dans le profil.
    // Plusieurs contrôles ont une plage négative (la luminosité va de -64 à 64) :
    // sans ça, clap prend `-20` pour une option courte inconnue.
    #[command(allow_negative_numbers = true)]
    Set {
        device: String,
        /// Nom du contrôle, tel qu'affiché par `controls`.
        control: String,
        value: i64,
    },
    /// Capture pendant quelques secondes et mesure la cadence réelle.
    Bench {
        device: String,
        #[arg(long, default_value_t = 5)]
        secs: u64,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        fps: Option<u32>,
        /// Force un format de pixel ("YUYV", "MJPG").
        #[arg(long)]
        fourcc: Option<String>,
    },
    /// Fait transiter l'audio de l'entrée vers la sortie.
    Audio {
        #[arg(long, default_value_t = 10)]
        secs: u64,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value_t = 20.0)]
        latency_ms: f32,
        /// Gain appliqué à la sortie (1.0 = niveau d'entrée).
        #[arg(long, default_value_t = 1.0)]
        gain: f32,
    },
    /// Affiche le chemin et le contenu de la configuration.
    Config,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let command = Cli::parse().command.unwrap_or(Command::Run {
        device: None,
        width: None,
        height: None,
        fps: None,
        fourcc: None,
        no_audio: false,
    });

    match command {
        Command::Run { device, width, height, fps, fourcc, no_audio } => {
            cmd_run(device.as_deref(), width, height, fps, fourcc.as_deref(), !no_audio)
        }
        Command::List => cmd_list(),
        Command::Formats { device } => cmd_formats(&device),
        Command::Controls { device } => cmd_controls(&device),
        Command::Set { device, control, value } => cmd_set(&device, &control, value),
        Command::Bench { device, secs, width, height, fps, fourcc } => {
            cmd_bench(&device, secs, width, height, fps, fourcc.as_deref())
        }
        Command::Audio { secs, input, output, latency_ms, gain } => {
            cmd_audio(secs, input.as_deref(), output.as_deref(), latency_ms, gain)
        }
        Command::Config => cmd_config(),
    }
}

fn cmd_list() -> Result<()> {
    let devices = camera::enumerate()?;
    println!("Vidéo ({} périphérique(s))", devices.len());
    if devices.is_empty() {
        // Le premier obstacle n'est pas le même des deux côtés, et dans les deux
        // cas rien ne le signale : c'est une liste vide, point.
        #[cfg(target_os = "linux")]
        println!("  (aucun — vérifier l'appartenance au groupe « video »)");
        #[cfg(windows)]
        println!(
            "  (aucun — vérifier Paramètres › Confidentialité et sécurité › Caméra, \
             et « Autoriser les applications de bureau à accéder à votre caméra »)"
        );
    }
    for d in &devices {
        println!("  [{}] {}", d.index, d.label());
        println!("      nœud   : {}", d.path.display());
        println!("      pilote : {}", d.driver);
        if let Some(by_id) = &d.by_id {
            println!("      by-id  : {by_id}");
        }
        println!("      clé    : {}", d.key);
    }

    println!();
    let inputs = audio::enumerate_inputs().unwrap_or_default();
    println!("Entrées audio ({})", inputs.len());
    for d in &inputs {
        println!("  {}{}", d.name, if d.is_default { "  (défaut)" } else { "" });
        println!("      id : {}", d.id);
    }

    println!();
    let outputs = audio::enumerate_outputs().unwrap_or_default();
    println!("Sorties audio ({})", outputs.len());
    for d in &outputs {
        println!("  {}{}", d.name, if d.is_default { "  (défaut)" } else { "" });
        println!("      id : {}", d.id);
    }

    Ok(())
}

fn cmd_formats(spec: &str) -> Result<()> {
    let dev = camera::find(spec)?;
    let camera = camera::open(&dev)?;
    let caps = camera.caps()?;

    println!("{}", dev.label());
    println!("clé : {}\n", dev.key);

    let mut by_format: std::collections::BTreeMap<String, Vec<&camera::FormatCaps>> =
        Default::default();
    for c in &caps {
        by_format.entry(c.pixfmt.name()).or_default().push(c);
    }

    for (name, mut modes) in by_format {
        let compressed = modes.first().map(|m| m.pixfmt.is_compressed()).unwrap_or(false);
        println!("{name}{}", if compressed { "  (compressé — décodage CPU)" } else { "" });
        modes.sort_by_key(|m| std::cmp::Reverse((m.width, m.height)));
        for m in modes {
            let fps: Vec<String> = m.fps.iter().map(|f| f.to_string()).collect();
            println!("  {:>5}x{:<5}  {} fps", m.width, m.height, fps.join(", "));
        }
        println!();
    }

    Ok(())
}

fn cmd_controls(spec: &str) -> Result<()> {
    let dev = camera::find(spec)?;
    let camera = camera::open(&dev)?;
    let controls = camera.control_handle().controls()?;

    println!("{}\n", dev.label());
    for c in &controls {
        let state = match (c.read_only, c.inactive) {
            (true, _) => "  [lecture seule]",
            (_, true) => "  [inactif — dépend d'un réglage auto]",
            _ => "",
        };
        match &c.kind {
            camera::ControlKind::Integer { min, max, step } => {
                println!(
                    "  {:<32} {} (défaut {}, {}..{} pas {}){}",
                    c.name, c.current, c.default, min, max, step, state
                );
            }
            camera::ControlKind::Boolean => {
                println!(
                    "  {:<32} {} (défaut {}){}",
                    c.name,
                    c.current != 0,
                    c.default != 0,
                    state
                );
            }
            camera::ControlKind::Menu { items } => {
                let current = items
                    .iter()
                    .find(|(i, _)| *i as i64 == c.current)
                    .map(|(_, n)| n.as_str())
                    .unwrap_or("?");
                println!("  {:<32} {} = {current}{state}", c.name, c.current);
                for (i, name) in items {
                    println!("      {i} : {name}");
                }
            }
            camera::ControlKind::Button => println!("  {:<32} (action){state}", c.name),
        }
    }

    Ok(())
}

fn cmd_set(spec: &str, control: &str, value: i64) -> Result<()> {
    let dev = camera::find(spec)?;
    let camera = camera::open(&dev)?;
    let handle = camera.control_handle();

    let controls = handle.controls()?;
    let target = controls
        .iter()
        .find(|c| c.name == control)
        .or_else(|| controls.iter().find(|c| c.name.contains(control)))
        .with_context(|| format!("contrôle « {control} » inconnu sur ce périphérique"))?;

    handle.set_control(target.id, value)?;

    let mut cfg = Config::load()?;
    cfg.profile_mut(&dev.key).hw_controls.insert(target.name.clone(), value);
    cfg.save()?;

    println!("{} = {value}  (enregistré dans le profil {})", target.name, dev.key);
    Ok(())
}

/// Construit la demande de format en combinant profil enregistré et arguments.
fn build_request(
    profile: &crate::config::Profile,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    fourcc: Option<&str>,
) -> FormatRequest {
    let pixfmt = fourcc.or(profile.video.fourcc.as_deref()).map(|s| {
        let mut cc = [b' '; 4];
        for (dst, src) in cc.iter_mut().zip(s.bytes()) {
            *dst = src;
        }
        PixelFormat::from_fourcc(cc)
    });

    FormatRequest {
        width: width.unwrap_or(profile.video.width),
        height: height.unwrap_or(profile.video.height),
        fps: fps.unwrap_or(profile.video.fps),
        pixfmt,
        buffers: 3,
    }
}

fn cmd_run(
    spec: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    fourcc: Option<&str>,
    audio: bool,
) -> Result<()> {
    let config = Config::load()?;

    // Un périphérique demandé explicitement doit exister : se rabattre en
    // silence sur un autre serait pire que l'erreur. Le dernier utilisé, lui,
    // peut avoir été débranché — on prend alors ce qui est là.
    let dev = match spec {
        Some(spec) => camera::find(spec)?,
        None => {
            let devices = camera::enumerate()?;
            config
                .general
                .last_device
                .as_deref()
                .and_then(|key| devices.iter().find(|d| d.key.as_str() == key).cloned())
                .or_else(|| devices.into_iter().next())
                .context("aucun périphérique de capture détecté")?
        }
    };

    let profile = config.profile(&dev.key).cloned().unwrap_or_default();
    let request = build_request(&profile, width, height, fps, fourcc);

    log::info!("ouverture de {} ({})", dev.label(), dev.key);
    app::run(config, dev, request, audio)
}

fn cmd_bench(
    spec: &str,
    secs: u64,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    fourcc: Option<&str>,
) -> Result<()> {
    let dev = camera::find(spec)?;
    let cfg = Config::load()?;
    let profile = cfg.profile(&dev.key).cloned().unwrap_or_default();
    let req = build_request(&profile, width, height, fps, fourcc);

    let camera = camera::open(&dev)?;
    let capture = camera.start(req, None)?;
    let format = capture.format();

    println!("{}", dev.label());
    println!(
        "format négocié : {}x{} {} @ {} tampons",
        format.width,
        format.height,
        format.pixfmt.name(),
        3
    );
    println!("capture pendant {secs} s...\n");

    let mut decoder = MjpegDecoder::new();
    let mut rgba = Vec::new();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut decode_total = Duration::ZERO;
    let mut decode_max = Duration::ZERO;
    let mut first_frame: Option<Duration> = None;

    while Instant::now() < deadline {
        let Some(frame) = capture.wait_take(Duration::from_millis(500)) else {
            continue;
        };
        first_frame.get_or_insert_with(|| start.elapsed());
        frames += 1;
        bytes += frame.data.len() as u64;

        if frame.format.pixfmt == PixelFormat::Mjpeg {
            let t0 = Instant::now();
            match decoder.decode_into(&frame.data, &mut rgba) {
                Ok(_) => {
                    let dt = t0.elapsed();
                    decode_total += dt;
                    decode_max = decode_max.max(dt);
                }
                Err(e) => log::warn!("trame JPEG rejetée : {e:#}"),
            }
        }

        capture.recycle(frame);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let stats = capture.stats();
    let dropped = stats.dropped.load(Ordering::Relaxed);
    let errors = stats.errors.load(Ordering::Relaxed);

    println!("trames reçues     : {frames}  ({:.1} /s)", frames as f64 / elapsed);
    println!("trames écrasées   : {dropped}  (capture plus rapide que la consommation)");
    let missed = stats.missed.load(Ordering::Relaxed);
    if missed > 0 {
        println!("trames perdues    : {missed}  (jamais livrées par le pilote — bande passante USB)");
    }
    println!("erreurs / timeouts: {errors}");
    if let Some(t) = first_frame {
        println!("première trame    : {:.0} ms après le démarrage", t.as_secs_f64() * 1000.0);
    }
    println!(
        "débit moyen       : {:.1} Mo/s ({:.0} Ko par trame)",
        bytes as f64 / elapsed / 1e6,
        bytes as f64 / frames.max(1) as f64 / 1e3
    );

    if frames > 0 && format.pixfmt == PixelFormat::Mjpeg {
        println!(
            "décodage JPEG     : {:.2} ms en moyenne, {:.2} ms au pire",
            decode_total.as_secs_f64() * 1000.0 / frames as f64,
            decode_max.as_secs_f64() * 1000.0
        );
        println!(
            "                    soit {:.0} % d'un cœur à cette cadence",
            decode_total.as_secs_f64() / elapsed * 100.0
        );
    }

    Ok(())
}

fn cmd_audio(
    secs: u64,
    input: Option<&str>,
    output: Option<&str>,
    latency_ms: f32,
    gain: f32,
) -> Result<()> {
    let controls = Arc::new(AudioControls::default());
    controls.set_gain(gain);
    let pass = Passthrough::start(input, output, latency_ms, Arc::clone(&controls))?;

    println!(
        "entrée : {} ({} Hz, {} canaux)",
        pass.input_name, pass.input_rate, pass.input_channels
    );
    println!(
        "sortie : {} ({} Hz, {} canaux)",
        pass.output_name, pass.output_rate, pass.output_channels
    );
    println!("transit pendant {secs} s (Ctrl+C pour arrêter)\n");

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(1000));
        let s = &pass.stats;
        println!(
            "  tampon {:>5.1} ms   dérive {:>+5} ppm   manques {}   débordements {}",
            pass.latency_ms(),
            s.drift_ppm(),
            s.underruns.load(Ordering::Relaxed),
            s.overruns.load(Ordering::Relaxed),
        );
    }

    Ok(())
}

fn cmd_config() -> Result<()> {
    let path = Config::path()?;
    println!("{}\n", path.display());
    match std::fs::read_to_string(&path) {
        Ok(text) => print!("{text}"),
        Err(_) => println!("(pas encore de fichier — les valeurs par défaut s'appliquent)"),
    }
    Ok(())
}
