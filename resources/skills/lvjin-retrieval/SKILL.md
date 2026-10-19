# 法律金融检索与问答

为个人法律金融问题组织四阶段证据流程，按范围调用本地/获准公共检索、重排、生成与有限核查修订。

版本：0.4.0-m2；实现状态：本机检索、重排与原文定位；连接并授权生成接口后，规划检索、生成回答并逐句核查（最多两次修订）。未授权时只交付原文依据。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：query-plan, knowledge-search, graph-relations, evidence-rerank, evidence-read, web-research, finance-calculate, answer-compose, answer-grounding。
