# TODO

Repository work register for `bitty`. This file stays under 300 lines; completed
items move to git history rather than accumulating here.

## 2026-09-26 更新

### 今日完成
- [x] 修复 3 个 P1 issues PRs (等待 CI)
  - [x] PR #1453: PTY 关闭 + 剪贴板 (#1443, #1407)
  - [x] PR #1455: IME 输入修复 (#1449, #1439)
  - [x] PR #1457: 工作区切换 (#1446, #1435)
- [x] 修复 main 分支 CI 阻塞 (PR #1460)
- [x] 创建安全追踪 issue (#1461)

### 待处理 PRs (8)
- [ ] PR #1460: CI 修复 - 等待 merge 以解除其他 PRs 阻塞
- [ ] PR #1453: PTY 修复 - 等待 CI 通过
- [ ] PR #1455: IME 修复 - 等待 CI 通过
- [ ] PR #1457: 工作区修复 - 等待 CI 通过
- [ ] PR #1459: bitty-network 集成 - CONFLICTING
- [ ] PR #1456: Mod+Shift+digit 修复
- [ ] PR #1451: PTY/IPC 测试
- [ ] PR #1448: Panel shell reap

## P0/P1 Issues (17 待处理)

### 高优先级 - 测试基础设施 (P1)
- [ ] #1408: Parser-to-state fuzz + 不变量证据
  - 添加有界的 parser-to-state fuzz target
  - 检查状态不变量、resize/reset、replies
  - 规范哈希和重现证据
  
- [ ] #1406: 宽字符 reflow 时保持网格不变量
  - 宽度为 1 时的有界表示
  - 保持行长度和光标映射
  - scrollback/resize 转换测试

### 高优先级 - 运行时稳定 (P1)
- [ ] #1410: 插件 suspend/resume 事务化
  - 使 suspend/resume 转换成为事务
  - 确保状态一致性
  
- [ ] #1409: Atlas 规划保持帧一致性
  - 驱逐时保持 atlas 规划帧一致
  - 避免渲染不一致

### 高优先级 - 安全边界 (P1)
- [ ] #1404: 强制终端能力交集和端点预算
  - 能力交集检查
  - 端点资源预算
  - 严格执行
  
- [ ] #1403: 控制绑定到连接 principal/session/consent
  - 控制绑定到认证的 principal
  - 会话跟踪
  - 同意机制

### 高优先级 - 渲染问题 (P1)
- [ ] #1436: Fastfetch 连续输出重叠
  - 调查光标位置/滚动区域
  - 确保输出正确分离
  - 添加回归测试
  
- [ ] #1433: 文本选择跨越多个窄面板
  - 限制选择在单个面板内
  - 修复鼠标拖拽边界

### 高优先级 - 配置 (P1)
- [ ] #1397: Live config reload 未连接
  - Live class 缺少 watcher/apply 路径
  - 实现配置热加载

### 高优先级 - 安全追踪 (P1)
- [ ] #1461: CodeQL 敏感信息日志告警
  - 审查 SecretStore Debug 实现
  - 确保 secrets 从不明文记录
  - 使用编辑后的 Debug 表示

### P0 EPIC - 核心权限 (需要分解)
- [ ] #1401: 核心权限、终端安全和运行时完整性
  - 大型 EPIC，包含多个子任务
  - 需要分解为可执行的子 issues
  - 涉及权限、安全、运行时完整性

## 最近完成

### 2026-09-26
- [x] Windows CI 修复 (commit 11b8fd9)
- [x] Kitty graphics 调查 (#1434 - 协议已实现)
- [x] 内存优化调查追踪 (#1458)
- [x] Main 分支 CI 阻塞诊断和修复

### 2026-09-25
- [x] OSC 8 URI 分号保留 (#1450)
- [x] IPC peer 身份验证 (#1428)
- [x] R2 发布镜像 (#1430)
- [x] Kitty APC 限制 (#1405, #1427)
- [x] ED 22 scroll-and-clear (#1396, #1426)
- [x] 视口不在按键释放时跳转 (#1394, #1425)

## 架构和规划

### 测试策略
1. Parser-to-state fuzz (#1408) - 基础设施优先
2. Grid invariants (#1406) - 与 fuzz 配合
3. 现有测试继续保持

### 安全强化
1. 能力交集 (#1404)
2. Principal 绑定 (#1403)
3. CodeQL 告警 (#1461)
4. P0 EPIC 分解 (#1401)

### 渲染修复
1. Fastfetch 重叠 (#1436) - 需要实际测试
2. 选择跨面板 (#1433) - UI 测试

### 运行时稳定
1. Plugin suspend/resume (#1410)
2. Atlas planning (#1409)

## 依赖和阻塞

### 外部依赖
- bitty-network: 路径依赖已在 PR #1460 中修复
- bitty-terminal-docs: 规范契约 (#123, #122)
- bitty-devtools: 大型重构 (#138, #137)

### 内部依赖
- PR #1460 必须先 merge，才能解除其他 PRs 阻塞
- 测试基础设施 (#1408, #1406) 为其他工作提供支持

## 下一步行动

### 立即 (等待)
1. 监控 PR #1460 CI 完成
2. Merge PR #1460
3. 验证其他 PRs CI 自动通过
4. Merge 其他 PRs

### 短期 (本周)
1. 实现 #1408 (Parser-to-state fuzz)
2. 实现 #1406 (Grid invariants)
3. 分解 #1401 (P0 EPIC)

### 中期 (2-4 周)
1. 运行时稳定 (#1410, #1409)
2. 安全边界 (#1404, #1403)
3. 渲染修复 (#1436, #1433)
4. 配置热加载 (#1397)

### 长期 (v0.1.0 前)
1. 完成所有 P1 issues
2. 处理 P0 EPIC
3. 安全审查
4. 性能优化

## 进度跟踪

### P0/P1 完成度
- 总计: 17 issues
- 已实现: 6 (等待 merge)
- 待实现: 10
- EPIC: 1 (需要分解)
- 进度: 46% 实现 (31% 官方完成)

### 质量指标
- 本地测试: ✅ 全部通过
- CI 状态: ⏳ 等待 #1460 修复
- CodeQL: 1 告警追踪中
- 覆盖率: 持续监控

## 备注

- 所有剩余 P1 issues 都是架构级改动，需要深入时间
- 测试基础设施优先，为其他工作提供支持
- 安全问题有专门的追踪和修复流程
- 文档同步是定义完成的一部分

---

最后更新: 2026-09-26
维护者: OpenCode AI
