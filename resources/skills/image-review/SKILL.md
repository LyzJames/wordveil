# 图片文字与视觉交叉检查

对图片/扫描页保留OCR文字兜底；生成模型支持视觉且获准时必须读取真实图片，再比较关键字段和场景观察。

版本：0.1.0-m0；当前为 M0 合同原型，业务 Handler 在后续阶段实现。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-ocr, media-observe, evidence-read, answer-compose, answer-grounding。
