//! Reach Vision Loop Change Gate and Perceptual Hashing module.
//!
//! Provides perceptual hash (pHash / dHash) calculation, downsampling,
//! visual change distance calculation, region-of-interest (ROI) cropping,
//! and the vision loop change gate to prevent token burn on static screens.

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GenericImageView, GrayImage, ImageEncoder, imageops};
use serde::{Deserialize, Serialize};
use std::io::Cursor;

/// Estimated input tokens per VLM screenshot frame call (Gemini 3.8 Flash).
pub const ESTIMATED_TOKENS_PER_VLM_CALL: u64 = 1600;

/// Estimated cost per VLM screenshot frame call in USD ($0.15 / 1M tokens).
pub const ESTIMATED_COST_PER_VLM_CALL_USD: f64 = 0.00024;

/// Supported perceptual hash grid sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashSize {
    /// 8x8 grid (64 bits, standard dHash/pHash)
    Size8,
    /// 16x16 grid (256 bits, fine-grained change detection)
    Size16,
}

impl HashSize {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            HashSize::Size8 => (8, 8),
            HashSize::Size16 => (16, 16),
        }
    }

    pub fn bit_count(&self) -> usize {
        match self {
            HashSize::Size8 => 64,
            HashSize::Size16 => 256,
        }
    }
}

/// Region of Interest (ROI) for focused high-res crops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Roi {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Clamp the ROI to ensure it fits within given image bounds (img_w, img_h).
    pub fn clamp_to_bounds(&self, img_w: u32, img_h: u32) -> Option<Self> {
        if img_w == 0 || img_h == 0 {
            return None;
        }
        let x = self.x.min(img_w.saturating_sub(1));
        let y = self.y.min(img_h.saturating_sub(1));
        let width = self.width.max(1).min(img_w - x);
        let height = self.height.max(1).min(img_h - y);
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// Perceptual hash representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptualHash {
    pub bits: Vec<u8>,
    pub bit_count: usize,
}

impl PerceptualHash {
    pub fn new(bits: Vec<u8>, bit_count: usize) -> Self {
        Self { bits, bit_count }
    }

    pub fn from_u64(val: u64) -> Self {
        Self {
            bits: val.to_be_bytes().to_vec(),
            bit_count: 64,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        if self.bits.len() == 8 {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&self.bits);
            Some(u64::from_be_bytes(arr))
        } else {
            None
        }
    }

    /// Compute Hamming distance (number of differing bits) against another hash.
    pub fn hamming_distance(&self, other: &Self) -> usize {
        let max_len = self.bits.len().max(other.bits.len());
        let mut diff = 0usize;
        for i in 0..max_len {
            let b1 = self.bits.get(i).copied().unwrap_or(0);
            let b2 = other.bits.get(i).copied().unwrap_or(0);
            diff += (b1 ^ b2).count_ones() as usize;
        }
        diff
    }

