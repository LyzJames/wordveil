# 音频转写与原音频复核

对纯音频或视频音轨使用固定ASR文字兜底；生成接口支持原音频且获准时再直接听音频核对转写与可支持的声音信息。

版本：0.4.0-m2；实现状态：本机 OCR 与转写；授权且接口通过对应原生能力自检时，直接读取图片（音频需已验证的音频输入能力）并与本机文字逐字段比较，再形成有依据的分析。原生视频与联合媒体不在本版本。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：media-decode, audio-transcribe, media-observe, knowledge-index, evidence-read, answer-compose, answer-grounding。
