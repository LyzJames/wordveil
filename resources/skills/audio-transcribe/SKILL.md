# 本地音频文字转写

使用固定Qwen/Qwen3-ASR-0.6B-hf对纯音频或视频音轨分段转写，保留原文、回放区间与不完整状态。

版本：0.5.0-m4；实现状态：本机实现：在本机完成，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read、vault.write、local.asr；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
