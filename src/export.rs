//! Output writers: stereo layouts, anaglyph, depth map and a 3D mesh (.glb).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use image::{RgbImage, imageops};
use rayon::prelude::*;

use crate::postprocess::DepthMap;
use crate::settings::{ImageFormat, OutputSettings};
use crate::stereo::StereoPair;

/// Full side-by-side: left eye on the left. 2W × H.
pub fn sbs_full(p: &StereoPair) -> RgbImage {
    let (w, h) = p.left.dimensions();
    let mut out = RgbImage::new(w * 2, h);
    imageops::replace(&mut out, &p.left, 0, 0);
    imageops::replace(&mut out, &p.right, w as i64, 0);
    out
}

/// Half side-by-side: both eyes squeezed into the original W × H frame.
pub fn sbs_half(p: &StereoPair) -> RgbImage {
    let (w, h) = p.left.dimensions();
    let hw = (w / 2).max(1);
    let f = imageops::FilterType::Triangle;
    let (l, r) = rayon::join(
        || imageops::resize(&p.left, hw, h, f),
        || imageops::resize(&p.right, hw, h, f),
    );
    let mut out = RgbImage::new(hw * 2, h);
    imageops::replace(&mut out, &l, 0, 0);
    imageops::replace(&mut out, &r, hw as i64, 0);
    out
}

/// Top/bottom: left eye on top. W × 2H.
pub fn top_bottom(p: &StereoPair) -> RgbImage {
    let (w, h) = p.left.dimensions();
    let mut out = RgbImage::new(w, h * 2);
    imageops::replace(&mut out, &p.left, 0, 0);
    imageops::replace(&mut out, &p.right, 0, h as i64);
    out
}

/// Red/cyan anaglyph using Dubois' least-squares matrices (much better colour than the
/// naive "red channel from left" method).
pub fn anaglyph(p: &StereoPair) -> RgbImage {
    const L: [[f32; 3]; 3] = [
        [0.437, 0.449, 0.164],
        [-0.062, -0.062, -0.024],
        [-0.048, -0.050, -0.017],
    ];
    const R: [[f32; 3]; 3] = [
        [-0.011, -0.032, -0.007],
        [0.377, 0.761, 0.009],
        [-0.026, -0.093, 1.234],
    ];
    let (w, h) = p.left.dimensions();
    let (l, r) = (p.left.as_raw(), p.right.as_raw());
    let mut out = vec![0u8; l.len()];
    out.par_chunks_mut(3).enumerate().for_each(|(i, o)| {
        let lp = [l[i * 3] as f32, l[i * 3 + 1] as f32, l[i * 3 + 2] as f32];
        let rp = [r[i * 3] as f32, r[i * 3 + 1] as f32, r[i * 3 + 2] as f32];
        for c in 0..3 {
            let v = L[c][0] * lp[0] + L[c][1] * lp[1] + L[c][2] * lp[2]
                + R[c][0] * rp[0] + R[c][1] * rp[1] + R[c][2] * rp[2];
            o[c] = v.round().clamp(0.0, 255.0) as u8;
        }
    });
    RgbImage::from_raw(w, h, out).expect("size matches")
}

pub fn save_rgb(img: &RgbImage, path: &Path, o: &OutputSettings) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut wr = BufWriter::new(file);
    match o.format {
        ImageFormat::Jpeg => {
            let enc =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut wr, o.jpeg_quality.clamp(1, 100));
            img.write_with_encoder(enc)?;
        }
        ImageFormat::Png => img.write_to(&mut wr, image::ImageFormat::Png)?,
    }
    wr.flush()?;
    Ok(())
}

