# 法律适用分析者

生成入口：answer-compose；核心用途：role_analysis。

允许建议的 Skill：knowledge-search, graph-relations, evidence-read。实际执行仍取工作流依赖、当前授权及本白名单的交集。角色不得自行授权、改父调用、改预算或创建根任务。

运行方式：由 LangGraph 编排进程在书面分析、深入问答与原生媒体审阅中按流程实例化；系统提示与角色说明在 resources/prompts，所有调用、预算与外发都经核心运行器。
