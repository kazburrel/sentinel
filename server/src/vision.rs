//! Pure-Rust local vision inference.
//!
//! ONNX models run through `tract`; JPEG decoding, preprocessing,
//! postprocessing, face alignment, matching, and tracking inputs are all
//! implemented in Rust. No interpreter or sidecar process is involved.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use image::{DynamicImage, GenericImageView, RgbImage, imageops::FilterType};
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

const MODEL_SIZE: usize = 640;
const FACE_NMS_THRESHOLD: f32 = 0.30;
const OBJECT_NMS_THRESHOLD: f32 = 0.45;

type RunnableModel = TypedRunnableModel;

static FACE_DETECTOR: OnceLock<Result<Arc<RunnableModel>, String>> = OnceLock::new();
static FACE_RECOGNIZER: OnceLock<Result<Arc<RunnableModel>, String>> = OnceLock::new();
static OBJECT_DETECTOR: OnceLock<Result<Arc<RunnableModel>, String>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Rect {
    pub fn width(self) -> f32 {
        (self.x2 - self.x1).max(0.0)
    }

    pub fn height(self) -> f32 {
        (self.y2 - self.y1).max(0.0)
    }

    pub fn area(self) -> f32 {
        self.width() * self.height()
    }

    pub fn center(self) -> (f32, f32) {
        ((self.x1 + self.x2) * 0.5, (self.y1 + self.y2) * 0.5)
    }

    pub fn clamp(self, width: u32, height: u32) -> Self {
        let max_x = width.saturating_sub(1) as f32;
        let max_y = height.saturating_sub(1) as f32;
        Self {
            x1: self.x1.clamp(0.0, max_x),
            y1: self.y1.clamp(0.0, max_y),
            x2: self.x2.clamp(0.0, max_x),
            y2: self.y2.clamp(0.0, max_y),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FaceDetection {
    pub bounds: Rect,
    pub landmarks: [[f32; 2]; 5],
    pub confidence: f32,
}

pub struct BestFace {
    pub image: DynamicImage,
    pub detection: FaceDetection,
    pub face_count: usize,
    pub rotation: u16,
    pub quality: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Person,
    Knife,
    ConcerningObject,
    Package,
    Vehicle,
    Animal,
}

#[derive(Debug, Clone)]
pub struct ObjectDetection {
    pub bounds: Rect,
    pub kind: ObjectKind,
    pub label: &'static str,
    pub confidence: f32,
}

pub fn model_path(file_name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .join("scripts")
        .join("models")
        .join(file_name)
}

pub fn model_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn load_model(path: &Path) -> Result<Arc<RunnableModel>, String> {
    let model = tract_onnx::onnx()
        .model_for_path(path)
        .and_then(|model| model.into_optimized())
        .and_then(|model| model.into_runnable())
        .map_err(|error| format!("load ONNX model {}: {error}", path.display()))?;
    Ok(model)
}

fn face_detector() -> Result<&'static Arc<RunnableModel>, String> {
    FACE_DETECTOR
        .get_or_init(|| load_model(&model_path("face_detection_yunet_2023mar.onnx")))
        .as_ref()
        .map_err(Clone::clone)
}

fn face_recognizer() -> Result<&'static Arc<RunnableModel>, String> {
    FACE_RECOGNIZER
        .get_or_init(|| load_model(&model_path("face_recognition_sface_2021dec.onnx")))
        .as_ref()
        .map_err(Clone::clone)
}

fn object_detector() -> Result<&'static Arc<RunnableModel>, String> {
    OBJECT_DETECTOR
        .get_or_init(|| load_model(&model_path("yolo11n.onnx")))
        .as_ref()
        .map_err(Clone::clone)
}

