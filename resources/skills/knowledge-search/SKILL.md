# 本地混合检索

在当前私密空间、事务及公共知识范围内执行 BM25 与向量混合召回，融合排名并返回来源候选。

版本：0.4.0-m3；实现状态：本机实现：在本机完成，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read、index.read、local.embedding；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
