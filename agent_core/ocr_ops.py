"""Deterministic OCR geometry steps between the fixed Paddle models.

Page rotation uses exact quarter-turn transposition (no interpolation) so every
pixel maps back to the original page. Line crops follow the PaddleX 3.7.2
min-area-rectangle + perspective rule (CropByPolys, det_box_type="quad") and
report the four source corners of each crop in the rotated page frame.
"""
from __future__ import annotations

import base64
import io


def _decode(png_base64: str):
    from PIL import Image
    image = Image.open(io.BytesIO(base64.b64decode(png_base64, validate=True)))
    if image.width * image.height > 16_777_216:
        raise ValueError('image_pixel_budget')
    return image.convert('RGB')


def _encode(image) -> str:
    buffer = io.BytesIO()
    image.save(buffer, format='PNG')
    return base64.b64encode(buffer.getvalue()).decode('ascii')


def rotate_quarter(png_base64: str, clockwise_turns: int) -> dict:
    """Rotate the page clockwise by 0-3 quarter turns."""
    from PIL import Image
    if clockwise_turns not in (0, 1, 2, 3):
        raise ValueError('invalid_rotation')
    image = _decode(png_base64)
    # PIL's ROTATE_90 is counter-clockwise; one clockwise turn equals ROTATE_270.
    method = {1: Image.Transpose.ROTATE_270, 2: Image.Transpose.ROTATE_180, 3: Image.Transpose.ROTATE_90}.get(clockwise_turns)
    rotated = image.transpose(method) if method is not None else image
    return {'png_base64': _encode(rotated), 'width': rotated.width, 'height': rotated.height, 'clockwise_turns': clockwise_turns}


def doc_ori_clockwise_turns(label: str) -> int:
    """PaddleX rotate_image(img, angle) turns counter-clockwise by `angle`.

    The classifier label is that correction angle, so correcting a page labelled
    90 means rotating 90 degrees counter-clockwise, i.e. three clockwise turns.
    """
    angle = int(label)
    if angle not in (0, 90, 180, 270):
        raise ValueError('invalid_orientation_label')
    return (4 - angle // 90) % 4


def _ordered_minarea_points(points):
    import cv2
    import numpy as np
    box = cv2.minAreaRect(np.array(points).astype(np.int32))
    corners = sorted(list(cv2.boxPoints(box)), key=lambda p: p[0])
    if corners[1][1] > corners[0][1]:
        index_a, index_d = 0, 1
    else:
        index_a, index_d = 1, 0
    if corners[3][1] > corners[2][1]:
        index_b, index_c = 2, 3
    else:
        index_b, index_c = 3, 2
    return [corners[index_a], corners[index_b], corners[index_c], corners[index_d]]


def crop_polygons(png_base64: str, polygons: list) -> dict:
    """Perspective-rectified line crops plus their ordered source quadrilaterals."""
    import cv2
    import numpy as np
    from PIL import Image
    if len(polygons) > 2000:
        raise ValueError('too_many_text_regions')
    page = np.asarray(_decode(png_base64))
    crops = []
    for polygon in polygons:
        if not isinstance(polygon, list) or len(polygon) < 4:
            raise ValueError('invalid_polygon')
        quad = _ordered_minarea_points(polygon)
        points = np.array(quad, dtype=np.float32)
        width = int(max(np.linalg.norm(points[0] - points[1]), np.linalg.norm(points[2] - points[3])))
        height = int(max(np.linalg.norm(points[0] - points[3]), np.linalg.norm(points[1] - points[2])))
        if width < 1 or height < 1:
            crops.append(None)
            continue
        target = np.float32([[0, 0], [width, 0], [width, height], [0, height]])
        matrix = cv2.getPerspectiveTransform(points, target)
        crop = cv2.warpPerspective(page, matrix, (width, height), borderMode=cv2.BORDER_REPLICATE, flags=cv2.INTER_CUBIC)
        rotated_90 = crop.shape[0] * 1.0 / crop.shape[1] >= 1.5
        if rotated_90:
            crop = np.rot90(crop)
        crops.append({'png_base64': _encode(Image.fromarray(np.ascontiguousarray(crop))),
                      'quad': [[round(float(x), 2), round(float(y), 2)] for x, y in quad],
                      'rotated_90': bool(rotated_90)})
    return {'crops': crops}
