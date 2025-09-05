# update-clash-verge.ps1
param(
    [Parameter(Mandatory=$true)]
    [string]$DownloadUrl = "",
    [Parameter(Mandatory=$true)]
    [string]$ConfigUrl = ""
)

$InstallPath = "C:\Program Files\AIDC\ClashVergeRev"
$ExecutableName = "clash-verge.exe"
$TimeoutSeconds = 1800

# 设置错误处理
$ErrorActionPreference = "Stop"

# 日志函数
function Write-Log {
    param([string]$Message, [string]$Level = "INFO")
    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    Write-Host "[$timestamp] [$Level] $Message"
}

try {
    Write-Log "开始自动更新 Clash Verge Rev..."
    
    Write-Log "客户端下载地址: $DownloadUrl"
    Write-Log "配置订阅地址: $ConfigUrl"

    # --- 关闭系统代理
    Write-Log "关闭系统代理..."
    try {
        Invoke-WebRequest -Uri "http://127.0.0.1:33331/commands/scheme?param=clash://?enable_system_proxy=false" -ErrorAction SilentlyContinue
    } catch {
        Write-Log "关闭系统代理失败，继续执行..." "WARN"
    }
    
    # 清理现有进程和安装
    Write-Log "正在清理现有进程和安装..."
    # 杀掉相关进程
    $processNames = @(
        "clash-verge",
        "verge-mihomo",
        "clash-verge-service",
        "clash-meta",
        "clash-meta-alpha",
        "verge-mihomo-alpha"
    )
    foreach ($processName in $processNames) {
        $processes = Get-Process -Name $processName -ErrorAction SilentlyContinue
        if ($processes) {
            Write-Log "正在终止进程: $processName"
            $processes | Stop-Process -Force
            Start-Sleep -Seconds 2
        }
    }
    # 等待进程完全退出
    Write-Log "等待进程完全退出..."
    Start-Sleep -Seconds 5
    
    # --- 删除现有安装目录
    if (Test-Path $InstallPath) {
        Write-Log "删除现有安装目录: $InstallPath"
        Remove-Item $InstallPath -Recurse -Force -ErrorAction SilentlyContinue
    }

    # --- 删除缓存文件
    $userCachePath = Join-Path $env:APPDATA "io.github.clash-verge-rev.clash-verge-rev"
    if (Test-Path $userCachePath) {
        Write-Log "删除缓存目录: $userCachePath"
        Remove-Item $userCachePath -Recurse -Force -ErrorAction SilentlyContinue
    }

    # --- 准备下载压缩包
    Write-Log "正在下载更新包: $DownloadUrl"
    # 使用PowerShell原生方法获取临时目录
    $tempDir = $env:TEMP
    $zipFile = Join-Path $tempDir "aidc_clash.zip"
    # 清理临时文件
    if (Test-Path $zipFile) { Remove-Item $zipFile -Force }
    # 下载文件
    try {
        Write-Log "开始下载文件到: $zipFile"
        # 命令行里的进度条显示会占用大量 CPU 资源，十分影响下载的整体速度
        # 显示进度时，下载流程要起码 5 分钟；不显示进度时，下载流程只需要几十秒
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $DownloadUrl -OutFile $zipFile -TimeoutSec $TimeoutSeconds
        $ProgressPreference = 'Continue'
    } catch {
        Write-Log "下载失败: $($_.Exception.Message)" "ERROR"
        throw "下载失败"
    }
    Write-Log "下载完成: $zipFile"
    
    # --- 解压新版本
    Write-Log "正在解压新版本到: $InstallPath"
    # 创建安装目录
    New-Item -ItemType Directory -Path $InstallPath -Force | Out-Null
    # 使用PowerShell 5.0+的Expand-Archive命令解压
    try {
        # 关闭解压进度显示以提高性能
        $ProgressPreference = 'SilentlyContinue'
        Expand-Archive -Path $zipFile -DestinationPath $InstallPath -Force
        $ProgressPreference = 'Continue'
        Write-Log "解压完成"
    } catch {
        $ProgressPreference = 'Continue'  # 确保在异常情况下也恢复进度显示
        Write-Log "使用Expand-Archive失败，尝试使用Shell.Application" "WARN"
        try {
            # 备用方案：使用COM对象
            $shell = New-Object -ComObject Shell.Application
            $zip = $shell.NameSpace($zipFile)
            $destination = $shell.NameSpace($InstallPath)
            $destination.CopyHere($zip.Items(), 4)
            Write-Log "解压完成（使用Shell.Application）"
        } catch {
            Write-Log "解压失败: $($_.Exception.Message)" "ERROR"
            throw "解压失败"
        }
    }
    
    # --- 启动客户端
    $executablePath = Join-Path $InstallPath $ExecutableName
    if (Test-Path $executablePath) {
        Write-Log "启动客户端: $executablePath"
        Start-Process -FilePath $executablePath -ArgumentList "--ConfigUrl", "`"$ConfigUrl`""
        Write-Log "客户端启动成功"
    } else {
        throw "可执行文件不存在: $executablePath"
    }
    
    Write-Log "更新完成！"
    
} catch {
    Write-Log "更新失败: $($_.Exception.Message)" "ERROR"
    exit 1
} finally {
    # 清理临时文件
    Write-Log "清理临时文件"
    if (Test-Path $zipFile) { Remove-Item $zipFile -Force -ErrorAction SilentlyContinue}
    exit 0
}