    /// Normalized distance between 0.0 (identical) and 1.0 (completely opposite).
    pub fn normalized_distance(&self, other: &Self) -> f64 {
        let total_bits = self.bit_count.max(other.bit_count).max(1);
        let distance = self.hamming_distance(other);
        (distance as f64) / (total_bits as f64)
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(self.bits.len() * 2);
        for b in &self.bits {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

/// Downsample an image loaded from PNG bytes to grayscale at given dimensions.
pub fn downsample_to_grayscale(
    png_bytes: &[u8],
    target_w: u32,
    target_h: u32,
) -> Result<GrayImage> {
    let img = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)
        .context("Failed to load PNG image from bytes")?;
    let gray = img.to_luma8();
    let resized = imageops::resize(
        &gray,
        target_w.max(1),
        target_h.max(1),
        imageops::FilterType::Triangle,
    );
    Ok(resized)
}

/// Compute difference hash (dHash) for an image.
///
/// For an N x N target, downsamples to (N + 1) x N grayscale, then compares
/// horizontally adjacent pixels: bit = 1 if pixel[x + 1, y] > pixel[x, y].
pub fn compute_dhash(png_bytes: &[u8], size: HashSize) -> Result<PerceptualHash> {
    let (n_w, n_h) = size.dimensions();
    let sample_w = n_w + 1;
    let sample_h = n_h;

    let gray = downsample_to_grayscale(png_bytes, sample_w, sample_h)?;
    let total_bits = (n_w * n_h) as usize;
    let byte_count = total_bits.div_ceil(8);
    let mut bits = vec![0u8; byte_count];

    let mut bit_idx = 0usize;
    for y in 0..n_h {
        for x in 0..n_w {
            let p_left = gray.get_pixel(x, y)[0];
            let p_right = gray.get_pixel(x + 1, y)[0];
            if p_right > p_left {
                let byte_i = bit_idx / 8;
                let bit_pos = 7 - (bit_idx % 8);
                bits[byte_i] |= 1 << bit_pos;
            }
            bit_idx += 1;
        }
    }

    Ok(PerceptualHash::new(bits, total_bits))
}

/// Compute average perceptual hash (pHash / aHash) for an image.
///
/// Downsamples to N x N grayscale, computes the mean intensity, and sets
/// bit = 1 if pixel intensity >= mean.
pub fn compute_phash(png_bytes: &[u8], size: HashSize) -> Result<PerceptualHash> {
    let (n_w, n_h) = size.dimensions();
    let gray = downsample_to_grayscale(png_bytes, n_w, n_h)?;

    let total_pixels = (n_w * n_h) as usize;
    let sum: u64 = gray.pixels().map(|p| p[0] as u64).sum();
    let mean = (sum / (total_pixels as u64).max(1)) as u8;

    let byte_count = total_pixels.div_ceil(8);
    let mut bits = vec![0u8; byte_count];

    for (i, p) in gray.pixels().enumerate() {
        if p[0] >= mean {
            let byte_i = i / 8;
            let bit_pos = 7 - (i % 8);
            bits[byte_i] |= 1 << bit_pos;
        }
    }

    Ok(PerceptualHash::new(bits, total_pixels))
}

/// Calculate mean absolute pixel difference between two PNG frames downsampled to N x N.
///
/// Returns a visual distance percentage between 0.0 (identical) and 1.0 (maximal difference).
pub fn calculate_pixel_difference(prev_png: &[u8], curr_png: &[u8], size: HashSize) -> Result<f64> {
    let (w, h) = size.dimensions();
    let g1 = downsample_to_grayscale(prev_png, w, h)?;
    let g2 = downsample_to_grayscale(curr_png, w, h)?;

    let total_cells = (w * h) as f64;
    let mut sum_diff = 0.0f64;
    for (p1, p2) in g1.pixels().zip(g2.pixels()) {
        let diff = (p1[0] as i32 - p2[0] as i32).abs() as f64;
        sum_diff += diff;
    }

    let normalized_diff = sum_diff / (255.0 * total_cells);
    Ok(normalized_diff.clamp(0.0, 1.0))
}

/// Calculate comprehensive visual distance combining pixel differences and dHash distance.
pub fn calculate_visual_distance(prev_png: &[u8], curr_png: &[u8]) -> Result<f64> {
    if prev_png == curr_png {
        return Ok(0.0);
    }
    let pixel_diff = calculate_pixel_difference(prev_png, curr_png, HashSize::Size16)?;
    let hash_diff = {
        let h1 = compute_dhash(prev_png, HashSize::Size16)?;
        let h2 = compute_dhash(curr_png, HashSize::Size16)?;
        h1.normalized_distance(&h2)
    };
    // Blend pixel difference with perceptual difference
    let blended = 0.7 * pixel_diff + 0.3 * hash_diff;
    Ok(blended.clamp(0.0, 1.0))
}

/// Crop a Region of Interest (ROI) from a PNG image and encode it back to PNG.
pub fn crop_roi(png_bytes: &[u8], roi: &Roi) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)
        .context("Failed to load PNG for cropping")?;
    let (img_w, img_h) = img.dimensions();

