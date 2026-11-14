//! QR codes, so the headset can open the Quest share link by looking at the screen
//! (Quest 3 / 3S: the built-in QR code scanner reads codes through the passthrough cameras).

use anyhow::{Result, anyhow};
use qrcodegen::{QrCode, QrCodeEcc};

/// White border, in modules. The QR standard asks for 4.
const QUIET: usize = 4;

pub struct Qr {
    size: usize,
    dark: Vec<bool>,
}

impl Qr {
    pub fn new(text: &str) -> Result<Self> {
        // Medium error correction: tolerates moiré and glare when read off a monitor.
        let code = QrCode::encode_text(text, QrCodeEcc::Medium)
            .map_err(|_| anyhow!("text too long for a QR code"))?;
        let n = code.size() as usize;
        let size = n + 2 * QUIET;
        let mut dark = vec![false; size * size];
        for y in 0..n {
            for x in 0..n {
                dark[(y + QUIET) * size + x + QUIET] = code.get_module(x as i32, y as i32);
            }
        }
        Ok(Self { size, dark })
    }

    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y * self.size + x]
    }

    /// Black-on-white RGBA pixels with `scale` pixels per module. Returns (side, pixels).
    pub fn to_rgba(&self, scale: usize) -> (usize, Vec<u8>) {
        let side = self.size * scale;
        let mut px = Vec::with_capacity(side * side * 4);
        for y in 0..side {
            for x in 0..side {
                let v = if self.is_dark(x / scale, y / scale) { 0 } else { 255 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        (side, px)
    }

    /// For terminals: two module rows per line with '▀', colours set explicitly
    /// (black on white) so it scans on dark and light terminal themes alike.
    pub fn to_terminal(&self) -> String {
        let mut s = String::new();
        for y in (0..self.size).step_by(2) {
            for x in 0..self.size {
                let fg = if self.is_dark(x, y) { 16 } else { 231 };
                let bg = if self.is_dark(x, y + 1) { 16 } else { 231 };
                s.push_str(&format!("\x1b[38;5;{fg};48;5;{bg}m▀"));
            }
            s.push_str("\x1b[0m\n");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_finder_patterns_and_border() {
        let q = Qr::new("https://192.168.1.20:8781/t/abcdefghjkm/").unwrap();
        let n = q.size - 2 * QUIET;
        assert!(n >= 21 && (n - 17).is_multiple_of(4), "QR sides are 17 + 4·version");
        // Border is white; each finder pattern's corner and centre are dark.
        assert!((0..q.size).all(|i| !q.is_dark(i, 0) && !q.is_dark(0, i)));
        for (fx, fy) in [(0, 0), (n - 7, 0), (0, n - 7)] {
            assert!(q.is_dark(QUIET + fx, QUIET + fy));
            assert!(q.is_dark(QUIET + fx + 3, QUIET + fy + 3));
            assert!(!q.is_dark(QUIET + fx + 1, QUIET + fy + 1));
        }
        let (side, px) = q.to_rgba(3);
        assert_eq!(px.len(), side * side * 4);
    }
}
