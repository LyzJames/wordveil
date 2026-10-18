# 借款与还款分析

整理借条、转账和聊天材料中的借还款事实，计算情景并结合有依据的规则形成问题清单。

版本：0.3.0-m2；实现状态：本机识别、转写、检索与确定性计算已实现；生成分析与原生媒体复核在后续阶段接入，当前返回 requires_generation，不伪造结论。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-import, matter-manage, finance-calculate, lvjin-retrieval, report-export, image-review, audio-review, video-review。