    let clamped = match roi.clamp_to_bounds(img_w, img_h) {
        Some(c) => c,
        None => bail!("Cannot crop ROI on empty image (0x0)"),
    };

    let cropped: DynamicImage = img.crop_imm(clamped.x, clamped.y, clamped.width, clamped.height);
    let mut out_bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(Cursor::new(&mut out_bytes));
    encoder
        .write_image(
            cropped.as_bytes(),
            cropped.width(),
            cropped.height(),
            cropped.color().into(),
        )
        .context("Failed to encode cropped image to PNG")?;

    Ok(out_bytes)
}

/// Check whether an action description, kind, or key qualifies as a wait or scroll operation.
pub fn is_wait_or_scroll(kind: &str, key: Option<&str>, description: &str) -> bool {
    let k = kind.trim().to_lowercase();
    if k == "wait" || k == "scroll" || k == "sleep" {
        return true;
    }

    if k == "key" {
        let key_lower = key.unwrap_or_default().to_lowercase();
        if key_lower.contains("page")
            || key_lower.contains("down")
            || key_lower.contains("up")
            || key_lower.contains("scroll")
            || key_lower == "space"
        {
            return true;
        }
    }

    let desc_lower = description.to_lowercase();
    desc_lower.contains("wait")
        || desc_lower.contains("scroll")
        || desc_lower.contains("settle")
        || desc_lower.contains("loading")
        || desc_lower.contains("sleep")
}

/// Configuration for the perceptual change gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeGateConfig {
    /// Minimum visual change threshold (e.g., 0.01 for 1% change) to qualify as changed.
    pub min_change_threshold: f64,
    /// Maximum consecutive unchanged ticks before forcing a VLM invocation.
    pub max_unchanged_ticks: usize,
    /// Hash resolution used for change gate evaluation.
    pub hash_size: HashSize,
    /// Duration in milliseconds to back off / wait when skipping a VLM tick.
    pub backoff_ms: u64,
}

impl Default for ChangeGateConfig {
    fn default() -> Self {
        Self {
            min_change_threshold: 0.01, // 1% change
            max_unchanged_ticks: 3,
            hash_size: HashSize::Size16,
            backoff_ms: 750, // 750ms backoff
        }
    }
}

/// Evaluation decision produced by the change gate for a screenshot frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDecision {
    /// Whether the full VLM invocation should be skipped.
    pub should_skip_vlm: bool,
    /// Measured visual change percentage (0.0 to 1.0).
    pub visual_distance: f64,
    /// Current count of consecutive unchanged ticks.
    pub unchanged_ticks: usize,
    /// Human-readable explanation of the gating decision.
    pub reason: String,
    /// Suggested backoff delay in milliseconds.
    pub backoff_ms: u64,
}

/// Metrics tracked by the change gate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeGateMetrics {
    pub total_frames_evaluated: usize,
    pub total_vlm_calls: usize,
    pub skipped_vlm_ticks: usize,
    pub tokens_saved: u64,
    pub cost_saved_usd: f64,
    pub cache_hit_rate: f64,
}

/// State machine managing perceptual hash gating across vision loop ticks.
pub struct ChangeGate {
    pub config: ChangeGateConfig,
    previous_frame: Option<Vec<u8>>,
    previous_hash: Option<PerceptualHash>,
    unchanged_ticks: usize,
    skipped_vlm_ticks: usize,
    total_vlm_calls: usize,
    total_frames_evaluated: usize,
}

impl ChangeGate {
    pub fn new(config: ChangeGateConfig) -> Self {
        Self {
            config,
            previous_frame: None,
            previous_hash: None,
            unchanged_ticks: 0,
            skipped_vlm_ticks: 0,
            total_vlm_calls: 0,
            total_frames_evaluated: 0,
        }
    }

