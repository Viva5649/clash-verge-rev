# 修改细节

## 2025-08-26-启动时自动导入订阅

Clash Verge Rev 支持在应用启动时自动从指定的URL导入订阅配置。这个功能特别适用于：

- 企业环境下的统一配置分发
- 新设备首次启动时的自动配置
- 定期更新的订阅源自动导入

### 配置方法

在 `verge.yaml` 配置文件中添加以下配置：

```yaml
# 启用启动时自动导入订阅功能
enable_startup_import: true

# 要自动导入的订阅URL列表
startup_import_urls:
  - "https://example.com/subscription1"
  - "https://example.com/subscription2"
```

### 功能特性

- ✅ 支持多个订阅URL同时导入
- ✅ 自动跳过空白或无效的URL
- ✅ 防重复导入机制
- ✅ 自动重试机制（最多重试2次）
- ✅ 详细的日志记录
- ✅ 导入状态通知
- ✅ 不阻塞应用启动流程

### 测试功能

可以通过前端调用 `test_startup_import` 命令来手动测试这个功能，无需重启应用。

---
