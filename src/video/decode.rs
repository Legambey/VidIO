//! Décodage des formats compressés vers un tampon uploadable sur le GPU.
//!
//! Rectification d'un point du plan initial : le YUYV part bien sur le GPU sans
//! toucher au CPU, mais le MJPEG, lui, coûte forcément un décodage CPU. Ce n'est
//! pas « pas de conversion couleur », c'est « pas de conversion couleur quand le
//! format le permet ». D'où la préférence donnée au YUYV à la négociation, le
//! MJPEG ne servant que lorsque la bande passante USB ne laisse pas le choix.

use anyhow::{Context, Result, anyhow};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// Décodeur MJPEG réutilisable : les options et le tampon de sortie sont
/// conservés d'une trame à l'autre pour ne pas réallouer à chaque image.
pub struct MjpegDecoder {
    options: DecoderOptions,
}

impl Default for MjpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MjpegDecoder {
    pub fn new() -> Self {
        // Sortie en RGBA : c'est le seul agencement 4 octets par pixel qui
        // corresponde directement à un format de texture wgpu, donc pas de
        // repadding derrière.
        Self { options: DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA) }
    }

    /// Décode une trame JPEG dans `out`, redimensionné au besoin.
    /// Renvoie les dimensions décodées.
    pub fn decode_into(&mut self, jpeg: &[u8], out: &mut Vec<u8>) -> Result<(u32, u32)> {
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), self.options);
        decoder.decode_headers().context("en-têtes JPEG illisibles")?;

        let info = decoder.info().ok_or_else(|| anyhow!("dimensions JPEG absentes"))?;
        let size = decoder
            .output_buffer_size()
            .ok_or_else(|| anyhow!("taille de sortie JPEG inconnue"))?;

        if out.len() != size {
            out.resize(size, 0);
        }
        decoder.decode_into(out).context("décodage JPEG")?;

        Ok((info.width as u32, info.height as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage_without_panicking() {
        let mut dec = MjpegDecoder::new();
        let mut out = Vec::new();
        assert!(dec.decode_into(&[0xde, 0xad, 0xbe, 0xef], &mut out).is_err());
    }
}
