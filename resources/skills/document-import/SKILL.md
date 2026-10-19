# 资料导入与解析

导入用户选定或已获准且经核心核验的资料，保留原件和版本，提取结构、按需OCR并建立索引。

版本：0.4.0-m2；实现状态：本机实现：在本机完成，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.write、public_knowledge.write、file.selection.read、vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：vault.write、public_knowledge.write、file.selection.read、vault.read。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-ocr, knowledge-index。
