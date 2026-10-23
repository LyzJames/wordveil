# 结果预览与导出

将当前结果、证据目录或计算表整理为 Markdown/CSV，先预览真实内容再导出到用户确认的位置。

版本：0.4.0-m3；实现状态：本机实现：在本机完成，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：file.export。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
