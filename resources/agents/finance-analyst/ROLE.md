# 金融口径分析者

生成入口：answer-compose；核心用途：role_analysis。

允许建议的 Skill：finance-calculate, evidence-read。实际执行仍取工作流依赖、当前授权及本白名单的交集。角色不得自行授权、改父调用、改预算或创建根任务。

M1 状态：角色依赖生成接口，M1 不启动角色；M2 在用户配置并授权生成接口后提供。
