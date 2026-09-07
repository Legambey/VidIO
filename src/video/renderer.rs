//! Rendu GPU : upload de la trame, correction colorimétrique, effets CRT.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::camera::{Frame, FrameFormat, PixelFormat, YuvMatrix};
use crate::config::{ColorSettings, CrtSettings, PresentMode};

const FLAG_YUYV: u32 = 1;
const FLAG_BT709: u32 = 2;
const FLAG_FULL_RANGE: u32 = 4;
const FLAG_CRT: u32 = 8;
const FLAG_SMOOTH: u32 = 16;
const FLAG_SRGB_SURFACE: u32 = 32;
const FLAG_GREY: u32 = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    src_size: [f32; 2],
    view_size: [f32; 2],
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    scanline: f32,
    mask: f32,
    curvature: f32,
    halation: f32,
    flags: u32,
    _pad: [u32; 3],
}

/// Zone d'affichage dans la fenêtre, en pixels physiques.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Facteur d'agrandissement appliqué, pour l'affichage dans l'overlay.
    pub scale: f32,
}

/// Calcule la zone d'affichage centrée pour une source dans une fenêtre.
///
/// En mode entier, le facteur est tronqué à l'entier inférieur : chaque pixel
/// source devient un carré de N×N pixels écran, sans ligne dupliquée ni
/// interpolation. C'est ce qui distingue une image nette d'une image floue sur
/// du contenu à basse résolution.
pub fn compute_viewport(win: (u32, u32), src: (u32, u32), integer: bool) -> Viewport {
    let (win_w, win_h) = win;
    let (src_w, src_h) = src;
    if src_w == 0 || src_h == 0 || win_w == 0 || win_h == 0 {
        return Viewport { x: 0, y: 0, width: win_w, height: win_h, scale: 1.0 };
    }

    let fit = (win_w as f32 / src_w as f32).min(win_h as f32 / src_h as f32);

    // Sous un facteur 1, l'entier n'a pas de sens : on ne peut pas afficher
    // moins d'un pixel écran par pixel source sans en jeter. On retombe alors
    // sur l'ajustement proportionnel.
    let scale = if integer && fit >= 1.0 { fit.floor() } else { fit };

    let width = (src_w as f32 * scale).round().max(1.0) as u32;
    let height = (src_h as f32 * scale).round().max(1.0) as u32;

    Viewport {
        x: (win_w.saturating_sub(width)) / 2,
        y: (win_h.saturating_sub(height)) / 2,
        width: width.min(win_w),
        height: height.min(win_h),
        scale,
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,

    texture: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    /// Format de la trame décrivant la texture actuelle.
    texture_format: Option<FrameFormat>,

    pub present_mode: wgpu::PresentMode,
    pub adapter_name: String,
    srgb_surface: bool,
}

/// Dimensions de la texture qu'occupera une trame de ce format.
///
/// Ce n'est pas toujours la taille de l'image : le YUYV tient dans une texture
/// RGBA de demi-largeur, un texel portant deux pixels. Un flux compressé, lui,
/// arrive déjà décodé en RGBA pleine largeur — c'est le format publié par la
/// capture, pas celui de l'appareil, qu'il faut passer ici.
pub fn texture_size(pixfmt: PixelFormat, width: u32, height: u32) -> (u32, u32) {
    match pixfmt {
        PixelFormat::Yuyv => (width.div_ceil(2), height),
        _ => (width, height),
    }
}

impl Renderer {
    pub fn new(window: Arc<Window>, present: PresentMode) -> Result<Self> {
        let size = window.inner_size();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("création de la surface")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| anyhow!("aucun adaptateur GPU utilisable : {e}"))?;
        let adapter_name = adapter.get_info().name;

        // Les limites de l'adaptateur, et non celles par défaut de wgpu : ces
        // dernières plafonnent les textures à 2048 pixels de côté — une valeur
        // héritée du web, qui rejetait ici tout format au-delà du 1080p alors
        // que le moindre GPU intégré tient le 16384.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vidio"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .context("ouverture du périphérique GPU")?;

        // Par défaut, wgpu tue le processus à la première erreur de validation.
        // Un moniteur vidéo n'a pas à disparaître parce qu'une trame n'a pas
        // plu : on trace et on continue avec l'image précédente.
        device.on_uncaptured_error(Arc::new(|e| log::error!("erreur GPU : {e}")));

        let caps = surface.get_capabilities(&adapter);

        // On préfère une surface non sRGB : ce qu'on calcule est alors
        // exactement ce qui s'affiche, sans conversion cachée. Si le pilote n'en
        // propose pas, le shader annulera l'encodage.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let srgb_surface = format.is_srgb();

        let present_mode = pick_present_mode(&caps.present_modes, present);

        // On part de la configuration par défaut de la surface et on n'ajuste
        // que ce qui nous concerne : les champs restants suivront les évolutions
        // de wgpu sans qu'on ait à les énumérer.
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("surface incompatible avec cet adaptateur"))?;
        config.format = format;
        config.present_mode = present_mode;
        // Une seule trame en vol : on veut la présenter au plus tôt, pas lisser
        // une cadence.
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vidio-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vidio-bind-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vidio-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vidio-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vidio-uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            uniform_buffer,
            texture: None,
            bind_group: None,
            texture_format: None,
            present_mode,
            adapter_name,
            srgb_surface,
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Côté maximal d'une texture sur ce GPU. C'est lui qui décide quels
    /// formats de capture sont affichables.
    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Envoie une trame sur le GPU, en recréant la texture si le format a changé.
    pub fn upload(&mut self, frame: &Frame) -> Result<()> {
        let fmt = frame.format;

        // Le YUYV tient dans une texture RGBA de demi-largeur : un texel = deux
        // pixels. Aucun octet n'est touché par le CPU, le dépaquetage a lieu
        // dans le shader.
        let (bytes_per_pixel, texture_format) = match fmt.pixfmt {
            PixelFormat::Yuyv => (2u32, wgpu::TextureFormat::Rgba8Unorm),
            PixelFormat::Rgba8 => (4, wgpu::TextureFormat::Rgba8Unorm),
            PixelFormat::Grey => (1, wgpu::TextureFormat::R8Unorm),
            other => anyhow::bail!("format {} non géré par le rendu", other.name()),
        };
        let (tex_width, tex_height) = texture_size(fmt.pixfmt, fmt.width, fmt.height);

        // Vérifié avant de la créer : une texture trop grande est une erreur de
        // validation, et wgpu les traite en dehors de tout `Result`.
        let limit = self.max_texture_dimension();
        if tex_width > limit || tex_height > limit {
            anyhow::bail!(
                "{}x{} dépasse ce que ce GPU peut afficher ({limit} pixels de côté)",
                fmt.width,
                fmt.height
            );
        }

        if self.texture_format != Some(fmt) {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vidio-frame"),
                size: wgpu::Extent3d {
                    width: tex_width,
                    height: tex_height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: texture_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vidio-bind"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.uniform_buffer.as_entire_binding(),
                    },
                ],
            }));
            self.texture = Some(texture);
            self.texture_format = Some(fmt);
        }

        let texture = self.texture.as_ref().expect("texture créée juste au-dessus");
        let stride = if fmt.stride > 0 { fmt.stride } else { fmt.width * bytes_per_pixel };
        let expected = stride as usize * fmt.height as usize;
        if frame.data.len() < expected {
            anyhow::bail!(
                "trame incomplète : {} octets pour {expected} attendus",
                frame.data.len()
            );
        }

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data[..expected],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(fmt.height),
            },
            wgpu::Extent3d { width: tex_width, height: fmt.height, depth_or_array_layers: 1 },
        );

        Ok(())
    }

    /// Dessine la vidéo, puis laisse l'appelant ajouter son interface.
    ///
    /// `overlay` reçoit l'encodeur et la vue de sortie une fois la vidéo tracée,
    /// ce qui évite au moteur de rendu de connaître quoi que ce soit d'egui.
    pub fn render(
        &mut self,
        color: &ColorSettings,
        crt: &CrtSettings,
        integer_scale: bool,
        overlay: impl FnOnce(&wgpu::Device, &wgpu::Queue, &mut wgpu::CommandEncoder, &wgpu::TextureView),
    ) -> Result<Viewport> {
        // Avant la première trame il n'y a pas de vidéo à tracer, mais la passe
        // a quand même lieu : l'overlay doit être utilisable, et ses textures
        // doivent être consommées — egui panique si on les laisse en plan.
        let video = self.texture_format.map(|fmt| {
            let viewport = compute_viewport(
                (self.config.width, self.config.height),
                (fmt.width, fmt.height),
                integer_scale,
            );

            let mut flags = 0;
            if fmt.pixfmt == PixelFormat::Yuyv {
                flags |= FLAG_YUYV;
            }
            if fmt.pixfmt == PixelFormat::Grey {
                flags |= FLAG_GREY;
            }
            if fmt.color.matrix == YuvMatrix::Bt709 {
                flags |= FLAG_BT709;
            }
            if fmt.color.full_range {
                flags |= FLAG_FULL_RANGE;
            }
            if crt.enabled {
                flags |= FLAG_CRT;
            }
            // L'interpolation n'a de sens qu'aux facteurs non entiers : à
            // facteur entier elle ne ferait que ramollir une image exacte.
            if !integer_scale && viewport.scale.fract() != 0.0 {
                flags |= FLAG_SMOOTH;
            }
            if self.srgb_surface {
                flags |= FLAG_SRGB_SURFACE;
            }

            self.queue.write_buffer(
                &self.uniform_buffer,
                0,
                bytemuck::bytes_of(&Uniforms {
                    src_size: [fmt.width as f32, fmt.height as f32],
                    view_size: [viewport.width as f32, viewport.height as f32],
                    brightness: color.brightness,
                    contrast: color.contrast,
                    saturation: color.saturation,
                    gamma: color.gamma,
                    scanline: crt.scanline,
                    mask: crt.mask,
                    curvature: crt.curvature,
                    halation: crt.halation,
                    flags,
                    _pad: [0; 3],
                }),
            );

            viewport
        });

        let viewport = video.unwrap_or(Viewport { x: 0, y: 0, width: 0, height: 0, scale: 1.0 });

        use wgpu::CurrentSurfaceTexture;
        let output = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(t) | CurrentSurfaceTexture::Suboptimal(t) => t,
            // Redimensionnement en cours ou surface perdue : on reconfigure et
            // on laisse passer cette trame. La suivante arrive dans 16 ms.
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(viewport);
            }
            // Fenêtre masquée ou compositeur occupé : rien à dessiner.
            CurrentSurfaceTexture::Occluded | CurrentSurfaceTexture::Timeout => {
                return Ok(viewport);
            }
            CurrentSurfaceTexture::Validation => {
                return Err(anyhow!("surface refusée par la validation"));
            }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("vidio-frame") });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vidio-video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if let (Some(bind_group), true) =
                (self.bind_group.as_ref(), viewport.width > 0 && viewport.height > 0)
            {
                pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    viewport.width as f32,
                    viewport.height as f32,
                    0.0,
                    1.0,
                );
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        overlay(&self.device, &self.queue, &mut encoder, &view);

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(output);

        Ok(viewport)
    }
}

