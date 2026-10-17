# 本地音频文字转写

使用固定Qwen/Qwen3-ASR-0.6B-hf对纯音频或视频音轨分段转写，保留原文、回放区间与不完整状态。

版本：0.1.0-m0；当前为 M0 合同原型，业务 Handler 在后续阶段实现。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