fn resized_tensor(image: &DynamicImage, size: usize, bgr: bool, scale: f32) -> Tensor {
    let resized = image
        .resize_exact(size as u32, size as u32, FilterType::Triangle)
        .to_rgb8();
    let mut input = tract_ndarray::Array4::<f32>::zeros((1, 3, size, size));
    for (x, y, pixel) in resized.enumerate_pixels() {
        let channels = pixel.0;
        let values = if bgr {
            [channels[2], channels[1], channels[0]]
        } else {
            channels
        };
        for channel in 0..3 {
            input[[0, channel, y as usize, x as usize]] = values[channel] as f32 * scale;
        }
    }
    input.into()
}

pub fn detect_faces(image: &DynamicImage, threshold: f32) -> Result<Vec<FaceDetection>, String> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let input = resized_tensor(image, MODEL_SIZE, true, 1.0);
    let outputs = face_detector()?
        .run(tvec!(input.into()))
        .map_err(|error| format!("run YuNet: {error}"))?;

    let scale_x = width as f32 / MODEL_SIZE as f32;
    let scale_y = height as f32 / MODEL_SIZE as f32;
    let strides = [8.0f32, 16.0, 32.0];
    let grid_sizes = [80usize, 40, 20];
    let mut faces = Vec::new();
    for head in 0..3 {
        let cls = outputs[head]
            .to_plain_array_view::<f32>()
            .map_err(|error| format!("read YuNet class output: {error}"))?;
        let obj = outputs[head + 3]
            .to_plain_array_view::<f32>()
            .map_err(|error| format!("read YuNet object output: {error}"))?;
        let bbox = outputs[head + 6]
            .to_plain_array_view::<f32>()
            .map_err(|error| format!("read YuNet box output: {error}"))?;
        let kps = outputs[head + 9]
            .to_plain_array_view::<f32>()
            .map_err(|error| format!("read YuNet landmark output: {error}"))?;
        let grid = grid_sizes[head];
        let stride = strides[head];
        for row in 0..grid {
            for column in 0..grid {
                let index = row * grid + column;
                let score = (cls[[0, index, 0]].clamp(0.0, 1.0)
                    * obj[[0, index, 0]].clamp(0.0, 1.0))
                .sqrt();
                if score < threshold {
                    continue;
                }
                let center_x = (column as f32 + bbox[[0, index, 0]]) * stride;
                let center_y = (row as f32 + bbox[[0, index, 1]]) * stride;
                let box_width = bbox[[0, index, 2]].exp() * stride;
                let box_height = bbox[[0, index, 3]].exp() * stride;
                let bounds = Rect {
                    x1: (center_x - box_width * 0.5) * scale_x,
                    y1: (center_y - box_height * 0.5) * scale_y,
                    x2: (center_x + box_width * 0.5) * scale_x,
                    y2: (center_y + box_height * 0.5) * scale_y,
                }
                .clamp(width, height);
                if bounds.width() <= 1.0 || bounds.height() <= 1.0 {
                    continue;
                }
                let mut landmarks = [[0.0f32; 2]; 5];
                for (point, landmark) in landmarks.iter_mut().enumerate() {
                    landmark[0] = (kps[[0, index, point * 2]] + column as f32) * stride * scale_x;
                    landmark[1] = (kps[[0, index, point * 2 + 1]] + row as f32) * stride * scale_y;
                }
                faces.push(FaceDetection {
                    bounds,
                    landmarks,
                    confidence: score,
                });
            }
        }
    }
    faces.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
    let mut kept = Vec::new();
    for face in faces {
        if kept.iter().all(|existing: &FaceDetection| {
            intersection_over_union(face.bounds, existing.bounds) <= FACE_NMS_THRESHOLD
        }) {
            kept.push(face);
        }
        if kept.len() >= 32 {
            break;
        }
    }
    Ok(kept)
}