/// Choisit le mode de présentation en dégradant vers ce que la surface accepte.
///
/// `Immediate` présente sans attendre le balayage : c'est le minimum de latence,
/// au prix d'une déchirure possible. `Mailbox` remplace la trame en attente sans
/// bloquer, `Fifo` est le seul garanti partout mais impose d'attendre.
fn pick_present_mode(available: &[wgpu::PresentMode], wanted: PresentMode) -> wgpu::PresentMode {
    let order: &[wgpu::PresentMode] = match wanted {
        PresentMode::Immediate => &[
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Fifo,
        ],
        PresentMode::Mailbox => &[
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Fifo,
        ],
        PresentMode::Fifo => &[wgpu::PresentMode::Fifo],
    };

    order
        .iter()
        .copied()
        .find(|m| available.contains(m))
        .unwrap_or(wgpu::PresentMode::Fifo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_fits_in_a_half_width_texture() {
        assert_eq!(texture_size(PixelFormat::Yuyv, 3840, 2160), (1920, 2160));
        // Largeur impaire : le texel de bord est à moitié rempli, pas tronqué.
        assert_eq!(texture_size(PixelFormat::Yuyv, 721, 480), (361, 480));
    }

    #[test]
    fn a_compressed_stream_takes_the_full_width() {
        // C'est le format publié qui compte : le MJPEG arrive décodé en RGBA,
        // et c'est lui qui bute sur la limite du GPU.
        assert_eq!(PixelFormat::Mjpeg.published(), PixelFormat::Rgba8);
        assert_eq!(texture_size(PixelFormat::Mjpeg.published(), 3840, 2160), (3840, 2160));
    }

    #[test]
    fn integer_scale_rounds_down_and_centres() {
        // 1920x1080 pour une source 640x480 : facteur 2 (2,25 tronqué), reste
        // 640 px horizontalement et 120 verticalement, répartis de part et d'autre.
        let v = compute_viewport((1920, 1080), (640, 480), true);
        assert_eq!(v.scale, 2.0);
        assert_eq!((v.width, v.height), (1280, 960));
        assert_eq!((v.x, v.y), (320, 60));
    }

    #[test]
    fn proportional_scale_fills_one_axis() {
        let v = compute_viewport((1920, 1080), (640, 480), false);
        assert_eq!((v.width, v.height), (1440, 1080));
        assert_eq!(v.x, 240);
        assert_eq!(v.y, 0);
    }

    #[test]
    fn window_smaller_than_source_falls_back_to_proportional() {
        // Un facteur entier n'existe pas en dessous de 1 : on ne peut pas
        // afficher un demi-pixel, donc on ajuste proportionnellement.
        let v = compute_viewport((320, 240), (640, 480), true);
        assert_eq!((v.width, v.height), (320, 240));
        assert!(v.scale < 1.0);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let v = compute_viewport((0, 0), (640, 480), true);
        assert_eq!((v.width, v.height), (0, 0));
        let v = compute_viewport((800, 600), (0, 0), true);
        assert_eq!((v.width, v.height), (800, 600));
    }

    #[test]
    fn present_mode_degrades_when_unavailable() {
        let only_fifo = [wgpu::PresentMode::Fifo];
        assert_eq!(
            pick_present_mode(&only_fifo, PresentMode::Immediate),
            wgpu::PresentMode::Fifo
        );

        let no_immediate = [wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox];
        assert_eq!(
            pick_present_mode(&no_immediate, PresentMode::Immediate),
            wgpu::PresentMode::Mailbox
        );
    }
}

