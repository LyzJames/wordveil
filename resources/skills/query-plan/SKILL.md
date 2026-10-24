# 查询理解与多步策略

依据原始法律金融问题和已知事实生成有限搜索策略，列出缺失事实、精确词和来源范围。

版本：0.5.0-m4；实现状态：需要用户配置并授权的生成接口；只发送授权范围内的问题、原文摘录或获准的原生媒体（answer-grounding 在本机用 mmBERT 与确定性规则核查）。角色的规划、分析、检查与简报都经这些入口进入核心。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read、generation.request；条件能力（只在用户明确确认或受保护流程的根任务中授予）：generation.request。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