pub fn best_face_across_rotations(
    image: &DynamicImage,
    threshold: f32,
) -> Result<Option<BestFace>, String> {
    let mut best: Option<BestFace> = None;
    for rotation in [0u16, 90, 180, 270] {
        let rotated = rotate(image, rotation);
        let faces = detect_faces(&rotated, threshold)?;
        let face_count = faces.len();
        for face in faces {
            let image_area = (rotated.width() as f32 * rotated.height() as f32).max(1.0);
            let area_ratio = (face.bounds.area() / image_area).clamp(0.0, 1.0);
            let clipped = face.bounds.x1 <= 2.0
                || face.bounds.y1 <= 2.0
                || face.bounds.x2 >= rotated.width().saturating_sub(2) as f32
                || face.bounds.y2 >= rotated.height().saturating_sub(2) as f32;
            let quality =
                face.confidence * (1.0 + area_ratio * 8.0) * if clipped { 0.75 } else { 1.0 };
            if best
                .as_ref()
                .is_none_or(|candidate| quality > candidate.quality)
            {
                best = Some(BestFace {
                    image: rotated.clone(),
                    detection: face,
                    face_count,
                    rotation,
                    quality,
                });
            }
        }
    }
    Ok(best)
}

pub fn face_embedding(image: &DynamicImage, face: &FaceDetection) -> Result<Vec<f32>, String> {
    let aligned = align_face(image, &face.landmarks);
    let input = resized_tensor(&DynamicImage::ImageRgb8(aligned), 112, false, 1.0);
    let outputs = face_recognizer()?
        .run(tvec!(input.into()))
        .map_err(|error| format!("run SFace: {error}"))?;
    let output = outputs[0]
        .to_plain_array_view::<f32>()
        .map_err(|error| format!("read SFace output: {error}"))?;
    normalize_embedding(output.iter().copied().collect())
}

