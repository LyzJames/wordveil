# 图片文字与视觉交叉检查

对图片/扫描页保留OCR文字兜底；生成模型支持视觉且获准时必须读取真实图片，再比较关键字段和场景观察。

版本：0.4.0-m2；实现状态：本机 OCR 与转写；授权且接口通过对应原生能力自检时，直接读取图片（音频需已验证的音频输入能力）并与本机文字逐字段比较，再形成有依据的分析。原生视频与联合媒体不在本版本。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-ocr, media-observe, evidence-read, answer-compose, answer-grounding。