pub fn save_depth_png(depth: &DepthMap, path: &Path) -> Result<()> {
    depth
        .to_gray16()
        .save_with_format(path, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------------------
// GLB mesh
// ---------------------------------------------------------------------------------------

/// Write a textured, depth-displaced mesh as a binary glTF (.glb).
///
/// The camera that "took" the photo sits at the origin looking down -Z. The nearest point
/// is 1 m away and the farthest is `mesh_depth` m away, so standing at the origin in VR
/// you see the photo exactly as shot, and moving your head gives real parallax.
pub fn write_glb(img: &RgbImage, depth: &DepthMap, o: &OutputSettings, path: &Path) -> Result<()> {
    let (w, h) = img.dimensions();
    let cells = o.mesh_detail.clamp(16, 2048) as f32;
    let (gw, gh) = if w >= h {
        (cells as u32, ((cells * h as f32 / w as f32).round() as u32).max(1))
    } else {
        (((cells * w as f32 / h as f32).round() as u32).max(1), cells as u32)
    };
    let (vw, vh) = (gw + 1, gh + 1);

    let near = 1.0f32;
    let far = o.mesh_depth.max(1.01);
    let tan_x = (o.mesh_fov.clamp(10.0, 170.0).to_radians() / 2.0).tan();
    let tan_y = tan_x * h as f32 / w as f32;

    let mut positions = Vec::with_capacity((vw * vh * 3) as usize);
    let mut uvs = Vec::with_capacity((vw * vh * 2) as usize);
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for j in 0..vh {
        let v = j as f32 / gh as f32;
        for i in 0..vw {
            let u = i as f32 / gw as f32;
            let d = depth.sample(u * (w - 1) as f32, v * (h - 1) as f32);
            // Interpolate in inverse depth so equal depth-map steps look perceptually even.
            let z = 1.0 / (1.0 / far + d * (1.0 / near - 1.0 / far));
            let p = [(u * 2.0 - 1.0) * tan_x * z, (1.0 - v * 2.0) * tan_y * z, -z];
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
            positions.extend_from_slice(&p);
            uvs.extend_from_slice(&[u, v]);
        }
    }
    let mut indices = Vec::with_capacity((gw * gh * 6) as usize);
    for j in 0..gh {
        for i in 0..gw {
            let a = j * vw + i;
            let b = a + 1;
            let c = a + vw;
            let d = c + 1;
            // Counter-clockwise when seen from the origin.
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }

    // Texture: JPEG, at most 4096 px wide (most headsets' texture limit).
    let tex = if w > 4096 {
        imageops::resize(img, 4096, (4096.0 * h as f32 / w as f32) as u32, imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(&tex)?;

    // Binary buffer layout: positions | uvs | indices | jpeg (each 4-byte aligned).
    let mut bin: Vec<u8> = Vec::new();
    let view = |bin: &mut Vec<u8>, bytes: &[u8]| -> (usize, usize) {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let off = bin.len();
        bin.extend_from_slice(bytes);
        (off, bytes.len())
    };
    let f32s = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    let (pos_off, pos_len) = view(&mut bin, &f32s(&positions));
    let (uv_off, uv_len) = view(&mut bin, &f32s(&uvs));
    let idx_bytes: Vec<u8> = indices.iter().flat_map(|x| x.to_le_bytes()).collect();
    let (idx_off, idx_len) = view(&mut bin, &idx_bytes);
    let (img_off, img_len) = view(&mut bin, &jpeg);
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "Stereowright" },
        "extensionsUsed": ["KHR_materials_unlit"],
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0, "name": "photo" }],
        "meshes": [{ "primitives": [{
            "attributes": { "POSITION": 0, "TEXCOORD_0": 1 },
            "indices": 2,
            "material": 0
        }]}],
        "materials": [{
            "name": "photo",
            "doubleSided": true,
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0
            },
            "extensions": { "KHR_materials_unlit": {} }
        }],
        "textures": [{ "source": 0, "sampler": 0 }],
        "samplers": [{ "magFilter": 9729, "minFilter": 9987, "wrapS": 33071, "wrapT": 33071 }],
        "images": [{ "bufferView": 3, "mimeType": "image/jpeg" }],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": pos_off, "byteLength": pos_len, "target": 34962 },
            { "buffer": 0, "byteOffset": uv_off, "byteLength": uv_len, "target": 34962 },
            { "buffer": 0, "byteOffset": idx_off, "byteLength": idx_len, "target": 34963 },
            { "buffer": 0, "byteOffset": img_off, "byteLength": img_len }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": vw * vh, "type": "VEC3", "min": min, "max": max },
            { "bufferView": 1, "componentType": 5126, "count": vw * vh, "type": "VEC2" },
            { "bufferView": 2, "componentType": 5125, "count": indices.len(), "type": "SCALAR" }
        ]
    });
    let mut json_bytes = serde_json::to_vec(&json)?;
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }

    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut f = BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    f.write_all(b"glTF")?;
    f.write_all(&2u32.to_le_bytes())?;
    f.write_all(&(total as u32).to_le_bytes())?;
    f.write_all(&(json_bytes.len() as u32).to_le_bytes())?;
    f.write_all(b"JSON")?;
    f.write_all(&json_bytes)?;
    f.write_all(&(bin.len() as u32).to_le_bytes())?;
    f.write_all(b"BIN\0")?;
    f.write_all(&bin)?;
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_have_expected_sizes() {
        let p = StereoPair {
            left: RgbImage::new(40, 30),
            right: RgbImage::new(40, 30),
        };
        assert_eq!(sbs_full(&p).dimensions(), (80, 30));
        assert_eq!(sbs_half(&p).dimensions(), (40, 30));
        assert_eq!(top_bottom(&p).dimensions(), (40, 60));
        assert_eq!(anaglyph(&p).dimensions(), (40, 30));
    }

    #[test]
    fn glb_is_well_formed() {
        let img = RgbImage::from_pixel(64, 48, image::Rgb([200, 100, 50]));
        let depth = DepthMap {
            width: 64,
            height: 48,
            data: (0..64 * 48).map(|i| (i % 64) as f32 / 63.0).collect(),
        };
        let dir = std::env::temp_dir().join(format!("stereowright-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.glb");
        let o = OutputSettings {
            mesh_detail: 32,
            ..Default::default()
        };
        write_glb(&img, &depth, &o, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"glTF");
        let total = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(total, bytes.len());
        let jlen = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&bytes[20..20 + jlen]).unwrap();
        assert_eq!(json["accessors"][0]["count"], 33 * 25);
        std::fs::remove_dir_all(dir).ok();
    }
}
