# 授权联网研究

在用户许可的查询、目的地和预算内搜索并读取公共网页，记录来源、时间、原文快照和证据位置。

版本：0.3.0-m2；实现状态：联网检索、公共知识更新与外部工具在后续阶段提供；当前返回 needs_authorization，不发起这类网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：network.search、network.fetch；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
