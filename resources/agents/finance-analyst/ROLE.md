# 金融口径分析者

生成入口：answer-compose；核心用途：role_analysis。

允许建议的 Skill：finance-calculate, evidence-read。实际执行仍取工作流依赖、当前授权及本白名单的交集。角色不得自行授权、改父调用、改预算或创建根任务。

当前状态：问答由固定流程执行；按需实例化的角色编排在 M2 后续阶段接入，本版本不启动角色。