pub fn normalize_embedding(mut values: Vec<f32>) -> Result<Vec<f32>, String> {
    let magnitude = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if values.is_empty() || magnitude <= 1e-12 {
        return Err("empty face embedding".to_string());
    }
    for value in &mut values {
        *value /= magnitude;
    }
    Ok(values)
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32, String> {
    if left.is_empty() || left.len() != right.len() {
        return Err("face embeddings have incompatible dimensions".to_string());
    }
    Ok(left.iter().zip(right).map(|(a, b)| a * b).sum())
}

pub fn detect_objects(
    image: &DynamicImage,
    threshold: f32,
) -> Result<Vec<ObjectDetection>, String> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let scale = (MODEL_SIZE as f32 / width as f32).min(MODEL_SIZE as f32 / height as f32);
    let resized_width = (width as f32 * scale).round().max(1.0) as u32;
    let resized_height = (height as f32 * scale).round().max(1.0) as u32;
    let resized = image
        .resize_exact(resized_width, resized_height, FilterType::Triangle)
        .to_rgb8();
    let pad_x = (MODEL_SIZE as u32 - resized_width) / 2;
    let pad_y = (MODEL_SIZE as u32 - resized_height) / 2;
    let mut letterboxed = RgbImage::from_pixel(
        MODEL_SIZE as u32,
        MODEL_SIZE as u32,
        image::Rgb([114, 114, 114]),
    );
    image::imageops::replace(&mut letterboxed, &resized, pad_x as i64, pad_y as i64);
    let input = resized_tensor(
        &DynamicImage::ImageRgb8(letterboxed),
        MODEL_SIZE,
        false,
        1.0 / 255.0,
    );
    let outputs = object_detector()?
        .run(tvec!(input.into()))
        .map_err(|error| format!("run YOLO: {error}"))?;
    let output = outputs[0]
        .to_plain_array_view::<f32>()
        .map_err(|error| format!("read YOLO output: {error}"))?;

    let mut detections = Vec::new();
    for index in 0..output.shape()[2] {
        let mut best: Option<(usize, ObjectKind, &'static str, f32)> = None;
        for class_id in 0..80 {
            let Some((kind, label)) = relevant_coco_class(class_id) else {
                continue;
            };
            let confidence = output[[0, class_id + 4, index]];
            let class_threshold = match kind {
                ObjectKind::Person => threshold.max(0.10),
                ObjectKind::Knife | ObjectKind::ConcerningObject => threshold,
                ObjectKind::Package => threshold.max(0.20),
                ObjectKind::Vehicle | ObjectKind::Animal => threshold.max(0.30),
            };
            if confidence >= class_threshold
                && best.is_none_or(|candidate| confidence > candidate.3)
            {
                best = Some((class_id, kind, label, confidence));
            }
        }
        let Some((_class_id, kind, label, confidence)) = best else {
            continue;
        };
        let center_x = output[[0, 0, index]];
        let center_y = output[[0, 1, index]];
        let box_width = output[[0, 2, index]];
        let box_height = output[[0, 3, index]];
        let bounds = Rect {
            x1: (center_x - box_width * 0.5 - pad_x as f32) / scale,
            y1: (center_y - box_height * 0.5 - pad_y as f32) / scale,
            x2: (center_x + box_width * 0.5 - pad_x as f32) / scale,
            y2: (center_y + box_height * 0.5 - pad_y as f32) / scale,
        }
        .clamp(width, height);
        if bounds.width() > 1.0 && bounds.height() > 1.0 {
            detections.push(ObjectDetection {
                bounds,
                kind,
                label,
                confidence,
            });
        }
    }
    detections.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
    let mut kept = Vec::new();
    for detection in detections {
        if kept.iter().all(|existing: &ObjectDetection| {
            existing.kind != detection.kind
                || intersection_over_union(existing.bounds, detection.bounds)
                    <= OBJECT_NMS_THRESHOLD
        }) {
            kept.push(detection);
        }
        if kept.len() >= 64 {
            break;
        }
    }
    Ok(kept)
}

pub fn rotate(image: &DynamicImage, rotation: u16) -> DynamicImage {
    let rgb = image.to_rgb8();
    match rotation {
        90 => DynamicImage::ImageRgb8(image::imageops::rotate90(&rgb)),
        180 => DynamicImage::ImageRgb8(image::imageops::rotate180(&rgb)),
        270 => DynamicImage::ImageRgb8(image::imageops::rotate270(&rgb)),
        _ => DynamicImage::ImageRgb8(rgb),
    }
}

pub fn intersection_over_union(left: Rect, right: Rect) -> f32 {
    let x1 = left.x1.max(right.x1);
    let y1 = left.y1.max(right.y1);
    let x2 = left.x2.min(right.x2);
    let y2 = left.y2.min(right.y2);
    let intersection = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = left.area() + right.area() - intersection;
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn relevant_coco_class(class_id: usize) -> Option<(ObjectKind, &'static str)> {
    match class_id {
        0 => Some((ObjectKind::Person, "person")),
        1 => Some((ObjectKind::Vehicle, "bicycle")),
        2 => Some((ObjectKind::Vehicle, "car")),
        3 => Some((ObjectKind::Vehicle, "motorcycle")),
        5 => Some((ObjectKind::Vehicle, "bus")),
        7 => Some((ObjectKind::Vehicle, "truck")),
        14 => Some((ObjectKind::Animal, "bird")),
        15 => Some((ObjectKind::Animal, "cat")),
        16 => Some((ObjectKind::Animal, "dog")),
        17 => Some((ObjectKind::Animal, "horse")),
        18 => Some((ObjectKind::Animal, "sheep")),
        19 => Some((ObjectKind::Animal, "cow")),
        24 => Some((ObjectKind::Package, "backpack")),
        26 => Some((ObjectKind::Package, "handbag")),
        28 => Some((ObjectKind::Package, "suitcase")),
        34 => Some((ObjectKind::ConcerningObject, "baseball bat")),
        43 => Some((ObjectKind::Knife, "knife / sharp object")),
        76 => Some((ObjectKind::ConcerningObject, "scissors")),
        _ => None,
    }
}

fn align_face(image: &DynamicImage, landmarks: &[[f32; 2]; 5]) -> RgbImage {
    const DESTINATION: [[f32; 2]; 5] = [
        [38.2946, 51.6963],
        [73.5318, 51.5014],
        [56.0252, 71.7366],
        [41.5493, 92.3655],
        [70.7299, 92.2041],
    ];
    let source_mean = landmarks.iter().fold([0.0f32; 2], |mut mean, point| {
        mean[0] += point[0] / 5.0;
        mean[1] += point[1] / 5.0;
        mean
    });
    let destination_mean = DESTINATION.iter().fold([0.0f32; 2], |mut mean, point| {
        mean[0] += point[0] / 5.0;
        mean[1] += point[1] / 5.0;
        mean
    });
    let mut dot = 0.0f32;
    let mut cross = 0.0f32;
    let mut denominator = 0.0f32;
    for (source, destination) in landmarks.iter().zip(DESTINATION) {
        let sx = source[0] - source_mean[0];
        let sy = source[1] - source_mean[1];
        let dx = destination[0] - destination_mean[0];
        let dy = destination[1] - destination_mean[1];
        dot += sx * dx + sy * dy;
        cross += sx * dy - sy * dx;
        denominator += sx * sx + sy * sy;
    }
    let a = if denominator > 1e-6 {
        dot / denominator
    } else {
        1.0
    };
    let b = if denominator > 1e-6 {
        cross / denominator
    } else {
        0.0
    };
    let tx = destination_mean[0] - a * source_mean[0] + b * source_mean[1];
    let ty = destination_mean[1] - b * source_mean[0] - a * source_mean[1];
    let inverse_denominator = (a * a + b * b).max(1e-6);
    let source = image.to_rgb8();
    let mut output = RgbImage::new(112, 112);
    for y in 0..112 {
        for x in 0..112 {
            let dx = x as f32 - tx;
            let dy = y as f32 - ty;
            let source_x = (a * dx + b * dy) / inverse_denominator;
            let source_y = (-b * dx + a * dy) / inverse_denominator;
            output.put_pixel(x, y, bilinear_sample(&source, source_x, source_y));
        }
    }
    output
}

fn bilinear_sample(image: &RgbImage, x: f32, y: f32) -> image::Rgb<u8> {
    if x < 0.0
        || y < 0.0
        || x > image.width().saturating_sub(1) as f32
        || y > image.height().saturating_sub(1) as f32
    {
        return image::Rgb([0, 0, 0]);
    }
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(image.width() - 1);
    let y1 = (y0 + 1).min(image.height() - 1);
    let wx = x - x0 as f32;
    let wy = y - y0 as f32;
    let p00 = image.get_pixel(x0, y0).0;
    let p10 = image.get_pixel(x1, y0).0;
    let p01 = image.get_pixel(x0, y1).0;
    let p11 = image.get_pixel(x1, y1).0;
    let mut output = [0u8; 3];
    for channel in 0..3 {
        let top = p00[channel] as f32 * (1.0 - wx) + p10[channel] as f32 * wx;
        let bottom = p01[channel] as f32 * (1.0 - wx) + p11[channel] as f32 * wx;
        output[channel] = (top * (1.0 - wy) + bottom * wy).round().clamp(0.0, 255.0) as u8;
    }
    image::Rgb(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_handles_normalized_vectors() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0], &[1.0, 0.0]).is_err());
    }

    #[test]
    fn iou_is_zero_for_separate_boxes_and_one_for_equal_boxes() {
        let box_a = Rect {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        let box_b = Rect {
            x1: 20.0,
            y1: 20.0,
            x2: 30.0,
            y2: 30.0,
        };
        assert_eq!(intersection_over_union(box_a, box_b), 0.0);
        assert!((intersection_over_union(box_a, box_a) - 1.0).abs() < 1e-6);
    }
}
