# 依据整理与答案生成

根据已登记的证据、计算结果和用户问题生成或修订答案，明确事实、前提、冲突和引用。

版本：0.2.0-m1；实现状态：合同已注册；需要用户配置并授权的生成接口。M1 调用返回 requires_generation，不伪造结果。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read、generation.request；条件能力（只在用户明确确认或受保护流程的根任务中授予）：generation.request。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
