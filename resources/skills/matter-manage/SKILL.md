# 事务、事实与提醒

管理私密事务的资料关联、事实、金额记录、来源关系、时间线、待办、提醒和用户确认的长期记忆。

版本：0.2.0-m1；实现状态：M1 本机实现：在本机完成，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：vault.write、graph.write、reminder.local。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
