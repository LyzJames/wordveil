# 生成接口配置与能力自检

由用户配置生成/MCP连接，保存密钥并合成自检；按模态选择OCR/ASR兜底或获准原生媒体，分析入口不因缺模态被禁用。

版本：0.2.0-m1；实现状态：合同已注册；联网与外部连接能力在 M2 提供。M1 调用返回 needs_authorization，不发起任何网络请求。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：settings.connection；条件能力（只在用户明确确认或受保护流程的根任务中授予）：credential.write、network.probe。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：无。
