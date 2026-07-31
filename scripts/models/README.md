# Local vision models

FRIDAY loads these ONNX models directly in the Rust server through the
pure-Rust `tract` inference engine:

- `face_detection_yunet_2023mar.onnx` — YuNet face detection and five-point landmarks.
- `face_recognition_sface_2021dec.onnx` — SFace aligned-face embeddings.
- `yolo11n.onnx` — YOLO11 object/person detection for locked videos.

The face models are distributed under the Apache License 2.0 by the
[OpenCV Zoo](https://github.com/opencv/opencv_zoo). YOLO11 is provided by
Ultralytics under AGPL-3.0 or Enterprise terms. No camera image or face
embedding leaves the Mac.

SHA-256 checksums:

```text
8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4  face_detection_yunet_2023mar.onnx
0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79  face_recognition_sface_2021dec.onnx
1e091cf6511f2a795098ae7be608f5d97a238f07cf96b6e9b93ad4a359b1ea1d  yolo11n.onnx
```
