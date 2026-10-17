# 合同与金融条款阅读

阅读合同、保险或金融产品说明，定位具体条款、解释费用与风险并整理需要核实或协商的问题。

版本：0.1.0-m0；当前为 M0 合同原型，业务 Handler 在后续阶段实现。

所有调用经可信 SkillRuntime；未知字段、未登记或越界 Ref 均拒绝。权限、身份、生成模式、预算和批准凭证由核心提供。

输入：[严格 Schema](input.schema.json)；输出：[结果封装](output.schema.json)；[清单](manifest.json)。

直接子调用：document-import, lvjin-retrieval, matter-manage, report-export, image-review, audio-review, video-review。