    /// Evaluate current frame against previous frame.
    ///
    /// Rules:
    /// - If frame change is below `min_change_threshold` (< 1%) AND previous action was a wait/scroll:
    ///   - If `unchanged_ticks < max_unchanged_ticks`:
    ///     * Return `should_skip_vlm = true`, increment `skipped_vlm_ticks` and `unchanged_ticks`.
    ///   - If `unchanged_ticks >= max_unchanged_ticks`:
    ///     * Force VLM invocation, reset `unchanged_ticks = 0`.
    /// - If frame has changed >= `min_change_threshold` or previous action was not wait/scroll:
    ///   * Invoke VLM, reset `unchanged_ticks = 0`.
    pub fn evaluate(
        &mut self,
        current_frame_png: &[u8],
        last_action_was_wait_or_scroll: bool,
    ) -> Result<GateDecision> {
        self.total_frames_evaluated += 1;

        let curr_hash = compute_dhash(current_frame_png, self.config.hash_size)?;

        let (distance, is_first_frame) = match &self.previous_frame {
            Some(prev_frame) => {
                let d = calculate_pixel_difference(
                    prev_frame,
                    current_frame_png,
                    self.config.hash_size,
                )?;
                (d, false)
            }
            None => (1.0, true),
        };

        // Update stored frame and hash
        self.previous_frame = Some(current_frame_png.to_vec());
        self.previous_hash = Some(curr_hash);

        if is_first_frame {
            self.unchanged_ticks = 0;
            self.total_vlm_calls += 1;
            return Ok(GateDecision {
                should_skip_vlm: false,
                visual_distance: 1.0,
                unchanged_ticks: 0,
                reason: "Initial frame; invoking VLM".into(),
                backoff_ms: 0,
            });
        }

        let is_subthreshold = distance < self.config.min_change_threshold;

        if is_subthreshold && last_action_was_wait_or_scroll {
            if self.unchanged_ticks < self.config.max_unchanged_ticks {
                self.unchanged_ticks += 1;
                self.skipped_vlm_ticks += 1;
                return Ok(GateDecision {
                    should_skip_vlm: true,
                    visual_distance: distance,
                    unchanged_ticks: self.unchanged_ticks,
                    reason: format!(
                        "Visual change {:.3}% below threshold ({:.1}%) after wait/scroll; skipping VLM ({}/{} ticks)",
                        distance * 100.0,
                        self.config.min_change_threshold * 100.0,
                        self.unchanged_ticks,
                        self.config.max_unchanged_ticks
                    ),
                    backoff_ms: self.config.backoff_ms,
                });
            } else {
                // Max unchanged ticks reached, force VLM call
                self.unchanged_ticks = 0;
                self.total_vlm_calls += 1;
                return Ok(GateDecision {
                    should_skip_vlm: false,
                    visual_distance: distance,
                    unchanged_ticks: 0,
                    reason: format!(
                        "Maximum unchanged ticks ({}) reached; forcing VLM invocation",
                        self.config.max_unchanged_ticks
                    ),
                    backoff_ms: 0,
                });
            }
        }

        // Frame changed or action was not wait/scroll -> reset ticks and invoke VLM
        self.unchanged_ticks = 0;
        self.total_vlm_calls += 1;
        let reason = if !is_subthreshold {
            format!(
                "Frame changed by {:.3}% (threshold {:.1}%); invoking VLM",
                distance * 100.0,
                self.config.min_change_threshold * 100.0
            )
        } else {
            "Action was not wait/scroll; invoking VLM".into()
        };

        Ok(GateDecision {
            should_skip_vlm: false,
            visual_distance: distance,
            unchanged_ticks: 0,
            reason,
            backoff_ms: 0,
        })
    }

    /// Explicitly record a VLM invocation when called externally.
    pub fn record_vlm_invocation(&mut self) {
        self.total_vlm_calls += 1;
    }

    /// Retrieve summary audit metrics.
    pub fn metrics(&self) -> ChangeGateMetrics {
        let total_ticks = self.total_vlm_calls + self.skipped_vlm_ticks;
        let cache_hit_rate = if total_ticks > 0 {
            (self.skipped_vlm_ticks as f64) / (total_ticks as f64)
        } else {
            0.0
        };
        ChangeGateMetrics {
            total_frames_evaluated: self.total_frames_evaluated,
            total_vlm_calls: self.total_vlm_calls,
            skipped_vlm_ticks: self.skipped_vlm_ticks,
            tokens_saved: (self.skipped_vlm_ticks as u64) * ESTIMATED_TOKENS_PER_VLM_CALL,
            cost_saved_usd: (self.skipped_vlm_ticks as f64) * ESTIMATED_COST_PER_VLM_CALL_USD,
            cache_hit_rate,
        }
    }

