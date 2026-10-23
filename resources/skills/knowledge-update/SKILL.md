# 公共知识包导入与更新

验证并导入用户选定或已授权获取的法律金融公共知识，保存权利、来源、版本和更新差异。

版本：0.4.0-m3；实现状态：真实 Handler：DeepSeek Search/Fetch、公共知识包及只读 MCP。默认关闭；不可伪造的逐次确认与绑定凭证由核心校验。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：public_knowledge.write；条件能力（只在用户明确确认或受保护流程的根任务中授予）：network.update。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-import。
