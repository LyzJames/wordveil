# 合同与金融条款阅读

阅读合同、保险或金融产品说明，定位具体条款、解释费用与风险并整理需要核实或协商的问题。

版本：0.4.0-m2；实现状态：本机检索与确定性计算；连接并授权生成接口后，由固定的窄角色（法律适用、金融口径、证据检查、结果综合）形成有原文依据的书面分析并逐句核查。未授权时只交付本机证据与计算。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

直接能力：vault.read；条件能力（只在用户明确确认或受保护流程的根任务中授予）：无。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-import, lvjin-retrieval, matter-manage, report-export, image-review, audio-review, video-review。