    /// Reset gate state.
    pub fn reset(&mut self) {
        self.previous_frame = None;
        self.previous_hash = None;
        self.unchanged_ticks = 0;
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn make_test_png(w: u32, h: u32, color: Rgb<u8>) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = color;
        }
        let mut bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(Cursor::new(&mut bytes));
        encoder
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    fn make_gradient_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(Cursor::new(&mut bytes));
        encoder
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    #[test]
    fn test_identical_frames_have_zero_distance() {
        let png1 = make_test_png(64, 64, Rgb([100, 100, 100]));
        let png2 = make_test_png(64, 64, Rgb([100, 100, 100]));

        let diff = calculate_pixel_difference(&png1, &png2, HashSize::Size16).unwrap();
        assert_eq!(diff, 0.0);

        let visual_dist = calculate_visual_distance(&png1, &png2).unwrap();
        assert_eq!(visual_dist, 0.0);

        let h1 = compute_dhash(&png1, HashSize::Size8).unwrap();
        let h2 = compute_dhash(&png2, HashSize::Size8).unwrap();
        assert_eq!(h1.hamming_distance(&h2), 0);
        assert_eq!(h1.normalized_distance(&h2), 0.0);
    }

    #[test]
    fn test_different_frames_have_positive_distance() {
        let black = make_test_png(64, 64, Rgb([0, 0, 0]));
        let white = make_test_png(64, 64, Rgb([255, 255, 255]));

        let diff = calculate_pixel_difference(&black, &white, HashSize::Size16).unwrap();
        assert!((diff - 1.0).abs() < 1e-4);

        let grad = make_gradient_png(64, 64);
        let grad_diff = calculate_pixel_difference(&black, &grad, HashSize::Size16).unwrap();
        assert!(grad_diff > 0.1 && grad_diff < 0.9);
    }

    #[test]
    fn test_dhash_and_phash_computation() {
        let grad = make_gradient_png(64, 64);

        let dhash8 = compute_dhash(&grad, HashSize::Size8).unwrap();
        assert_eq!(dhash8.bit_count, 64);
        assert_eq!(dhash8.bits.len(), 8);
        assert!(dhash8.as_u64().is_some());

        let dhash16 = compute_dhash(&grad, HashSize::Size16).unwrap();
        assert_eq!(dhash16.bit_count, 256);
        assert_eq!(dhash16.bits.len(), 32);

        let phash = compute_phash(&grad, HashSize::Size8).unwrap();
        assert_eq!(phash.bit_count, 64);
        assert_eq!(phash.bits.len(), 8);
    }

    #[test]
    fn test_roi_cropping_and_bounds_clamping() {
        let grad = make_gradient_png(100, 100);

        // Crop center 40x40
        let roi = Roi::new(30, 30, 40, 40);
        let cropped_bytes = crop_roi(&grad, &roi).unwrap();
        let cropped_img = image::load_from_memory(&cropped_bytes).unwrap();
        assert_eq!(cropped_img.dimensions(), (40, 40));

        // Crop with out-of-bounds coordinates (clamped safely)
        let out_roi = Roi::new(90, 90, 50, 50);
        let cropped_clamped = crop_roi(&grad, &out_roi).unwrap();
        let clamped_img = image::load_from_memory(&cropped_clamped).unwrap();
        assert_eq!(clamped_img.dimensions(), (10, 10));
    }

