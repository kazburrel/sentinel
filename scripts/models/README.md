# Local face models

FRIDAY uses these official OpenCV Zoo models entirely on the local Mac:

- `face_detection_yunet_2023mar.onnx` — YuNet face detection and five-point landmarks.
- `face_recognition_sface_2021dec.onnx` — SFace aligned-face embeddings.

Both model directories are distributed under the Apache License 2.0 by the
[OpenCV Zoo](https://github.com/opencv/opencv_zoo). No camera image or face
embedding is sent to OpenCV or any other external service.

SHA-256 checksums:

```text
8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4  face_detection_yunet_2023mar.onnx
0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79  face_recognition_sface_2021dec.onnx
```
