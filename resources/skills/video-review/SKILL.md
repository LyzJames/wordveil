# 视频画面与音轨联合分析

按原时间轴抽帧OCR并转写视频音轨；有对应原生媒体能力且获准时读取实际帧/音频或视频，保留覆盖与分歧。

版本：0.1.0-m0；当前为 M0 合同原型，业务 Handler 在后续阶段实现。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：media-decode, image-review, audio-review, media-observe, knowledge-index, evidence-read, answer-compose, answer-grounding。