    #[test]
    fn test_change_gate_skips_vlm_on_wait_and_subthreshold() {
        let mut gate = ChangeGate::new(ChangeGateConfig {
            min_change_threshold: 0.01,
            max_unchanged_ticks: 3,
            hash_size: HashSize::Size16,
            backoff_ms: 500,
        });

        let frame1 = make_test_png(64, 64, Rgb([50, 50, 50]));
        let frame2 = make_test_png(64, 64, Rgb([50, 50, 50])); // Identical frame

        // First frame always invokes VLM
        let dec1 = gate.evaluate(&frame1, false).unwrap();
        assert!(!dec1.should_skip_vlm);
        assert_eq!(dec1.unchanged_ticks, 0);

        // Frame 2: identical and previous action was wait -> should skip VLM
        let dec2 = gate.evaluate(&frame2, true).unwrap();
        assert!(dec2.should_skip_vlm);
        assert_eq!(dec2.unchanged_ticks, 1);
        assert_eq!(dec2.backoff_ms, 500);

        // Frame 3: identical and previous action was scroll -> should skip VLM
        let dec3 = gate.evaluate(&frame2, true).unwrap();
        assert!(dec3.should_skip_vlm);
        assert_eq!(dec3.unchanged_ticks, 2);

        // Frame 4: 3rd unchanged tick -> should skip VLM (reaches max=3)
        let dec4 = gate.evaluate(&frame2, true).unwrap();
        assert!(dec4.should_skip_vlm);
        assert_eq!(dec4.unchanged_ticks, 3);

        // Frame 5: 4th tick exceeds max_unchanged_ticks (3) -> should force VLM invocation
        let dec5 = gate.evaluate(&frame2, true).unwrap();
        assert!(!dec5.should_skip_vlm);
        assert_eq!(dec5.unchanged_ticks, 0);
        assert!(dec5.reason.contains("Maximum unchanged ticks"));

        let metrics = gate.metrics();
        assert_eq!(metrics.skipped_vlm_ticks, 3);
        assert_eq!(metrics.total_vlm_calls, 2);
        assert_eq!(metrics.tokens_saved, 3 * ESTIMATED_TOKENS_PER_VLM_CALL);
        assert!((metrics.cost_saved_usd - 3.0 * ESTIMATED_COST_PER_VLM_CALL_USD).abs() < 1e-6);
    }

    #[test]
    fn test_change_gate_invokes_vlm_on_visual_change() {
        let mut gate = ChangeGate::new(ChangeGateConfig::default());

        let frame1 = make_test_png(64, 64, Rgb([10, 10, 10]));
        let frame2 = make_test_png(64, 64, Rgb([200, 200, 200])); // Major visual change

        let _ = gate.evaluate(&frame1, false).unwrap();
        // Even if previous action was wait, frame changed significantly
        let dec2 = gate.evaluate(&frame2, true).unwrap();
        assert!(!dec2.should_skip_vlm);
        assert_eq!(dec2.unchanged_ticks, 0);
        assert!(dec2.visual_distance > 0.5);
    }

    #[test]
    fn test_change_gate_invokes_vlm_if_action_not_wait_or_scroll() {
        let mut gate = ChangeGate::new(ChangeGateConfig::default());

        let frame1 = make_test_png(64, 64, Rgb([50, 50, 50]));
        let frame2 = make_test_png(64, 64, Rgb([50, 50, 50]));

        let _ = gate.evaluate(&frame1, false).unwrap();
        // Frame identical, but previous action was click/type (not wait/scroll)
        let dec2 = gate.evaluate(&frame2, false).unwrap();
        assert!(!dec2.should_skip_vlm);
        assert_eq!(dec2.unchanged_ticks, 0);
    }

    #[test]
    fn test_is_wait_or_scroll_helper() {
        assert!(is_wait_or_scroll("wait", None, "Waiting for animation"));
        assert!(is_wait_or_scroll("scroll", None, "Scroll down"));
        assert!(is_wait_or_scroll("key", Some("Page_Down"), "Page down"));
        assert!(is_wait_or_scroll("key", Some("Down"), "Arrow down"));
        assert!(is_wait_or_scroll("click", None, "Wait for page to settle"));
        assert!(!is_wait_or_scroll("click", None, "Click submit button"));
        assert!(!is_wait_or_scroll("type", None, "Type user password"));
    }
}
